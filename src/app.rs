use crate::cloud_init::{self, CloudInitOptions};
use crate::git;
use crate::network;
use crate::runtime::{self, LaunchConfig, LaunchMode};
use crate::state::{self, GuestEnvVar, ShareMount};
use clap::error::ErrorKind;
use clap::{ArgAction, ArgGroup, Args, CommandFactory, Parser, Subcommand};
use petname::petname;
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

pub fn run() -> Result<(), String> {
    let cli = Cli::parse(env::args().skip(1).collect())?;

    match cli.command {
        Command::Base(command) => base(command),
        Command::Launch(options) => launch(options),
        Command::Status(options) => status(options),
        Command::Stop(options) => stop(options),
        Command::Doctor => doctor(),
        Command::List => list_instances(),
        Command::Destroy(options) => destroy(options),
        Command::Help(text) => {
            print!("{text}");
            Ok(())
        }
    }
}

#[derive(Debug)]
enum Command {
    Base(BaseCommand),
    Launch(LaunchOptions),
    Status(TargetOptions),
    Stop(TargetOptions),
    Doctor,
    List,
    Destroy(DestroyOptions),
    Help(String),
}

#[derive(Clone, Debug, Subcommand)]
enum BaseCommand {
    Import(BaseImportOptions),
    Capture(BaseCaptureOptions),
    List,
}

#[derive(Debug)]
struct Cli {
    command: Command,
}

#[derive(Clone, Debug, Args)]
struct BaseImportOptions {
    #[arg(long)]
    name: String,
    #[arg(long)]
    image: PathBuf,
}

#[derive(Clone, Debug, Args)]
#[command(group(
    ArgGroup::new("capture_target")
        .required(true)
        .args(["instance", "repo", "base"])
))]
struct BaseCaptureOptions {
    #[arg(long)]
    name: String,
    #[arg(long = "instance", conflicts_with_all = ["repo", "branch", "base"])]
    instance: Option<String>,
    #[arg(long, requires = "branch", conflicts_with = "base")]
    repo: Option<String>,
    #[arg(long, requires = "repo")]
    branch: Option<String>,
    #[arg(long, conflicts_with_all = ["instance", "repo", "branch"])]
    base: Option<String>,
}

#[derive(Clone, Debug, Args)]
#[command(group(
    ArgGroup::new("launch_selector")
        .required(false)
        .args(["name", "repo"])
))]
struct LaunchOptions {
    #[arg(long, conflicts_with_all = ["repo", "branch"])]
    name: Option<String>,
    #[arg(long, conflicts_with = "name")]
    repo: Option<String>,
    #[arg(long, requires = "repo")]
    branch: Option<String>,
    #[arg(long, requires_all = ["repo", "branch"])]
    new_branch: bool,
    #[arg(long, requires_all = ["repo", "branch"])]
    from: Option<String>,
    #[arg(long)]
    base: Option<String>,
    #[arg(long, hide = true)]
    vm: bool,
    #[arg(long = "shell")]
    shell_only: bool,
    #[arg(long = "no-enter")]
    no_enter: bool,
    #[arg(long, default_value_t = runtime::default_cpus())]
    cpus: u8,
    #[arg(long = "memory-mib", default_value_t = runtime::default_memory_mib())]
    memory_mib: u32,
    #[arg(long = "no-cloud-init", action = ArgAction::SetFalse, default_value_t = true)]
    cloud_init: bool,
    #[arg(long = "cloud-user", default_value_t = cloud_init::default_cloud_user())]
    cloud_user: String,
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long = "ssh-pubkey")]
    ssh_pubkey: Option<PathBuf>,
    #[arg(long = "ssh-private-key")]
    ssh_private_key: Option<PathBuf>,
    #[arg(long = "init-script")]
    init_script: Option<PathBuf>,
    #[arg(long = "share", value_parser = state::parse_share)]
    shares: Vec<ShareMount>,
    #[arg(long = "with-codex")]
    with_codex: bool,
    #[arg(long = "with-claude")]
    with_claude: bool,
    #[arg(long = "with-gh")]
    with_gh: bool,
    #[arg(long = "with-ai")]
    with_ai: bool,
    #[arg(long)]
    verbose: bool,
    #[arg(long = "clear-shares")]
    clear_shares: bool,
}

#[derive(Clone, Debug, Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .args(["name", "repo", "base"])
))]
struct TargetOptions {
    #[arg(long, conflicts_with_all = ["repo", "branch", "base"])]
    name: Option<String>,
    #[arg(long, requires = "branch", conflicts_with = "base")]
    repo: Option<String>,
    #[arg(long, requires = "repo")]
    branch: Option<String>,
    #[arg(long, conflicts_with_all = ["name", "repo", "branch"])]
    base: Option<String>,
}

#[derive(Clone, Debug, Args)]
struct DestroyOptions {
    #[command(flatten)]
    target: TargetOptions,
    #[arg(long = "yes", short = 'y')]
    yes: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "yolobox",
    disable_help_subcommand = true,
    args_conflicts_with_subcommands = true
)]
struct ClapCli {
    #[command(subcommand)]
    command: Option<ClapCommand>,
    #[command(flatten)]
    launch: LaunchOptions,
}

#[derive(Debug, Subcommand)]
enum ClapCommand {
    Base(ClapBaseCommand),
    Launch(LaunchOptions),
    Status(TargetOptions),
    Stop(TargetOptions),
    Doctor,
    List,
    Destroy(DestroyOptions),
    Help,
}

#[derive(Debug, Args)]
struct ClapBaseCommand {
    #[command(subcommand)]
    command: BaseCommand,
}

impl Cli {
    fn parse(args: Vec<String>) -> Result<Self, String> {
        let parse_input = std::iter::once("yolobox".to_string())
            .chain(args)
            .collect::<Vec<_>>();
        let cli = match ClapCli::try_parse_from(parse_input) {
            Ok(cli) => cli,
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                ) =>
            {
                return Ok(Self {
                    command: Command::Help(err.to_string()),
                });
            }
            Err(err) => return Err(err.to_string()),
        };

        let command = match cli.command {
            Some(ClapCommand::Base(command)) => Command::Base(command.command),
            Some(ClapCommand::Launch(options)) => {
                Command::Launch(normalize_launch_options(options))
            }
            Some(ClapCommand::Status(options)) => Command::Status(options),
            Some(ClapCommand::Stop(options)) => Command::Stop(options),
            Some(ClapCommand::Doctor) => Command::Doctor,
            Some(ClapCommand::List) => Command::List,
            Some(ClapCommand::Destroy(options)) => Command::Destroy(options),
            Some(ClapCommand::Help) => Command::Help(render_help()),
            None => Command::Launch(normalize_launch_options(cli.launch)),
        };

        Ok(Self { command })
    }
}

fn base(command: BaseCommand) -> Result<(), String> {
    match command {
        BaseCommand::Import(options) => {
            let image = state::import_base_image(&options.name, &options.image)?;
            for line in image.summary_lines() {
                println!("{line}");
            }
            Ok(())
        }
        BaseCommand::Capture(options) => {
            let instance = state::find_instance(
                options.instance.as_deref(),
                options.repo.as_deref(),
                options.branch.as_deref(),
                options.base.as_deref(),
            )?
            .ok_or_else(|| {
                missing_instance_message(&TargetOptions {
                    name: options.instance.clone(),
                    repo: options.repo.clone(),
                    branch: options.branch.clone(),
                    base: options.base.clone(),
                })
            })?;
            runtime::stop_instance_vm(&instance.instance_dir)?;
            let image = state::import_base_image(&options.name, &instance.rootfs_path)?;
            for line in image.summary_lines() {
                println!("{line}");
            }
            Ok(())
        }
        BaseCommand::List => {
            let images = state::list_base_images()?;
            if images.is_empty() {
                println!("no base images");
                return Ok(());
            }

            for image in images {
                println!(
                    "{}  {}  {} MiB",
                    image.name,
                    image.id,
                    image.size_bytes.div_ceil(1024 * 1024)
                );
            }
            Ok(())
        }
    }
}

fn launch(options: LaunchOptions) -> Result<(), String> {
    let options = resolve_launch_selector(options)?;

    if options.vm && options.shell_only {
        return Err("--vm and --shell are mutually exclusive".to_string());
    }
    if options.clear_shares
        && (!options.shares.is_empty()
            || options.with_codex
            || options.with_claude
            || options.with_gh)
    {
        return Err(
            "--clear-shares cannot be combined with --share, --with-codex, --with-claude, --with-gh, or --with-ai"
                .to_string(),
        );
    }
    if !options.cloud_init && options.init_script.is_some() {
        return Err("--init-script requires cloud-init; remove --no-cloud-init".to_string());
    }

    let plan = runtime::resolve_runtime(options.shell_only);
    let resolved_base = if options.base.is_some() {
        options.base.clone()
    } else {
        default_base_name()?
    };
    preflight_launch(&options, &plan, resolved_base.as_deref())?;

    let generated_name = if options.name.is_none() && options.repo.is_none() {
        Some(generate_instance_name()?)
    } else {
        None
    };
    let instance_name = options.name.as_deref().or(generated_name.as_deref());
    let requested_shares = if options.clear_shares {
        Some(Vec::new())
    } else {
        build_requested_shares(&options)?
    };
    let requested_guest_env = build_requested_guest_env(&options)?;
    let instance = state::ensure_instance(
        instance_name,
        options.repo.as_deref(),
        options.branch.as_deref(),
        resolved_base.as_deref(),
        requested_shares.as_deref(),
        requested_guest_env.as_deref(),
    )?;
    let vmnet = match plan.mode {
        LaunchMode::Krunkit => Some(network::resolve_for_instance(&instance)?),
        _ => None,
    };
    let prepared_cloud_init = match plan.mode {
        LaunchMode::External(_) | LaunchMode::Krunkit => cloud_init::prepare(
            &instance,
            &CloudInitOptions {
                enabled: options.cloud_init,
                user: options.cloud_user.clone(),
                hostname: options
                    .hostname
                    .clone()
                    .or_else(|| instance_name.map(str::to_string)),
                ssh_pubkey: options.ssh_pubkey.clone(),
                init_script: options.init_script.clone(),
                shares: instance.shares.clone(),
                network: vmnet.clone(),
                verbose: options.verbose,
            },
        )?,
        LaunchMode::Shell => None,
    };

    if let (Some(repo), Some(branch)) = (options.repo.as_deref(), options.branch.as_deref()) {
        git::ensure_checkout(
            &instance.checkout_dir,
            repo,
            branch,
            options.new_branch,
            options.from.as_deref(),
        )?;
    }

    let ssh_private_key_path = options
        .ssh_private_key
        .clone()
        .or_else(cloud_init::discover_ssh_private_key)
        .or_else(|| {
            options
                .ssh_pubkey
                .as_ref()
                .and_then(|path| cloud_init::private_key_from_public(path))
        })
        .or_else(|| {
            prepared_cloud_init
                .as_ref()
                .and_then(|prepared| cloud_init::private_key_from_public(&prepared.ssh_pubkey_path))
        });

    let launch_config = LaunchConfig {
        require_vm: !options.shell_only,
        cpus: options.cpus,
        memory_mib: options.memory_mib,
        cloud_init_image: prepared_cloud_init
            .as_ref()
            .map(|prepared| prepared.image_path.clone()),
        cloud_init_user: prepared_cloud_init
            .as_ref()
            .map(|prepared| prepared.user.clone()),
        hostname: prepared_cloud_init
            .as_ref()
            .map(|prepared| prepared.hostname.clone()),
        ssh_pubkey_path: prepared_cloud_init
            .as_ref()
            .map(|prepared| prepared.ssh_pubkey_path.clone()),
        ssh_private_key_path,
        init_script_path: prepared_cloud_init
            .as_ref()
            .and_then(|prepared| prepared.init_script_path.clone()),
        shares: instance.shares.clone(),
        guest_env: instance.guest_env.clone(),
        verbose: options.verbose,
        vmnet,
    };

    if options.verbose {
        for line in runtime::launch_summary(&instance, &launch_config) {
            println!("{line}");
        }
    }

    if options.no_enter {
        return Ok(());
    }

    let code = runtime::launch(plan, &instance, &launch_config)?;
    if code != 0 {
        return Err(format!("child process exited with code {code}"));
    }
    Ok(())
}

fn status(options: TargetOptions) -> Result<(), String> {
    let instance = state::find_instance(
        options.name.as_deref(),
        options.repo.as_deref(),
        options.branch.as_deref(),
        options.base.as_deref(),
    )?
    .ok_or_else(|| missing_instance_message(&options))?;
    for line in instance.summary_lines() {
        println!("{line}");
    }
    println!(
        "runtime: {}",
        if runtime::is_instance_vm_running(&instance.instance_dir)? {
            "running"
        } else {
            "stopped"
        }
    );
    Ok(())
}

fn list_instances() -> Result<(), String> {
    let instances = state::list_instances()?;
    if instances.is_empty() {
        println!("no instances");
        return Ok(());
    }

    for instance in instances {
        let runtime_state = if runtime::is_instance_vm_running(&instance.instance_dir)? {
            "running"
        } else {
            "stopped"
        };
        println!(
            "{}  {}  {}  {}  {}",
            instance.id,
            instance.repo.as_deref().unwrap_or("-"),
            instance.branch.as_deref().unwrap_or("-"),
            instance.base_image_name,
            runtime_state
        );
    }
    Ok(())
}

fn doctor() -> Result<(), String> {
    let krunkit = runtime::command_exists("krunkit");
    let vmnet_client = network::find_vmnet_client();
    let ssh_public = cloud_init::discover_ssh_public_key();
    let ssh_private = cloud_init::discover_ssh_private_key().or_else(|| {
        ssh_public
            .as_ref()
            .and_then(|path| cloud_init::private_key_from_public(path))
    });

    println!(
        "krunkit: {}",
        if krunkit {
            "ok"
        } else {
            "missing (brew tap slp/krunkit && brew install krunkit)"
        }
    );
    println!(
        "vmnet-client: {}",
        vmnet_client
            .as_ref()
            .map(|path| format!("ok ({})", path.display()))
            .unwrap_or_else(|| {
                "missing (curl -fsSL https://raw.githubusercontent.com/nirs/vmnet-helper/main/install.sh | sudo bash)".to_string()
            })
    );
    println!(
        "ssh_pubkey: {}",
        ssh_public
            .as_ref()
            .map(|path| format!("ok ({})", path.display()))
            .unwrap_or_else(|| "missing".to_string())
    );
    println!(
        "ssh_private_key: {}",
        ssh_private
            .as_ref()
            .map(|path| format!("ok ({})", path.display()))
            .unwrap_or_else(|| "missing".to_string())
    );
    println!(
        "builtin_vm_ready: {}",
        if krunkit && vmnet_client.is_some() && ssh_public.is_some() && ssh_private.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    Ok(())
}

fn destroy(options: DestroyOptions) -> Result<(), String> {
    if let Some(instance) = state::find_instance(
        options.target.name.as_deref(),
        options.target.repo.as_deref(),
        options.target.branch.as_deref(),
        options.target.base.as_deref(),
    )? {
        if !options.yes && !confirm_destroy(&instance.id)? {
            println!("aborted");
            return Ok(());
        }
        runtime::stop_instance_vm(&instance.instance_dir)?;
    }
    let removed = state::destroy_instance(
        options.target.name.as_deref(),
        options.target.repo.as_deref(),
        options.target.branch.as_deref(),
        options.target.base.as_deref(),
    )?;
    match removed {
        Some(path) => println!("removed {}", path.display()),
        None => println!("nothing to remove"),
    }
    Ok(())
}

fn stop(options: TargetOptions) -> Result<(), String> {
    let instance = state::find_instance(
        options.name.as_deref(),
        options.repo.as_deref(),
        options.branch.as_deref(),
        options.base.as_deref(),
    )?
    .ok_or_else(|| missing_instance_message(&options))?;
    runtime::stop_instance_vm(&instance.instance_dir)?;
    println!("stopped {}", instance.id);
    Ok(())
}

fn confirm_destroy(instance_id: &str) -> Result<bool, String> {
    print!("Destroy instance {instance_id}? [y/N] ");
    io::stdout().flush().map_err(|err| err.to_string())?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|err| err.to_string())?;
    Ok(confirm_destroy_answer(&input))
}

fn confirm_destroy_answer(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn resolve_launch_selector(mut options: LaunchOptions) -> Result<LaunchOptions, String> {
    if options.repo.is_some() && options.branch.is_none() {
        let repo = options.repo.as_deref().unwrap_or_default();
        let branch = prompt_for_branch(repo)?;
        options.branch = Some(branch);
    }
    Ok(options)
}

fn prompt_for_branch(repo: &str) -> Result<String, String> {
    if !io::stdin().is_terminal() {
        return Err(
            "--repo was provided without --branch, but stdin is not interactive; pass --branch explicitly"
                .to_string(),
        );
    }

    let branches = git::list_recent_remote_branches(repo, 12)?;
    if branches.is_empty() {
        return Err(format!("no remote branches found for {repo}"));
    }
    if branches.len() == 1 {
        return Ok(branches[0].clone());
    }

    eprintln!("Select a branch for {repo}:");
    for (index, branch) in branches.iter().enumerate() {
        eprintln!("  {}. {}", index + 1, branch);
    }

    loop {
        eprint!("Branch [1-{}] (default 1): ", branches.len());
        io::stderr().flush().map_err(|err| err.to_string())?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|err| err.to_string())?;
        let selection = input.trim();
        if selection.is_empty() {
            return Ok(branches[0].clone());
        }
        if let Ok(index) = selection.parse::<usize>() {
            if (1..=branches.len()).contains(&index) {
                return Ok(branches[index - 1].clone());
            }
        }
        eprintln!(
            "Invalid selection. Enter a number from 1 to {}.",
            branches.len()
        );
    }
}

fn missing_instance_message(options: &TargetOptions) -> String {
    match (
        options.name.as_deref(),
        options.repo.as_deref(),
        options.branch.as_deref(),
        options.base.as_deref(),
    ) {
        (Some(name), _, _, _) => format!("no instance found for name {}", name),
        (None, Some(repo), Some(branch), _) => format!("no instance found for {} {}", repo, branch),
        (None, None, None, Some(base)) => format!("no base-only instance found for base {}", base),
        _ => "no instance found".to_string(),
    }
}

fn preflight_launch(
    options: &LaunchOptions,
    plan: &runtime::RuntimePlan,
    resolved_base: Option<&str>,
) -> Result<(), String> {
    if resolved_base.is_none() {
        return Err(missing_base_guidance());
    }

    if !options.shell_only && matches!(plan.mode, LaunchMode::Shell) {
        return Err(missing_vm_runtime_guidance());
    }

    if !options.shell_only {
        let ssh_pubkey = options
            .ssh_pubkey
            .clone()
            .or_else(cloud_init::discover_ssh_public_key);
        let ssh_private = options
            .ssh_private_key
            .clone()
            .or_else(cloud_init::discover_ssh_private_key)
            .or_else(|| {
                ssh_pubkey
                    .as_ref()
                    .and_then(|path| cloud_init::private_key_from_public(path))
            });

        if ssh_pubkey.is_none() || ssh_private.is_none() {
            return Err(missing_ssh_guidance());
        }
    }

    Ok(())
}

fn missing_base_guidance() -> String {
    [
        "no base images are imported yet.",
        "",
        "Get a Linux guest image and import one first. Example:",
        "  curl -LO https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-arm64.img",
        "  qemu-img convert -f qcow2 -O raw jammy-server-cloudimg-arm64.img ubuntu-jammy-arm64.raw",
        "  yolobox base import --name ubuntu --image ./ubuntu-jammy-arm64.raw",
        "",
        "Then launch:",
        "  yolobox --base ubuntu",
        "",
        "For a full environment check:",
        "  yolobox doctor",
    ]
    .join("\n")
}

fn missing_vm_runtime_guidance() -> String {
    let mut lines = vec![
        "yolobox cannot launch a VM yet because the built-in runtime is not fully installed."
            .to_string(),
    ];
    lines.push(String::new());

    if !runtime::command_exists("krunkit") {
        lines.push("Install krunkit:".to_string());
        lines.push("  brew tap slp/krunkit".to_string());
        lines.push("  brew install krunkit".to_string());
        lines.push(String::new());
    }

    if network::find_vmnet_client().is_none() {
        lines.push("Install vmnet-helper:".to_string());
        lines.push(
            "  curl -fsSL https://raw.githubusercontent.com/nirs/vmnet-helper/main/install.sh | sudo bash"
                .to_string(),
        );
        lines.push(String::new());
    }

    lines.push("Then verify the machine is ready:".to_string());
    lines.push("  yolobox doctor".to_string());
    lines.push(String::new());
    lines.push("If you only want a host shell instead of a VM:".to_string());
    lines.push("  yolobox --shell".to_string());
    lines.join("\n")
}

fn missing_ssh_guidance() -> String {
    [
        "yolobox needs an SSH keypair to log into the guest automatically.",
        "",
        "Create one if you do not already have it:",
        "  ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519",
        "",
        "Or pass explicit paths:",
        "  yolobox --ssh-pubkey ~/.ssh/id_ed25519.pub --ssh-private-key ~/.ssh/id_ed25519",
        "",
        "For a full environment check:",
        "  yolobox doctor",
    ]
    .join("\n")
}

fn normalize_launch_options(mut options: LaunchOptions) -> LaunchOptions {
    if options.with_ai {
        options.with_codex = true;
        options.with_claude = true;
    }
    options
}

fn render_help() -> String {
    let mut command = ClapCli::command();
    let mut output = Vec::new();
    command
        .write_long_help(&mut output)
        .expect("writing clap help should succeed");
    String::from_utf8(output).expect("clap help should be valid UTF-8")
}

fn build_requested_shares(options: &LaunchOptions) -> Result<Option<Vec<ShareMount>>, String> {
    let mut shares = options.shares.clone();
    if options.with_codex {
        shares.push(default_credential_share(".codex", &options.cloud_user)?);
    }
    if options.with_claude {
        shares.push(default_credential_share(".claude", &options.cloud_user)?);
    }
    if options.with_gh {
        if let Ok(share) = default_credential_share(".config/gh", &options.cloud_user) {
            shares.push(share);
        }
    }

    if shares.is_empty() {
        return Ok(None);
    }

    shares.sort();
    shares.dedup();
    Ok(Some(shares))
}

fn build_requested_guest_env(options: &LaunchOptions) -> Result<Option<Vec<GuestEnvVar>>, String> {
    let mut vars = Vec::new();

    if let Some(name) = host_git_config("user.name")? {
        vars.push(GuestEnvVar {
            name: "GIT_AUTHOR_NAME".to_string(),
            value: name.clone(),
        });
        vars.push(GuestEnvVar {
            name: "GIT_COMMITTER_NAME".to_string(),
            value: name,
        });
    }
    if let Some(email) = host_git_config("user.email")? {
        vars.push(GuestEnvVar {
            name: "GIT_AUTHOR_EMAIL".to_string(),
            value: email.clone(),
        });
        vars.push(GuestEnvVar {
            name: "GIT_COMMITTER_EMAIL".to_string(),
            value: email,
        });
    }

    if options.with_claude {
        if let Some(value) = env::var_os("ANTHROPIC_API_KEY") {
            vars.push(GuestEnvVar {
                name: "ANTHROPIC_API_KEY".to_string(),
                value: value
                    .into_string()
                    .map_err(|_| "ANTHROPIC_API_KEY is not valid UTF-8".to_string())?,
            });
        }
    }

    if options.with_gh {
        if let Some(token) = host_gh_auth_token()? {
            vars.push(GuestEnvVar {
                name: "GH_TOKEN".to_string(),
                value: token,
            });
        }
    }

    if vars.is_empty() {
        return Ok(None);
    }

    vars.sort_by(|left, right| left.name.cmp(&right.name));
    vars.dedup_by(|left, right| left.name == right.name);
    Ok(Some(vars))
}

fn host_git_config(key: &str) -> Result<Option<String>, String> {
    let output = ProcessCommand::new("git")
        .arg("config")
        .arg("--global")
        .arg("--get")
        .arg(key)
        .output()
        .map_err(|err| format!("failed to query host git config {key}: {err}"))?;

    if !output.status.success() {
        return Ok(None);
    }

    let value = String::from_utf8(output.stdout)
        .map_err(|_| format!("host git config {key} is not valid UTF-8"))?
        .trim()
        .to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn host_gh_auth_token() -> Result<Option<String>, String> {
    let output = ProcessCommand::new("gh")
        .arg("auth")
        .arg("token")
        .output()
        .map_err(|err| err.to_string())?;

    if !output.status.success() {
        return Ok(None);
    }

    let token = String::from_utf8(output.stdout).map_err(|err| err.to_string())?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn default_credential_share(dotdir: &str, cloud_user: &str) -> Result<ShareMount, String> {
    let home = env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;
    let host_path = PathBuf::from(home).join(dotdir);
    let guest_path = PathBuf::from(format!("/home/{cloud_user}/{dotdir}"));
    state::share_mount(&host_path, &guest_path).map_err(|err| {
        format!(
            "could not enable {} sharing from {} to {}: {err}",
            dotdir,
            host_path.display(),
            guest_path.display()
        )
    })
}

fn default_base_name() -> Result<Option<String>, String> {
    let images = state::list_base_images()?;
    Ok(images
        .into_iter()
        .max_by_key(|image| image.created_unix)
        .map(|image| image.name))
}

fn generate_instance_name() -> Result<String, String> {
    let existing = state::list_instances()?
        .into_iter()
        .map(|instance| instance.id)
        .collect::<std::collections::BTreeSet<_>>();

    for _ in 0..256 {
        let candidate = petname(2, "-")
            .ok_or_else(|| "petname failed to generate an instance name".to_string())?;
        if !existing.contains(&candidate) {
            return Ok(candidate);
        }
    }

    Err("could not generate a unique instance name; pass --name explicitly".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        BaseCommand, Cli, Command, confirm_destroy_answer, missing_base_guidance,
        missing_ssh_guidance, missing_vm_runtime_guidance,
    };

    #[test]
    fn no_args_defaults_to_launch() {
        let cli = Cli::parse(Vec::new()).expect("parse should succeed");
        assert!(matches!(cli.command, Command::Launch(_)));
    }

    #[test]
    fn top_level_flags_parse_as_launch() {
        let cli = Cli::parse(vec![
            "--base".to_string(),
            "ubuntu".to_string(),
            "--with-gh".to_string(),
            "--share".to_string(),
            "/tmp:/mnt/tmp".to_string(),
        ])
        .expect("parse should succeed");

        match cli.command {
            Command::Launch(options) => {
                assert_eq!(options.base.as_deref(), Some("ubuntu"));
                assert!(options.with_gh);
                assert_eq!(options.shares.len(), 1);
            }
            _ => panic!("expected launch command"),
        }
    }

    #[test]
    fn explicit_stop_subcommand_parses() {
        let cli = Cli::parse(vec![
            "stop".to_string(),
            "--name".to_string(),
            "tools-box".to_string(),
        ])
        .expect("parse should succeed");

        match cli.command {
            Command::Stop(options) => {
                assert_eq!(options.name.as_deref(), Some("tools-box"));
            }
            _ => panic!("expected stop command"),
        }
    }

    #[test]
    fn explicit_destroy_subcommand_parses_yes() {
        let cli = Cli::parse(vec![
            "destroy".to_string(),
            "--name".to_string(),
            "tools-box".to_string(),
            "--yes".to_string(),
        ])
        .expect("parse should succeed");

        match cli.command {
            Command::Destroy(options) => {
                assert_eq!(options.target.name.as_deref(), Some("tools-box"));
                assert!(options.yes);
            }
            _ => panic!("expected destroy command"),
        }
    }

    #[test]
    fn explicit_base_capture_subcommand_parses() {
        let cli = Cli::parse(vec![
            "base".to_string(),
            "capture".to_string(),
            "--name".to_string(),
            "ubuntu-dev".to_string(),
            "--instance".to_string(),
            "tools-box".to_string(),
        ])
        .expect("parse should succeed");

        match cli.command {
            Command::Base(BaseCommand::Capture(options)) => {
                assert_eq!(options.name, "ubuntu-dev");
                assert_eq!(options.instance.as_deref(), Some("tools-box"));
            }
            _ => panic!("expected base capture command"),
        }
    }

    #[test]
    fn status_still_enforces_repo_branch_pairing() {
        let err = Cli::parse(vec![
            "status".to_string(),
            "--repo".to_string(),
            "git@github.com:org/repo.git".to_string(),
        ])
        .expect_err("parse should fail");
        assert!(err.contains("--repo <REPO>"));
        assert!(err.contains("--branch <BRANCH>"));
    }

    #[test]
    fn status_requires_a_target_selector() {
        let err = Cli::parse(vec!["status".to_string()]).expect_err("parse should fail");
        assert!(err.contains("required arguments were not provided"));
    }

    #[test]
    fn launch_accepts_repo_without_branch_for_interactive_selection() {
        let cli = Cli::parse(vec![
            "--repo".to_string(),
            "git@github.com:org/repo.git".to_string(),
        ])
        .expect("parse should succeed");
        match cli.command {
            Command::Launch(options) => {
                assert_eq!(options.repo.as_deref(), Some("git@github.com:org/repo.git"));
                assert!(options.branch.is_none());
            }
            _ => panic!("expected launch command"),
        }
    }

    #[test]
    fn missing_base_guidance_includes_import_example() {
        let guidance = missing_base_guidance();
        assert!(guidance.contains("yolobox base import --name ubuntu"));
        assert!(guidance.contains("yolobox --base ubuntu"));
    }

    #[test]
    fn missing_ssh_guidance_includes_keygen_example() {
        let guidance = missing_ssh_guidance();
        assert!(guidance.contains("ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519"));
        assert!(guidance.contains("yolobox doctor"));
    }

    #[test]
    fn missing_vm_runtime_guidance_includes_doctor_hint() {
        let guidance = missing_vm_runtime_guidance();
        assert!(guidance.contains("yolobox doctor"));
    }

    #[test]
    fn destroy_confirmation_accepts_yes_forms() {
        assert!(confirm_destroy_answer("y"));
        assert!(confirm_destroy_answer("yes"));
        assert!(!confirm_destroy_answer(""));
        assert!(!confirm_destroy_answer("n"));
    }
}
