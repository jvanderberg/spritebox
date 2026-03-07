use crate::network::VmnetConfig;
use crate::state::{Instance, ShareMount};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_CLOUD_USER: &str = "vibe";
const SSH_KEY_BASENAMES: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];
const GUEST_WORKSPACE_PATH: &str = "/workspace";
const DEFAULT_NOFILE_LIMIT: u64 = 65_536;

pub struct CloudInitOptions {
    pub enabled: bool,
    pub user: String,
    pub hostname: Option<String>,
    pub ssh_pubkey: Option<PathBuf>,
    pub init_script: Option<PathBuf>,
    pub shares: Vec<ShareMount>,
    pub network: Option<VmnetConfig>,
    pub verbose: bool,
}

pub struct PreparedCloudInit {
    pub image_path: PathBuf,
    pub user: String,
    pub hostname: String,
    pub ssh_pubkey_path: PathBuf,
    pub init_script_path: Option<PathBuf>,
}

pub fn default_cloud_user() -> String {
    env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLOUD_USER.to_string())
}

pub fn discover_ssh_public_key() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    let ssh_dir = PathBuf::from(home).join(".ssh");
    for basename in SSH_KEY_BASENAMES {
        let path = ssh_dir.join(format!("{basename}.pub"));
        if path.is_file() {
            return Some(path);
        }
    }
    for candidate in ["authorized_keys"] {
        let path = ssh_dir.join(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

pub fn discover_ssh_private_key() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    let ssh_dir = PathBuf::from(home).join(".ssh");
    for basename in SSH_KEY_BASENAMES {
        let private_path = ssh_dir.join(basename);
        let public_path = ssh_dir.join(format!("{basename}.pub"));
        if private_path.is_file() && public_path.is_file() {
            return Some(private_path);
        }
    }
    None
}

pub fn private_key_from_public(public_key_path: &Path) -> Option<PathBuf> {
    let path_str = public_key_path.to_string_lossy();
    if !path_str.ends_with(".pub") {
        return None;
    }

    let private_key = PathBuf::from(path_str.trim_end_matches(".pub"));
    if private_key.is_file() {
        Some(private_key)
    } else {
        None
    }
}

pub fn prepare(
    instance: &Instance,
    options: &CloudInitOptions,
) -> Result<Option<PreparedCloudInit>, String> {
    if !options.enabled {
        return Ok(None);
    }

    let ssh_pubkey_path = options
        .ssh_pubkey
        .clone()
        .or_else(discover_ssh_public_key)
        .ok_or_else(|| {
            "cloud-init requires an SSH public key; pass --ssh-pubkey or create ~/.ssh/id_ed25519.pub"
                .to_string()
        })?;
    if !ssh_pubkey_path.is_file() {
        return Err(format!(
            "SSH public key file {} does not exist",
            ssh_pubkey_path.display()
        ));
    }
    let ssh_pubkey = fs::read_to_string(&ssh_pubkey_path)
        .map_err(|err| {
            format!(
                "failed to read SSH public key {}: {err}",
                ssh_pubkey_path.display()
            )
        })?
        .trim()
        .to_string();
    if ssh_pubkey.is_empty() {
        return Err(format!(
            "SSH public key file {} is empty",
            ssh_pubkey_path.display()
        ));
    }

    let hostname = options
        .hostname
        .clone()
        .map(|value| sanitize_hostname(&value))
        .unwrap_or_else(|| default_hostname(instance));
    let host_uid = detect_host_uid();
    let cloud_init_dir = instance.instance_dir.join("cloud-init");
    let files_dir = cloud_init_dir.join("files");
    fs::create_dir_all(&files_dir).map_err(|err| err.to_string())?;

    let user_data = render_user_data(
        &options.user,
        &ssh_pubkey,
        &hostname,
        host_uid,
        &options.shares,
    );
    let init_script_path = match &options.init_script {
        Some(path) => {
            if !path.is_file() {
                return Err(format!("init script {} does not exist", path.display()));
            }
            let script = fs::read_to_string(path)
                .map_err(|err| format!("failed to read init script {}: {err}", path.display()))?;
            let rendered = render_user_data_with_init_script(
                &options.user,
                &ssh_pubkey,
                &hostname,
                host_uid,
                &options.shares,
                &script,
            );
            Some((path.clone(), rendered))
        }
        None => None,
    };
    let meta_data = render_meta_data(&instance.id, &hostname);
    let network_config = options.network.as_ref().map(render_network_config);

    fs::write(
        files_dir.join("user-data"),
        init_script_path
            .as_ref()
            .map(|(_, rendered)| rendered.as_str())
            .unwrap_or(&user_data),
    )
    .map_err(|err| err.to_string())?;
    fs::write(files_dir.join("meta-data"), meta_data).map_err(|err| err.to_string())?;
    if let Some(network_config) = network_config {
        fs::write(files_dir.join("network-config"), network_config)
            .map_err(|err| err.to_string())?;
    }

    let image_path = cloud_init_dir.join("seed.iso");
    if image_path.exists() {
        fs::remove_file(&image_path).map_err(|err| err.to_string())?;
    }
    create_seed_image(&files_dir, &image_path, options.verbose)?;

    Ok(Some(PreparedCloudInit {
        image_path,
        user: options.user.clone(),
        hostname,
        ssh_pubkey_path,
        init_script_path: init_script_path.map(|(path, _)| path),
    }))
}

fn create_seed_image(source_dir: &Path, image_path: &Path, verbose: bool) -> Result<(), String> {
    if command_exists("hdiutil") {
        let mut command = Command::new("hdiutil");
        command
            .arg("makehybrid")
            .arg("-iso")
            .arg("-joliet")
            .arg("-default-volume-name")
            .arg("CIDATA")
            .arg("-o")
            .arg(image_path)
            .arg(source_dir);
        quiet_command(&mut command, verbose);
        let status = command.status().map_err(|err| err.to_string())?;
        if status.success() {
            return Ok(());
        }
        return Err("hdiutil makehybrid failed while creating cloud-init seed image".to_string());
    }

    if command_exists("genisoimage") {
        let mut command = Command::new("genisoimage");
        command
            .arg("-output")
            .arg(image_path)
            .arg("-volid")
            .arg("CIDATA")
            .arg("-joliet")
            .arg("-rock")
            .arg(source_dir);
        quiet_command(&mut command, verbose);
        let status = command.status().map_err(|err| err.to_string())?;
        if status.success() {
            return Ok(());
        }
        return Err("genisoimage failed while creating cloud-init seed image".to_string());
    }

    if command_exists("mkisofs") {
        let mut command = Command::new("mkisofs");
        command
            .arg("-output")
            .arg(image_path)
            .arg("-volid")
            .arg("CIDATA")
            .arg("-joliet")
            .arg("-rock")
            .arg(source_dir);
        quiet_command(&mut command, verbose);
        let status = command.status().map_err(|err| err.to_string())?;
        if status.success() {
            return Ok(());
        }
        return Err("mkisofs failed while creating cloud-init seed image".to_string());
    }

    Err("no ISO creator found; install hdiutil, genisoimage, or mkisofs".to_string())
}

fn quiet_command(command: &mut Command, verbose: bool) {
    if !verbose {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
}

fn default_hostname(instance: &Instance) -> String {
    match (&instance.repo, &instance.branch) {
        (Some(repo), Some(branch)) => {
            let repo_name = repo_basename(repo);
            sanitize_hostname(&format!("{repo_name}-{branch}"))
        }
        _ => sanitize_hostname(&instance.base_image_name),
    }
}

fn repo_basename(repo: &str) -> String {
    let trimmed = repo.trim_end_matches('/');
    let last_segment = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    if last_segment.is_empty() {
        "vibebox".to_string()
    } else {
        last_segment.to_string()
    }
}

fn sanitize_hostname(input: &str) -> String {
    let mut hostname = String::new();
    let mut previous_dash = false;

    for ch in input.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };

        if normalized == '-' {
            if !previous_dash && !hostname.is_empty() {
                hostname.push('-');
            }
            previous_dash = true;
        } else {
            hostname.push(normalized);
            previous_dash = false;
        }

        if hostname.len() >= 63 {
            break;
        }
    }

    hostname.trim_matches('-').to_string()
}

fn render_user_data(
    user: &str,
    ssh_pubkey: &str,
    hostname: &str,
    host_uid: Option<u32>,
    shares: &[ShareMount],
) -> String {
    let uid_line = host_uid
        .map(|uid| format!("    uid: {uid}\n"))
        .unwrap_or_default();
    let mounts = render_mounts(shares);
    let bootcmd = render_bootcmd(shares);
    let runcmd = render_runcmd(user, shares, false);
    let write_files = render_common_write_files(user);
    format!(
        "#cloud-config\npreserve_hostname: false\nhostname: {hostname}\nfqdn: {hostname}\ngrowpart:\n  mode: auto\n  devices: [\"/\"]\nresize_rootfs: true\nusers:\n  - default\n  - name: {user}\n{uid_line}    sudo: ALL=(ALL) NOPASSWD:ALL\n    groups: adm, sudo\n    shell: /bin/bash\n    lock_passwd: true\n    ssh_authorized_keys:\n      - {ssh_pubkey}\nssh_pwauth: false\ndisable_root: true\npackage_update: false\npackage_upgrade: false\nwrite_files:\n{write_files}\nbootcmd:\n{bootcmd}\nmounts:\n{mounts}\nruncmd:\n{runcmd}\n"
    )
}

fn render_user_data_with_init_script(
    user: &str,
    ssh_pubkey: &str,
    hostname: &str,
    host_uid: Option<u32>,
    shares: &[ShareMount],
    init_script: &str,
) -> String {
    let uid_line = host_uid
        .map(|uid| format!("    uid: {uid}\n"))
        .unwrap_or_default();
    let run_init_script = r#"#!/bin/sh
set -eu
cd /workspace 2>/dev/null || cd "$HOME"
if [ ! -f /var/lib/vibebox/init.done ]; then
  /var/lib/vibebox/init.sh >>/var/log/vibebox-init.log 2>&1
  sudo touch /var/lib/vibebox/init.done
fi
"#;
    let mounts = render_mounts(shares);
    let bootcmd = render_bootcmd(shares);
    let runcmd = render_runcmd(user, shares, true);
    let common_write_files = render_common_write_files(user);

    format!(
        "#cloud-config\npreserve_hostname: false\nhostname: {hostname}\nfqdn: {hostname}\ngrowpart:\n  mode: auto\n  devices: [\"/\"]\nresize_rootfs: true\nusers:\n  - default\n  - name: {user}\n{uid_line}    sudo: ALL=(ALL) NOPASSWD:ALL\n    groups: adm, sudo\n    shell: /bin/bash\n    lock_passwd: true\n    ssh_authorized_keys:\n      - {ssh_pubkey}\nssh_pwauth: false\ndisable_root: true\npackage_update: false\npackage_upgrade: false\nwrite_files:\n{common_write_files}\n  - path: /var/lib/vibebox/init.sh\n    permissions: '0755'\n    owner: root:root\n    content: |\n{init_script_content}\n  - path: /var/lib/vibebox/run-init.sh\n    permissions: '0755'\n    owner: root:root\n    content: |\n{run_init_script_content}\nbootcmd:\n{bootcmd}\nmounts:\n{mounts}\nruncmd:\n{runcmd}\n",
        init_script_content = indent_for_yaml(init_script, 6),
        run_init_script_content = indent_for_yaml(run_init_script, 6),
    )
}

fn render_mounts(shares: &[ShareMount]) -> String {
    let mut lines = vec![
        "  - [ workspace, /workspace, virtiofs, \"defaults,nofail\", \"0\", \"0\" ]".to_string(),
    ];
    for (index, share) in shares.iter().enumerate() {
        lines.push(format!(
            "  - [ {}, \"{}\", virtiofs, \"defaults,nofail\", \"0\", \"0\" ]",
            share_tag(index),
            yaml_escape(&share.guest_path.display().to_string())
        ));
    }
    lines.join("\n")
}

fn render_bootcmd(shares: &[ShareMount]) -> String {
    let mut lines = vec![
        "  - [ sh, -lc, \"if [ -e /workspace ] && [ ! -d /workspace ]; then rm -f /workspace; fi; mkdir -p /workspace\" ]".to_string(),
    ];
    for (index, share) in shares.iter().enumerate() {
        let _ = index;
        lines.push(format!(
            "  - [ sh, -lc, \"if [ -e {path} ] && [ ! -d {path} ]; then rm -f {path}; fi; mkdir -p {path}\" ]",
            path = shell_quote(&share.guest_path.display().to_string())
        ));
    }
    lines.join("\n")
}

fn render_runcmd(user: &str, shares: &[ShareMount], includes_init_script: bool) -> String {
    let mut lines = vec![
        "  - [ sh, -lc, \"if ! dpkg -s avahi-daemon >/dev/null 2>&1; then apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y avahi-daemon; fi\" ]".to_string(),
        "  - [ systemctl, enable, --now, avahi-daemon ]".to_string(),
        "  - [ sh, -lc, \"if [ -e /workspace ] && [ ! -d /workspace ]; then rm -f /workspace; fi; mkdir -p /workspace; mountpoint -q /workspace || mount /workspace || true\" ]".to_string(),
    ];
    for (index, share) in shares.iter().enumerate() {
        let _ = index;
        lines.push(format!(
            "  - [ sh, -lc, \"if [ -e {path} ] && [ ! -d {path} ]; then rm -f {path}; fi; mkdir -p {path}; mountpoint -q {path} || mount {path} || true\" ]",
            path = shell_quote(&share.guest_path.display().to_string())
        ));
    }
    if includes_init_script {
        lines.push(format!(
            "  - [ sh, -lc, \"mkdir -p /var/lib/vibebox && touch /var/log/vibebox-init.log && chown {user}:{user} /var/log/vibebox-init.log\" ]"
        ));
        lines.push(format!(
            "  - [ su, -, {user}, -c, /var/lib/vibebox/run-init.sh ]"
        ));
    }
    lines.push(format!(
        "  - [ ln, -sfn, {}, /home/{user}/workspace ]",
        GUEST_WORKSPACE_PATH
    ));
    lines.join("\n")
}

fn render_common_write_files(user: &str) -> String {
    format!(
        "  - path: /etc/security/limits.d/99-vibebox.conf\n    permissions: '0644'\n    owner: root:root\n    content: |\n{}\n",
        indent_for_yaml(&render_nofile_limits(user), 6)
    )
}

fn render_nofile_limits(user: &str) -> String {
    format!(
        "* soft nofile {limit}\n* hard nofile {limit}\nroot soft nofile {limit}\nroot hard nofile {limit}\n{user} soft nofile {limit}\n{user} hard nofile {limit}\n",
        limit = DEFAULT_NOFILE_LIMIT
    )
}

fn share_tag(index: usize) -> String {
    format!("share{index}")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn yaml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn indent_for_yaml(content: &str, spaces: usize) -> String {
    let prefix = " ".repeat(spaces);
    let mut lines = content
        .lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>();
    if content.ends_with('\n') {
        lines.push(prefix);
    }
    lines.join("\n")
}

fn detect_host_uid() -> Option<u32> {
    let output = Command::new("id").arg("-u").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

fn render_meta_data(instance_id: &str, hostname: &str) -> String {
    format!("instance-id: {instance_id}\nlocal-hostname: {hostname}\n")
}

fn render_network_config(network: &VmnetConfig) -> String {
    let dns_addresses = network
        .dns_servers
        .iter()
        .map(|server| format!("        - {server}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "version: 2\nethernets:\n  ls0:\n    match:\n      macaddress: \"{mac}\"\n    dhcp4: false\n    addresses:\n      - {guest}/{prefix}\n    routes:\n      - to: default\n        via: {gateway}\n    nameservers:\n      addresses:\n{dns_addresses}\n",
        mac = network.mac_address,
        guest = network.guest_ip,
        prefix = network.prefix_len,
        gateway = network.gateway_ip
    )
}

fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(name).exists()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        default_cloud_user, default_hostname, render_meta_data, render_network_config,
        render_user_data, render_user_data_with_init_script, repo_basename, sanitize_hostname,
    };
    use crate::network::VmnetConfig;
    use crate::ports::PortMapping;
    use crate::state::{Instance, ShareMount};
    use std::env;
    use std::path::PathBuf;

    #[test]
    fn user_data_contains_user_and_key() {
        let rendered =
            render_user_data("vibe", "ssh-ed25519 AAAATEST", "repo-main", Some(501), &[]);
        assert!(rendered.contains("preserve_hostname: false"));
        assert!(rendered.contains("hostname: repo-main"));
        assert!(rendered.contains("growpart:"));
        assert!(rendered.contains("resize_rootfs: true"));
        assert!(rendered.contains("name: vibe"));
        assert!(rendered.contains("uid: 501"));
        assert!(rendered.contains("ssh-ed25519 AAAATEST"));
        assert!(rendered.contains("rm -f /workspace"));
        assert!(rendered.contains("apt-get install -y avahi-daemon"));
        assert!(rendered.contains("systemctl, enable, --now, avahi-daemon"));
        assert!(rendered.contains("path: /etc/security/limits.d/99-vibebox.conf"));
        assert!(rendered.contains("* soft nofile 65536"));
        assert!(rendered.contains("workspace, /workspace, virtiofs"));
        assert!(rendered.contains("ln, -sfn, /workspace, /home/vibe/workspace"));
    }

    #[test]
    fn meta_data_contains_instance_and_hostname() {
        let rendered = render_meta_data("abc123", "vibebox-host");
        assert!(rendered.contains("instance-id: abc123"));
        assert!(rendered.contains("local-hostname: vibebox-host"));
    }

    #[test]
    fn init_script_is_written_and_run_once() {
        let rendered = render_user_data_with_init_script(
            "vibe",
            "ssh-ed25519 AAAATEST",
            "repo-main",
            Some(501),
            &[ShareMount {
                host_path: PathBuf::from("/tmp/host-share"),
                guest_path: PathBuf::from("/mnt/share"),
            }],
            "#!/bin/sh\necho init\n",
        );
        assert!(rendered.contains("write_files:"));
        assert!(rendered.contains("path: /var/lib/vibebox/init.sh"));
        assert!(rendered.contains("path: /var/lib/vibebox/run-init.sh"));
        assert!(rendered.contains("su, -, vibe, -c, /var/lib/vibebox/run-init.sh"));
        assert!(rendered.contains("touch /var/log/vibebox-init.log"));
        assert!(rendered.contains("sudo touch /var/lib/vibebox/init.done"));
        assert!(rendered.contains("apt-get install -y avahi-daemon"));
        assert!(rendered.contains("systemctl, enable, --now, avahi-daemon"));
        assert!(rendered.contains("path: /etc/security/limits.d/99-vibebox.conf"));
        assert!(rendered.contains("[ share0, \"/mnt/share\", virtiofs"));
    }

    #[test]
    fn hostname_is_sanitized() {
        let hostname = sanitize_hostname("Feature/X for Repo!!!");
        assert_eq!(hostname, "feature-x-for-repo");
    }

    #[test]
    fn repo_basename_strips_transport_and_git_suffix() {
        assert_eq!(
            repo_basename("git@github.com:jvanderberg/kicad_jlcimport.git"),
            "kicad_jlcimport"
        );
        assert_eq!(
            repo_basename("https://github.com/jvanderberg/kicad_jlcimport/"),
            "kicad_jlcimport"
        );
    }

    #[test]
    fn default_hostname_uses_repo_and_branch() {
        let instance = Instance {
            id: "ignored-instance-id".to_string(),
            repo: Some("git@github.com:jvanderberg/kicad_jlcimport.git".to_string()),
            branch: Some("main".to_string()),
            instance_dir: PathBuf::from("/tmp/instance"),
            base_image_id: "ubuntu".to_string(),
            base_image_name: "ubuntu".to_string(),
            base_image_path: PathBuf::from("/tmp/base.img"),
            checkout_dir: PathBuf::from("/tmp/checkout"),
            rootfs_path: PathBuf::from("/tmp/branch.img"),
            rootfs_mb: 1024,
            host_port_base: 28000,
            ports: vec![PortMapping {
                host: 28000,
                guest: 22,
            }],
            shares: Vec::new(),
            guest_env: Vec::new(),
            created_unix: 0,
        };

        assert_eq!(default_hostname(&instance), "kicad-jlcimport-main");
    }

    #[test]
    fn default_hostname_uses_base_name_for_base_only_instances() {
        let instance = Instance {
            id: "ignored-instance-id".to_string(),
            repo: None,
            branch: None,
            instance_dir: PathBuf::from("/tmp/instance"),
            base_image_id: "ubuntu".to_string(),
            base_image_name: "ubuntu".to_string(),
            base_image_path: PathBuf::from("/tmp/base.img"),
            checkout_dir: PathBuf::from("/tmp/checkout"),
            rootfs_path: PathBuf::from("/tmp/branch.img"),
            rootfs_mb: 1024,
            host_port_base: 28000,
            ports: vec![PortMapping {
                host: 28000,
                guest: 22,
            }],
            shares: Vec::new(),
            guest_env: Vec::new(),
            created_unix: 0,
        };

        assert_eq!(default_hostname(&instance), "ubuntu");
    }

    #[test]
    fn default_cloud_user_prefers_host_user() {
        unsafe {
            env::set_var("USER", "host-user");
        }
        assert_eq!(default_cloud_user(), "host-user");
        unsafe {
            env::remove_var("USER");
        }
    }

    #[test]
    fn network_config_contains_static_ip() {
        let rendered = render_network_config(&VmnetConfig {
            client_path: PathBuf::from("/opt/vmnet-helper/bin/vmnet-client"),
            interface_id: "uuid".to_string(),
            mac_address: "52:54:89:ab:cd:ef".to_string(),
            guest_ip: "192.168.105.10".to_string(),
            gateway_ip: "192.168.105.1".to_string(),
            prefix_len: 24,
            dhcp_start: "192.168.105.1".to_string(),
            dhcp_end: "192.168.105.254".to_string(),
            dns_servers: vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()],
        });
        assert!(rendered.contains("macaddress: \"52:54:89:ab:cd:ef\""));
        assert!(rendered.contains("- 192.168.105.10/24"));
        assert!(rendered.contains("via: 192.168.105.1"));
    }
}
