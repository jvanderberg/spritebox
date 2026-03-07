use crate::network::VmnetConfig;
use crate::ports::PortMapping;
use crate::state::{GuestEnvVar, Instance, ShareMount};
use crossterm::cursor::{Hide, MoveTo, Show, position};
use crossterm::execute;
use crossterm::terminal::{Clear as TermClear, ClearType};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use std::env;
use std::fs;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::{io::Error as IoError, os::unix::process::CommandExt};

const DEFAULT_CPUS: u8 = 4;
const DEFAULT_MEMORY_MIB: u32 = 8192;
const DEFAULT_WORKSPACE_TAG: &str = "workspace";
const DEFAULT_SSH_READY_TIMEOUT_SECS: u64 = 300;
const QUIET_LOADING_FRAME_MILLIS: u64 = 16;
const SSH_PROBE_INTERVAL_MILLIS: u64 = 1000;
const GUEST_MOUNT_ROOT: &str = "/var/lib/vibebox/mounts";
const GUEST_WORKSPACE_PATH: &str = "/workspace";
const GUEST_NOFILE_LIMIT: u64 = 65_536;
const HOST_NOFILE_LIMIT: u64 = 65_536;

pub enum LaunchMode {
    External(PathBuf),
    Krunkit,
    Shell,
}

pub struct RuntimePlan {
    pub mode: LaunchMode,
}

pub struct LaunchConfig {
    pub require_vm: bool,
    pub cpus: u8,
    pub memory_mib: u32,
    pub cloud_init_image: Option<PathBuf>,
    pub cloud_init_user: Option<String>,
    pub hostname: Option<String>,
    pub ssh_pubkey_path: Option<PathBuf>,
    pub ssh_private_key_path: Option<PathBuf>,
    pub init_script_path: Option<PathBuf>,
    pub shares: Vec<ShareMount>,
    pub guest_env: Vec<GuestEnvVar>,
    pub verbose: bool,
    pub vmnet: Option<VmnetConfig>,
}

pub fn default_cpus() -> u8 {
    DEFAULT_CPUS
}

pub fn default_memory_mib() -> u32 {
    DEFAULT_MEMORY_MIB
}

pub fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(name).exists()))
        .unwrap_or(false)
}

pub fn stop_instance_vm(instance_dir: &Path) -> Result<(), String> {
    for pid in instance_process_pids(instance_dir)? {
        terminate_pid(pid)?;
    }
    stop_stale_vm(&instance_dir.join("runtime").join("krunkit.pid"))
}

pub fn is_instance_vm_running(instance_dir: &Path) -> Result<bool, String> {
    instance_has_running_vm(instance_dir)
}

pub fn resolve_runtime(force_shell: bool) -> RuntimePlan {
    if force_shell {
        return RuntimePlan {
            mode: LaunchMode::Shell,
        };
    }

    if let Some(launcher) = env::var_os("VIBEBOX_VM_LAUNCHER").map(PathBuf::from) {
        return RuntimePlan {
            mode: LaunchMode::External(launcher),
        };
    }

    if command_exists("krunkit") {
        return RuntimePlan {
            mode: LaunchMode::Krunkit,
        };
    }

    RuntimePlan {
        mode: LaunchMode::Shell,
    }
}

pub fn launch(
    plan: RuntimePlan,
    instance: &Instance,
    config: &LaunchConfig,
) -> Result<i32, String> {
    match plan.mode {
        LaunchMode::Shell => {
            if config.require_vm {
                return Err(
                    "vm runtime requested, but neither VIBEBOX_VM_LAUNCHER nor krunkit is available"
                        .to_string(),
                );
            }
            launch_shell(instance)
        }
        LaunchMode::External(launcher) => launch_external(&launcher, instance, config),
        LaunchMode::Krunkit => launch_krunkit(instance, config),
    }
}

pub fn launch_summary(instance: &Instance, config: &LaunchConfig) -> Vec<String> {
    let mut lines = instance.summary_lines();
    lines.push(format!("vm_cpus: {}", config.cpus));
    lines.push(format!("vm_memory_mib: {}", config.memory_mib));
    if let Some(path) = &config.cloud_init_image {
        lines.push(format!("cloud_init: {}", path.display()));
    }
    if let Some(user) = &config.cloud_init_user {
        lines.push(format!("guest_user: {}", user));
    }
    if let Some(hostname) = &config.hostname {
        lines.push(format!("guest_hostname: {}", hostname));
    }
    if let Some(path) = &config.ssh_pubkey_path {
        lines.push(format!("ssh_pubkey: {}", path.display()));
    }
    if let Some(path) = &config.ssh_private_key_path {
        lines.push(format!("ssh_private_key: {}", path.display()));
    }
    if let Some(path) = &config.init_script_path {
        lines.push(format!("init_script: {}", path.display()));
    }
    if let Some(vmnet) = &config.vmnet {
        lines.extend(vmnet.summary_lines());
    }

    if let Some(launcher) = env::var_os("VIBEBOX_VM_LAUNCHER") {
        lines.push(format!(
            "vm_launcher: external {}",
            PathBuf::from(launcher).display()
        ));
    } else if command_exists("krunkit") {
        if config.vmnet.is_some() {
            lines.push("vm_launcher: builtin krunkit via vmnet-client".to_string());
            lines.push("network: guest IP + .local mDNS hostname".to_string());
        } else {
            lines.push("vm_launcher: builtin krunkit without host networking".to_string());
        }
    } else {
        lines.push(if config.require_vm {
            "vm_launcher: unavailable".to_string()
        } else {
            "vm_launcher: local shell".to_string()
        });
    }

    if config.cloud_init_image.is_some() {
        lines.push("guest_workspace: auto-mounted at /workspace via cloud-init".to_string());
    } else {
        lines.push(format!(
            "guest_workspace: mount -t virtiofs {} /workspace",
            DEFAULT_WORKSPACE_TAG
        ));
    }
    lines
}

fn launch_shell(instance: &Instance) -> Result<i32, String> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let status = Command::new(shell)
        .current_dir(&instance.checkout_dir)
        .env("VIBEBOX_INSTANCE", &instance.id)
        .env("VIBEBOX_CHECKOUT", &instance.checkout_dir)
        .env("VIBEBOX_BASE_IMAGE", &instance.base_image_path)
        .env("VIBEBOX_BASE_IMAGE_ID", &instance.base_image_id)
        .env("VIBEBOX_ROOTFS", &instance.rootfs_path)
        .env("VIBEBOX_REPO", instance.repo.as_deref().unwrap_or_default())
        .env(
            "VIBEBOX_BRANCH",
            instance.branch.as_deref().unwrap_or_default(),
        )
        .env("VIBEBOX_SHARES", encode_env_shares(&instance.shares))
        .env(
            "VIBEBOX_GUEST_ENV",
            encode_guest_env_names(&instance.guest_env),
        )
        .env("VIBEBOX_PORTS", encode_env_ports(&instance.ports))
        .status()
        .map_err(|err| err.to_string())?;
    Ok(status.code().unwrap_or_default())
}

fn launch_external(
    launcher: &Path,
    instance: &Instance,
    config: &LaunchConfig,
) -> Result<i32, String> {
    let mut command = Command::new(launcher);
    command
        .current_dir(&instance.checkout_dir)
        .env("VIBEBOX_INSTANCE", &instance.id)
        .env("VIBEBOX_REPO", instance.repo.as_deref().unwrap_or_default())
        .env(
            "VIBEBOX_BRANCH",
            instance.branch.as_deref().unwrap_or_default(),
        )
        .env("VIBEBOX_CHECKOUT", &instance.checkout_dir)
        .env("VIBEBOX_BASE_IMAGE", &instance.base_image_path)
        .env("VIBEBOX_BASE_IMAGE_ID", &instance.base_image_id)
        .env("VIBEBOX_ROOTFS", &instance.rootfs_path)
        .env("VIBEBOX_ROOTFS_MB", instance.rootfs_mb.to_string())
        .env("VIBEBOX_CPUS", config.cpus.to_string())
        .env("VIBEBOX_MEMORY_MIB", config.memory_mib.to_string())
        .env(
            "VIBEBOX_CLOUD_INIT_IMAGE",
            config
                .cloud_init_image
                .as_ref()
                .map(|path| path.as_os_str())
                .unwrap_or_default(),
        )
        .env(
            "VIBEBOX_CLOUD_INIT_USER",
            config.cloud_init_user.as_deref().unwrap_or_default(),
        )
        .env(
            "VIBEBOX_HOSTNAME",
            config.hostname.as_deref().unwrap_or_default(),
        )
        .env("VIBEBOX_SHARES", encode_env_shares(&config.shares))
        .env(
            "VIBEBOX_GUEST_ENV",
            encode_guest_env_names(&config.guest_env),
        )
        .env("VIBEBOX_PORTS", encode_env_ports(&instance.ports));

    if let Some(vmnet) = &config.vmnet {
        command
            .env("VIBEBOX_GUEST_IP", &vmnet.guest_ip)
            .env("VIBEBOX_GUEST_GATEWAY", &vmnet.gateway_ip)
            .env("VIBEBOX_GUEST_MAC", &vmnet.mac_address)
            .env("VIBEBOX_INTERFACE_ID", &vmnet.interface_id);
    }

    if let Some(path) = &config.ssh_private_key_path {
        command.env("VIBEBOX_SSH_PRIVATE_KEY", path);
    }

    let status = command.status().map_err(|err| err.to_string())?;
    Ok(status.code().unwrap_or_default())
}

fn launch_krunkit(instance: &Instance, config: &LaunchConfig) -> Result<i32, String> {
    let vmnet = config
        .vmnet
        .as_ref()
        .ok_or_else(|| "built-in VM networking requires vmnet-client".to_string())?;
    let ssh_user = config
        .cloud_init_user
        .as_deref()
        .ok_or_else(|| "SSH guest user is required for built-in VM launch".to_string())?;
    let ssh_private_key = config
        .ssh_private_key_path
        .as_ref()
        .ok_or_else(|| "SSH private key is required for built-in VM launch".to_string())?;

    let runtime_dir = instance.instance_dir.join("runtime");
    fs::create_dir_all(&runtime_dir).map_err(|err| err.to_string())?;

    let console_log = runtime_dir.join("console.log");
    let krunkit_log = runtime_dir.join("krunkit.log");
    let pidfile = runtime_dir.join("krunkit.pid");
    let known_hosts = runtime_dir.join("known_hosts");
    let shares_path = runtime_dir.join("shares");

    if instance_has_running_vm(&instance.instance_dir)? {
        if running_vm_matches_shares(&shares_path, &config.shares)? {
            if config.verbose {
                eprintln!("reconnecting to running vm for {}", instance.id);
            }
            if wait_for_running_vm_ssh(
                &instance.instance_dir,
                ssh_user,
                ssh_private_key,
                &known_hosts,
                vmnet,
                config.verbose,
            )? {
                return connect_ssh(
                    instance,
                    ssh_user,
                    ssh_private_key,
                    &known_hosts,
                    vmnet,
                    &config.shares,
                    &config.guest_env,
                    config.verbose,
                );
            }

            if config.verbose {
                eprintln!(
                    "running vm for {} did not accept ssh within timeout, restarting it",
                    instance.id
                );
            }
        } else {
            if config.verbose {
                eprintln!(
                    "share configuration changed for {}, restarting vm",
                    instance.id
                );
            }
        }
    }

    // Relaunches can leave vmnet-client orphaned even after krunkit exits.
    // Clean up any instance-owned runtime processes before reusing the same interface id.
    stop_instance_vm(&instance.instance_dir)?;
    stop_vmnet_helper_interface(&vmnet.interface_id)?;
    remove_if_exists(&known_hosts)?;
    write_runtime_shares(&shares_path, &config.shares)?;

    let stdout_file = File::create(&krunkit_log).map_err(|err| err.to_string())?;
    let stderr_file = stdout_file.try_clone().map_err(|err| err.to_string())?;

    let mut command = Command::new(&vmnet.client_path);
    command
        .arg("--interface-id")
        .arg(&vmnet.interface_id)
        .arg("--operation-mode")
        .arg("shared")
        .arg("--start-address")
        .arg(&vmnet.dhcp_start)
        .arg("--end-address")
        .arg(&vmnet.dhcp_end)
        .arg("--subnet-mask")
        .arg("255.255.255.0")
        .arg("--")
        .arg("krunkit")
        .arg("--cpus")
        .arg(config.cpus.to_string())
        .arg("--memory")
        .arg(config.memory_mib.to_string())
        .arg("--pidfile")
        .arg(&pidfile)
        .arg("--device")
        .arg(format!(
            "virtio-blk,path={},format=raw",
            instance.rootfs_path.display()
        ));

    if let Some(seed_image) = &config.cloud_init_image {
        command.arg("--device").arg(format!(
            "virtio-blk,path={},format=raw",
            seed_image.display()
        ));
    }

    command
        .arg("--device")
        .arg(krunkit_network_device(&vmnet.mac_address))
        .arg("--device")
        .arg(format!(
            "virtio-fs,sharedDir={},mountTag={}",
            instance.checkout_dir.display(),
            DEFAULT_WORKSPACE_TAG
        ));
    for (index, share) in config.shares.iter().enumerate() {
        command.arg("--device").arg(format!(
            "virtio-fs,sharedDir={},mountTag={}",
            share.host_path.display(),
            share_tag(index)
        ));
    }
    command
        .arg("--device")
        .arg(format!(
            "virtio-serial,logFilePath={}",
            console_log.display()
        ))
        .current_dir(&instance.checkout_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    apply_child_nofile_limit(&mut command, HOST_NOFILE_LIMIT)?;

    let mut child = command.spawn().map_err(|err| err.to_string())?;
    let ssh_ready = wait_for_ssh(
        &mut child,
        ssh_user,
        ssh_private_key,
        &known_hosts,
        vmnet,
        &krunkit_log,
        config.verbose,
    )?;
    if !ssh_ready {
        stop_child(&mut child)?;
        return Err(format!(
            "timed out waiting for SSH on {}; see {} and {}",
            vmnet.guest_ip,
            console_log.display(),
            krunkit_log.display()
        ));
    }

    connect_ssh(
        instance,
        ssh_user,
        ssh_private_key,
        &known_hosts,
        vmnet,
        &config.shares,
        &config.guest_env,
        config.verbose,
    )
}

fn wait_for_ssh(
    child: &mut Child,
    ssh_user: &str,
    ssh_private_key: &Path,
    known_hosts: &Path,
    vmnet: &VmnetConfig,
    krunkit_log: &Path,
    verbose: bool,
) -> Result<bool, String> {
    let start = Instant::now();
    let mut progress = LaunchProgress::new(&vmnet.guest_ip, verbose);
    let mut last_probe = None;
    while start.elapsed() < ssh_ready_timeout() {
        progress.tick(start.elapsed())?;
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            progress.finish()?;
            return Err(format!(
                "vm process exited early with code {:?}",
                status.code()
            ));
        }

        if let Some(error) = detect_runtime_failure(krunkit_log)? {
            progress.finish()?;
            return Err(error);
        }

        if should_probe_ssh(start.elapsed(), last_probe) {
            last_probe = Some(start.elapsed());
            if probe_ssh(ssh_user, ssh_private_key, known_hosts, vmnet)? {
                progress.finish()?;
                return Ok(true);
            }
        }

        thread::sleep(Duration::from_millis(QUIET_LOADING_FRAME_MILLIS));
    }

    progress.finish()?;
    Ok(false)
}

fn wait_for_running_vm_ssh(
    instance_dir: &Path,
    ssh_user: &str,
    ssh_private_key: &Path,
    known_hosts: &Path,
    vmnet: &VmnetConfig,
    verbose: bool,
) -> Result<bool, String> {
    let start = Instant::now();
    let mut progress = LaunchProgress::new(&vmnet.guest_ip, verbose);
    let mut last_probe = None;
    while start.elapsed() < ssh_ready_timeout() {
        progress.tick(start.elapsed())?;
        if should_probe_ssh(start.elapsed(), last_probe) {
            last_probe = Some(start.elapsed());
            if probe_ssh(ssh_user, ssh_private_key, known_hosts, vmnet)? {
                progress.finish()?;
                return Ok(true);
            }
        }

        if !instance_has_running_vm(instance_dir)? {
            progress.finish()?;
            return Ok(false);
        }

        thread::sleep(Duration::from_millis(QUIET_LOADING_FRAME_MILLIS));
    }

    progress.finish()?;
    Ok(false)
}

fn detect_runtime_failure(krunkit_log: &Path) -> Result<Option<String>, String> {
    if !krunkit_log.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(krunkit_log).map_err(|err| err.to_string())?;
    if contents.contains("vmnet_start_interface: VMNET_FAILURE") {
        return Ok(Some(format!(
            "vmnet-helper failed to create the shared network interface; see {}",
            krunkit_log.display()
        )));
    }

    Ok(None)
}

fn connect_ssh(
    _instance: &Instance,
    ssh_user: &str,
    ssh_private_key: &Path,
    known_hosts: &Path,
    vmnet: &VmnetConfig,
    shares: &[ShareMount],
    guest_env: &[GuestEnvVar],
    verbose: bool,
) -> Result<i32, String> {
    let mut command = ssh_base_command(ssh_user, ssh_private_key, known_hosts, vmnet, true);
    command
        .arg("-tt")
        .arg(format!("{ssh_user}@{}", vmnet.guest_ip))
        .arg(guest_shell_command(shares, guest_env, verbose));

    let status = command.status().map_err(|err| err.to_string())?;
    Ok(status.code().unwrap_or_default())
}

fn krunkit_network_device(mac_address: &str) -> String {
    format!("virtio-net,type=unixgram,fd=4,mac={mac_address}")
}

#[cfg(unix)]
fn apply_child_nofile_limit(command: &mut Command, limit: u64) -> Result<(), String> {
    let limit =
        libc::rlim_t::try_from(limit).map_err(|_| format!("invalid nofile limit {limit}"))?;
    unsafe {
        command.pre_exec(move || set_current_process_nofile_limit(limit));
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_child_nofile_limit(_command: &mut Command, _limit: u64) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn set_current_process_nofile_limit(limit: libc::rlim_t) -> Result<(), IoError> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let get_result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut current) };
    if get_result != 0 {
        return Err(IoError::last_os_error());
    }

    let desired_soft = limit.min(current.rlim_max);
    let desired_hard = current.rlim_max.max(limit);
    let updated = libc::rlimit {
        rlim_cur: desired_soft,
        rlim_max: desired_hard,
    };
    let set_result = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &updated) };
    if set_result != 0 {
        return Err(IoError::last_os_error());
    }

    Ok(())
}

fn workspace_backing_mount_path() -> &'static str {
    "/var/lib/vibebox/mounts/workspace"
}

fn share_backing_mount_path(index: usize) -> String {
    format!("{GUEST_MOUNT_ROOT}/{}", share_tag(index))
}

fn bind_mount_command(backing_path: &str, guest_path: &str, mount_command: &str) -> String {
    let backing = shell_quote(backing_path);
    let guest = shell_quote(guest_path);
    format!(
        " if [ -e {backing} ] && [ ! -d {backing} ]; then sudo rm -f {backing}; fi; sudo mkdir -p {backing}; if [ -e {guest} ] && [ ! -d {guest} ]; then sudo rm -f {guest}; fi; sudo mkdir -p {guest}; if mountpoint -q {guest} && [ \"$(findmnt -n -o FSTYPE {guest} 2>/dev/null || true)\" = \"virtiofs\" ]; then sudo umount {guest} >/dev/null 2>&1 || true; fi; if ! mountpoint -q {backing}; then {mount_command} fi; if ! mountpoint -q {guest}; then sudo mount --bind {backing} {guest} >/dev/null 2>&1 || true; fi; sudo mount --make-private {guest} >/dev/null 2>&1 || true;",
    )
}

fn guest_shell_command(shares: &[ShareMount], guest_env: &[GuestEnvVar], verbose: bool) -> String {
    let mut share_mounts = String::new();
    for (index, share) in shares.iter().enumerate() {
        let backing_path = share_backing_mount_path(index);
        let guest_path = share.guest_path.display().to_string();
        let mount_command = format!(
            "sudo mount -t virtiofs {} {} >/dev/null 2>&1 || true;",
            share_tag(index),
            shell_quote(&backing_path)
        );
        share_mounts.push_str(&bind_mount_command(
            &backing_path,
            &guest_path,
            &mount_command,
        ));
    }
    let guest_env_exports = guest_env
        .iter()
        .map(|var| format!(" export {}={};", var.name, shell_quote(&var.value)))
        .collect::<String>();
    let git_identity_sync = " if command -v git >/dev/null 2>&1; then if [ -n \"${GIT_AUTHOR_NAME:-}\" ]; then git config --global user.name \"$GIT_AUTHOR_NAME\" >/dev/null 2>&1 || true; fi; if [ -n \"${GIT_AUTHOR_EMAIL:-}\" ]; then git config --global user.email \"$GIT_AUTHOR_EMAIL\" >/dev/null 2>&1 || true; fi; fi;";

    let cloud_init_wait = if verbose {
        "if command -v cloud-init >/dev/null 2>&1; then CLOUD_INIT_STATUS=\"$(sudo cloud-init status 2>/dev/null || true)\"; if ! printf \"%s\" \"$CLOUD_INIT_STATUS\" | grep -q \"status: done\"; then echo \"waiting for cloud-init/bootstrap...\"; if sudo test -e /var/log/vibebox-init.log; then sudo sh -lc \"tail -n +1 -F /var/log/vibebox-init.log\" & TAIL_PID=$!; fi; sudo cloud-init status --wait >/dev/null 2>&1 || true; if [ -n \"${TAIL_PID:-}\" ]; then kill \"$TAIL_PID\" 2>/dev/null || true; wait \"$TAIL_PID\" 2>/dev/null || true; fi; echo \"cloud-init/bootstrap complete\"; fi; if sudo test -f /var/lib/vibebox/init.done && sudo test -f /var/log/vibebox-init.log && ! sudo test -f /var/lib/vibebox/init-log-shown; then echo \"bootstrap log:\"; sudo cat /var/log/vibebox-init.log; sudo touch /var/lib/vibebox/init-log-shown; fi; fi;"
    } else {
        "if command -v cloud-init >/dev/null 2>&1; then sudo cloud-init status --wait >/dev/null 2>&1 || true; fi;"
    };

    let body = format!(
        "{cloud_init_wait} ROOT_DEV=\"$(findmnt -n -o SOURCE / 2>/dev/null || true)\"; ROOT_FS=\"$(findmnt -n -o FSTYPE / 2>/dev/null || true)\"; if command -v growpart >/dev/null 2>&1 && [ -n \"$ROOT_DEV\" ]; then PARENT_DEV=\"/dev/$(lsblk -no PKNAME \"$ROOT_DEV\" 2>/dev/null || true)\"; PART_NUM=\"$(lsblk -no PARTN \"$ROOT_DEV\" 2>/dev/null || true)\"; if [ -n \"$PARENT_DEV\" ] && [ -n \"$PART_NUM\" ]; then sudo growpart \"$PARENT_DEV\" \"$PART_NUM\" >/dev/null 2>&1 || true; fi; fi; if [ -n \"$ROOT_DEV\" ]; then case \"$ROOT_FS\" in ext2|ext3|ext4) if command -v resize2fs >/dev/null 2>&1; then sudo resize2fs \"$ROOT_DEV\" >/dev/null 2>&1 || true; fi ;; xfs) if command -v xfs_growfs >/dev/null 2>&1; then sudo xfs_growfs / >/dev/null 2>&1 || true; fi ;; esac; fi; ulimit -n {nofile_limit} >/dev/null 2>&1 || true;{workspace_mount}{share_mounts}{guest_env_exports}{git_identity_sync} ln -sfn {workspace_path} \"$HOME/workspace\"; cd {workspace_path} 2>/dev/null || cd \"$HOME\"; exec /bin/bash -l",
        workspace_mount = bind_mount_command(
            workspace_backing_mount_path(),
            GUEST_WORKSPACE_PATH,
            &format!(
                "sudo mount -t virtiofs {} {} >/dev/null 2>&1 || true;",
                DEFAULT_WORKSPACE_TAG,
                shell_quote(workspace_backing_mount_path())
            )
        ),
        nofile_limit = GUEST_NOFILE_LIMIT,
        workspace_path = GUEST_WORKSPACE_PATH,
    );

    format!("bash -lc {}", shell_quote(&body))
}

fn ssh_base_command(
    ssh_user: &str,
    ssh_private_key: &Path,
    known_hosts: &Path,
    vmnet: &VmnetConfig,
    forward_agent: bool,
) -> Command {
    let mut command = Command::new("ssh");
    let _ = ssh_user;
    let _ = vmnet;
    command
        .arg("-i")
        .arg(ssh_private_key)
        .arg("-o")
        .arg("IdentitiesOnly=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg("-o")
        .arg("ServerAliveInterval=30")
        .arg("-o")
        .arg("ServerAliveCountMax=3");
    if forward_agent {
        command.arg("-A");
    }
    command
}

fn probe_ssh(
    ssh_user: &str,
    ssh_private_key: &Path,
    known_hosts: &Path,
    vmnet: &VmnetConfig,
) -> Result<bool, String> {
    let mut command = ssh_base_command(ssh_user, ssh_private_key, known_hosts, vmnet, false);
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectionAttempts=1")
        .arg("-o")
        .arg("ConnectTimeout=2")
        .arg(format!("{ssh_user}@{}", vmnet.guest_ip))
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = command.status().map_err(|err| err.to_string())?;
    Ok(status.success())
}

fn ssh_ready_timeout() -> Duration {
    env::var("VIBEBOX_SSH_READY_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_SSH_READY_TIMEOUT_SECS))
}

struct LaunchProgress<'a> {
    guest_ip: &'a str,
    verbose: bool,
    quiet_ui: Option<QuietLoadingUi>,
    last_reported_second: Option<u64>,
}

impl<'a> LaunchProgress<'a> {
    fn new(guest_ip: &'a str, verbose: bool) -> Self {
        Self {
            guest_ip,
            verbose,
            quiet_ui: if verbose {
                None
            } else {
                Some(QuietLoadingUi::start())
            },
            last_reported_second: None,
        }
    }

    fn tick(&mut self, elapsed: Duration) -> Result<(), String> {
        if self.verbose {
            let elapsed_secs = elapsed.as_secs();
            if self.last_reported_second == Some(elapsed_secs) {
                return Ok(());
            }
            self.last_reported_second = Some(elapsed_secs);
            eprint!("\rwaiting for ssh on {} ({}s)", self.guest_ip, elapsed_secs);
        }
        io::stderr().flush().map_err(|err| err.to_string())
    }

    fn finish(&mut self) -> Result<(), String> {
        if self.last_reported_second.is_some() {
            eprintln!();
            io::stderr().flush().map_err(|err| err.to_string())?;
            self.last_reported_second = None;
        }
        if let Some(mut quiet_ui) = self.quiet_ui.take() {
            quiet_ui.finish();
        }
        Ok(())
    }
}

fn should_probe_ssh(elapsed: Duration, last_probe: Option<Duration>) -> bool {
    last_probe
        .map(|previous| {
            elapsed.saturating_sub(previous) >= Duration::from_millis(SSH_PROBE_INTERVAL_MILLIS)
        })
        .unwrap_or(true)
}

struct QuietLoadingUi {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl QuietLoadingUi {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let anchor_row = position().map(|(_, row)| row).unwrap_or(0);
            let mut terminal = match Terminal::new(CrosstermBackend::new(io::stderr())) {
                Ok(terminal) => terminal,
                Err(_) => return,
            };
            let started = Instant::now();
            let _ = execute!(io::stderr(), Hide);
            while !thread_stop.load(Ordering::Relaxed) {
                let _ = terminal.draw(|frame| {
                    let area = frame.area();
                    if area.height == 0 {
                        return;
                    }
                    let rect = Rect {
                        x: 0,
                        y: anchor_row.min(area.height.saturating_sub(1)),
                        width: area.width,
                        height: 1,
                    };
                    frame.render_widget(Clear, rect);
                    frame.render_widget(
                        Paragraph::new(loading_line(started.elapsed())).alignment(Alignment::Left),
                        rect,
                    );
                });
                thread::sleep(Duration::from_millis(QUIET_LOADING_FRAME_MILLIS));
            }
            let _ = execute!(
                io::stderr(),
                MoveTo(0, anchor_row),
                TermClear(ClearType::CurrentLine),
                Show
            );
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for QuietLoadingUi {
    fn drop(&mut self) {
        self.finish();
    }
}

fn loading_line(elapsed: Duration) -> Line<'static> {
    const TEXT: &str = "Loading";
    const BRIGHT_GRAY: u8 = 255;
    const DARK_GRAY: u8 = 236;
    const DARK_SPOT_SIGMA: f32 = 1.7;
    const SHIMMER_SECONDS: f32 = 1.2;
    const PAUSE_SECONDS: f32 = 0.7;

    let cycle = SHIMMER_SECONDS + PAUSE_SECONDS;
    let cycle_progress = elapsed.as_secs_f32() % cycle;
    let len = TEXT.len() as f32;
    let travel_padding = DARK_SPOT_SIGMA * 2.2;
    let spans = TEXT
        .chars()
        .enumerate()
        .map(|(index, ch)| {
            let gray = if cycle_progress < SHIMMER_SECONDS {
                let progress = cycle_progress / SHIMMER_SECONDS;
                let start = -travel_padding;
                let end = (len - 1.0) + travel_padding;
                let head = start + progress * (end - start);
                let distance = (index as f32 - head).abs();
                let dip = (-distance.powi(2) / (2.0 * DARK_SPOT_SIGMA.powi(2))).exp();
                BRIGHT_GRAY as f32 - dip * (BRIGHT_GRAY - DARK_GRAY) as f32
            } else {
                BRIGHT_GRAY as f32
            };
            Span::styled(
                ch.to_string(),
                Style::default().fg(Color::Indexed(gray.round() as u8)),
            )
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn stop_child(child: &mut Child) -> Result<(), String> {
    if child.try_wait().map_err(|err| err.to_string())?.is_some() {
        return Ok(());
    }
    child.kill().map_err(|err| err.to_string())?;
    let _ = child.wait().map_err(|err| err.to_string())?;
    Ok(())
}

fn stop_stale_vm(pidfile: &Path) -> Result<(), String> {
    if !pidfile.exists() {
        return Ok(());
    }

    let pid = fs::read_to_string(pidfile)
        .map_err(|err| err.to_string())?
        .trim()
        .parse::<i32>()
        .map_err(|err| format!("invalid pidfile {}: {err}", pidfile.display()))?;

    let _ = terminate_pid(pid);
    remove_if_exists(pidfile)?;
    Ok(())
}

fn instance_process_pids(instance_dir: &Path) -> Result<Vec<i32>, String> {
    let output = Command::new("ps")
        .arg("ax")
        .arg("-o")
        .arg("pid=,command=")
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err("failed to inspect process list".to_string());
    }

    Ok(parse_instance_processes(
        &String::from_utf8_lossy(&output.stdout),
        instance_dir,
    ))
}

fn instance_has_running_vm(instance_dir: &Path) -> Result<bool, String> {
    Ok(!instance_process_pids(instance_dir)?.is_empty())
}

fn vmnet_helper_pids(interface_id: &str) -> Result<Vec<i32>, String> {
    let output = Command::new("ps")
        .arg("ax")
        .arg("-o")
        .arg("pid=,command=")
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err("failed to inspect process list".to_string());
    }

    Ok(parse_vmnet_helper_processes(
        &String::from_utf8_lossy(&output.stdout),
        interface_id,
    ))
}

fn parse_instance_processes(output: &str, instance_dir: &Path) -> Vec<i32> {
    let instance_marker = instance_dir.display().to_string();
    let mut pids = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim_start();
        let Some((pid_str, command)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let command = command.trim();
        if !(command.contains("krunkit") || command.contains("vmnet-client")) {
            continue;
        }
        if !command.contains(&instance_marker) {
            continue;
        }
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn parse_vmnet_helper_processes(output: &str, interface_id: &str) -> Vec<i32> {
    let mut pids = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim_start();
        let Some((pid_str, command)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let command = command.trim();
        if !command.contains("vmnet-helper") {
            continue;
        }
        if !command.contains(interface_id) {
            continue;
        }
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn stop_vmnet_helper_interface(interface_id: &str) -> Result<(), String> {
    for pid in vmnet_helper_pids(interface_id)? {
        let _ = terminate_pid(pid);
    }
    Ok(())
}

fn terminate_pid(pid: i32) -> Result<(), String> {
    let term_status = Command::new("kill")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| err.to_string())?;
    if !term_status.success() {
        return Ok(());
    }

    for _ in 0..20 {
        let alive = Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .status()
            .map_err(|err| err.to_string())?
            .success();
        if !alive {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }

    let _ = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn encode_env_ports(ports: &[PortMapping]) -> String {
    ports
        .iter()
        .map(|mapping| format!("{}:{}", mapping.host, mapping.guest))
        .collect::<Vec<_>>()
        .join(",")
}

fn encode_env_shares(shares: &[ShareMount]) -> String {
    shares
        .iter()
        .map(|share| {
            format!(
                "{}:{}",
                share.host_path.display(),
                share.guest_path.display()
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn encode_guest_env_names(vars: &[GuestEnvVar]) -> String {
    vars.iter()
        .map(|var| var.name.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn write_runtime_shares(path: &Path, shares: &[ShareMount]) -> Result<(), String> {
    fs::write(path, encode_env_shares(shares)).map_err(|err| err.to_string())
}

fn running_vm_matches_shares(path: &Path, shares: &[ShareMount]) -> Result<bool, String> {
    if !path.exists() {
        return Ok(shares.is_empty());
    }
    let current = fs::read_to_string(path).map_err(|err| err.to_string())?;
    Ok(current == encode_env_shares(shares))
}

fn share_tag(index: usize) -> String {
    format!("share{index}")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    if path.exists() {
        fs::remove_file(path).map_err(|err| err.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        detect_runtime_failure, encode_env_ports, guest_shell_command, krunkit_network_device,
        loading_line, parse_instance_processes, parse_vmnet_helper_processes,
        running_vm_matches_shares, share_tag, should_probe_ssh, ssh_base_command,
        ssh_ready_timeout, stop_stale_vm, write_runtime_shares,
    };
    use crate::network::VmnetConfig;
    use crate::ports::PortMapping;
    use crate::state::{GuestEnvVar, ShareMount};
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn port_encodings_track_host_to_guest() {
        let spec = encode_env_ports(&[
            PortMapping {
                host: 21344,
                guest: 22,
            },
            PortMapping {
                host: 21345,
                guest: 3000,
            },
        ]);
        assert_eq!(spec, "21344:22,21345:3000");
    }

    #[test]
    fn krunkit_vmnet_helper_device_uses_unixgram_backend() {
        assert_eq!(
            krunkit_network_device("52:54:60:ed:a8:f9"),
            "virtio-net,type=unixgram,fd=4,mac=52:54:60:ed:a8:f9"
        );
    }

    #[test]
    fn guest_shell_waits_for_workspace_readiness() {
        let command = guest_shell_command(
            &[ShareMount {
                host_path: PathBuf::from("/tmp/share"),
                guest_path: PathBuf::from("/mnt/share"),
            }],
            &[GuestEnvVar {
                name: "ANTHROPIC_API_KEY".to_string(),
                value: "sk-ant-abc123".to_string(),
            }],
            true,
        );
        assert!(command.contains("cloud-init status --wait"));
        assert!(command.contains("waiting for cloud-init/bootstrap"));
        assert!(command.contains("/var/log/vibebox-init.log"));
        assert!(command.contains("bootstrap log:"));
        assert!(command.contains("init-log-shown"));
        assert!(command.contains("growpart"));
        assert!(command.contains("resize2fs"));
        assert!(command.contains("/var/lib/vibebox/mounts/workspace"));
        assert!(command.contains("mount -t virtiofs workspace"));
        assert!(command.contains("mount --bind"));
        assert!(command.contains(&format!("mount -t virtiofs {}", share_tag(0))));
        assert!(command.contains("/mnt/share"));
        assert!(command.contains("export ANTHROPIC_API_KEY="));
        assert!(command.contains("git config --global user.name \"$GIT_AUTHOR_NAME\""));
        assert!(command.contains("git config --global user.email \"$GIT_AUTHOR_EMAIL\""));
        assert!(command.contains("cd /workspace"));
    }

    #[test]
    fn quiet_guest_shell_suppresses_bootstrap_chatter() {
        let command = guest_shell_command(&[], &[], false);
        assert!(command.contains("cloud-init status --wait >/dev/null 2>&1"));
        assert!(!command.contains("waiting for cloud-init/bootstrap"));
        assert!(!command.contains("bootstrap log:"));
        assert!(!command.contains("/var/log/vibebox-init.log"));
    }

    #[test]
    fn interactive_ssh_forwards_agent_but_probes_do_not() {
        let vmnet = VmnetConfig {
            client_path: PathBuf::from("/opt/vmnet-helper/bin/vmnet-client"),
            interface_id: "123e4567-e89b-12d3-a456-426614174000".to_string(),
            mac_address: "52:54:de:61:b0:1f".to_string(),
            guest_ip: "192.168.105.33".to_string(),
            gateway_ip: "192.168.105.1".to_string(),
            prefix_len: 24,
            dhcp_start: "192.168.105.1".to_string(),
            dhcp_end: "192.168.105.254".to_string(),
            dns_servers: vec!["1.1.1.1".to_string()],
        };
        let forwarded_args = ssh_base_command(
            "joshv",
            PathBuf::from("/tmp/id_rsa").as_path(),
            PathBuf::from("/tmp/known_hosts").as_path(),
            &vmnet,
            true,
        )
        .get_args()
        .map(OsString::from)
        .collect::<Vec<_>>();
        assert!(forwarded_args.iter().any(|arg| arg == "-A"));
        assert!(!forwarded_args.iter().any(|arg| arg == "-L"));

        let probe_args = ssh_base_command(
            "joshv",
            PathBuf::from("/tmp/id_rsa").as_path(),
            PathBuf::from("/tmp/known_hosts").as_path(),
            &vmnet,
            false,
        )
        .get_args()
        .map(OsString::from)
        .collect::<Vec<_>>();
        assert!(!probe_args.iter().any(|arg| arg == "-A"));
    }

    #[test]
    fn loading_line_animates_highlight() {
        let first = loading_line(Duration::from_millis(0));
        let second = loading_line(Duration::from_millis(600));
        assert_eq!(first.width(), "Loading".len());
        assert_eq!(first.spans.len(), "Loading".len());
        assert_eq!(first.spans[0].content.as_ref(), "L");
        assert_eq!(second.spans[1].content.as_ref(), "o");
        let styles_changed = first
            .spans
            .iter()
            .zip(second.spans.iter())
            .any(|(left, right)| left.style != right.style);
        assert!(styles_changed);
    }

    #[test]
    fn ssh_probe_interval_is_rate_limited() {
        assert!(should_probe_ssh(Duration::from_millis(0), None));
        assert!(!should_probe_ssh(
            Duration::from_millis(500),
            Some(Duration::from_millis(0))
        ));
        assert!(should_probe_ssh(
            Duration::from_millis(1000),
            Some(Duration::from_millis(0))
        ));
    }

    #[test]
    fn stale_vm_cleanup_tolerates_missing_process() {
        let pidfile = PathBuf::from("/tmp/vibebox-stale-test.pid");
        fs::write(&pidfile, "999999\n").unwrap();
        stop_stale_vm(&pidfile).unwrap();
        assert!(!pidfile.exists());
    }

    #[test]
    fn process_parser_ignores_unrelated_lines() {
        let instance_dir = PathBuf::from("/tmp/instance-a");
        let output = "  10 krunkit --pidfile /tmp/instance-a/runtime/krunkit.pid\n  11 /opt/vmnet-helper/bin/vmnet-client -- /tmp/instance-a\n  12 krunkit --pidfile /tmp/instance-b/runtime/krunkit.pid\n  13 ssh something\n";
        let parsed = parse_instance_processes(output, &instance_dir);
        assert_eq!(parsed, vec![10, 11]);
    }

    #[test]
    fn vmnet_helper_parser_matches_interface_id() {
        let output = "  20 sudo --non-interactive /opt/vmnet-helper/bin/vmnet-helper --fd=3 --interface-id aaa\n  21 /opt/vmnet-helper/bin/vmnet-helper --fd=3 --interface-id aaa\n  22 /opt/vmnet-helper/bin/vmnet-helper --fd=3 --interface-id bbb\n";
        let parsed = parse_vmnet_helper_processes(output, "aaa");
        assert_eq!(parsed, vec![20, 21]);
    }

    #[test]
    fn runtime_failure_detection_reports_vmnet_errors() {
        let log_path = PathBuf::from("/tmp/vibebox-runtime-test.log");
        fs::write(
            &log_path,
            "ERROR [main] vmnet_start_interface: VMNET_FAILURE\n",
        )
        .unwrap();
        let error = detect_runtime_failure(&log_path).unwrap();
        assert!(
            error
                .unwrap()
                .contains("vmnet-helper failed to create the shared network interface")
        );
        let _ = fs::remove_file(&log_path);
    }

    #[test]
    fn ssh_ready_timeout_uses_default_when_unset_or_invalid() {
        let saved = env::var("VIBEBOX_SSH_READY_TIMEOUT_SECS").ok();
        unsafe {
            env::remove_var("VIBEBOX_SSH_READY_TIMEOUT_SECS");
        }
        assert_eq!(ssh_ready_timeout(), Duration::from_secs(300));

        unsafe {
            env::set_var("VIBEBOX_SSH_READY_TIMEOUT_SECS", "nope");
        }
        assert_eq!(ssh_ready_timeout(), Duration::from_secs(300));

        unsafe {
            env::remove_var("VIBEBOX_SSH_READY_TIMEOUT_SECS");
        }
        match saved {
            Some(value) => unsafe {
                env::set_var("VIBEBOX_SSH_READY_TIMEOUT_SECS", value);
            },
            None => unsafe {
                env::remove_var("VIBEBOX_SSH_READY_TIMEOUT_SECS");
            },
        }
    }

    #[test]
    fn ssh_ready_timeout_uses_env_override() {
        unsafe {
            env::set_var("VIBEBOX_SSH_READY_TIMEOUT_SECS", "45");
        }
        assert_eq!(ssh_ready_timeout(), Duration::from_secs(45));
        unsafe {
            env::remove_var("VIBEBOX_SSH_READY_TIMEOUT_SECS");
        }
    }

    #[test]
    fn running_vm_matches_share_config_file() {
        let path = PathBuf::from("/tmp/vibebox-shares-test");
        let shares = vec![ShareMount {
            host_path: PathBuf::from("/tmp/host"),
            guest_path: PathBuf::from("/mnt/guest"),
        }];
        write_runtime_shares(&path, &shares).unwrap();
        assert!(running_vm_matches_shares(&path, &shares).unwrap());
        assert!(!running_vm_matches_shares(&path, &[]).unwrap());
        let _ = fs::remove_file(&path);
    }
}
