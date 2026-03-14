use crate::cloud_init::{self, CloudInitOptions};
use crate::git;
use crate::host_bridge;
use crate::network;
use crate::runtime::{self, GuestExecCommand, LaunchConfig, LaunchMode};
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
        Command::Exec(options) => exec(options),
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
    /// Manage immutable base images used to create instances.
    Base(BaseCommand),
    /// Create or reopen an instance and optionally enter it.
    Launch(LaunchOptions),
    /// Run a non-interactive command inside an instance over SSH.
    Exec(ExecOptions),
    /// Show instance metadata and whether the VM is currently running.
    Status(TargetOptions),
    /// Stop a running VM without deleting its state.
    Stop(TargetOptions),
    /// Check whether host prerequisites for the built-in VM runtime are installed.
    Doctor,
    /// List all known instances.
    List,
    /// Delete an instance and all of its state.
    Destroy(DestroyOptions),
    Help(String),
}

#[derive(Clone, Debug, Subcommand)]
enum BaseCommand {
    /// Import a raw disk image as a named immutable base image.
    Import(BaseImportOptions),
    /// Capture an instance root disk as a new immutable base image.
    Capture(BaseCaptureOptions),
    /// List imported base images.
    List,
}

#[derive(Debug)]
struct Cli {
    command: Command,
}

#[derive(Clone, Debug, Args)]
struct BaseImportOptions {
    /// Name used to refer to the imported base image.
    #[arg(long)]
    name: String,
    /// Path to a raw disk image file to import.
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
    /// Name for the newly captured base image.
    #[arg(long)]
    name: String,
    /// Capture from an existing standalone instance by name.
    #[arg(long = "instance", conflicts_with_all = ["repo", "branch", "base"])]
    instance: Option<String>,
    /// Capture from the instance identified by this repo.
    #[arg(long, requires = "branch", conflicts_with = "base")]
    repo: Option<String>,
    /// Capture from the instance identified by this branch.
    #[arg(long, requires = "repo")]
    branch: Option<String>,
    /// Capture from a base-only instance identified by base name.
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
    /// Launch or reopen a standalone instance with this explicit name.
    #[arg(long, conflicts_with_all = ["repo", "branch"])]
    name: Option<String>,
    /// Git remote URL to clone and keep checked out in the instance.
    #[arg(long, conflicts_with = "name")]
    repo: Option<String>,
    /// Branch to check out for the instance.
    #[arg(long, requires = "repo")]
    branch: Option<String>,
    /// Create the branch if it does not already exist on the remote.
    #[arg(long, requires_all = ["repo", "branch"])]
    new_branch: bool,
    /// Base branch to use with --new-branch. Defaults to the remote's default branch.
    #[arg(long, requires_all = ["repo", "branch"])]
    from: Option<String>,
    /// Base image to use when creating a new instance.
    #[arg(long)]
    base: Option<String>,
    #[arg(long, hide = true)]
    vm: bool,
    /// Skip the VM and open a host shell in the checkout directory instead.
    #[arg(long = "shell")]
    shell_only: bool,
    /// Prepare the instance and exit before launching or attaching to it.
    #[arg(long = "no-enter")]
    no_enter: bool,
    /// Number of virtual CPUs to allocate to the VM.
    #[arg(long, default_value_t = runtime::default_cpus())]
    cpus: u8,
    /// Memory to allocate to the VM in MiB.
    #[arg(long = "memory-mib", default_value_t = runtime::default_memory_mib())]
    memory_mib: u32,
    /// Skip cloud-init guest configuration and use the base image as-is.
    #[arg(long = "no-cloud-init", action = ArgAction::SetFalse, default_value_t = true)]
    cloud_init: bool,
    /// Guest username to create or use when cloud-init is enabled.
    #[arg(long = "user", default_value_t = cloud_init::default_cloud_user())]
    cloud_user: String,
    /// Override the guest hostname.
    #[arg(long)]
    hostname: Option<String>,
    /// Path to the public SSH key to authorize for guest login.
    #[arg(long = "ssh-pubkey")]
    ssh_pubkey: Option<PathBuf>,
    /// Path to the private SSH key the launcher uses to SSH into the guest; it is not copied into the VM.
    #[arg(long = "ssh-private-key")]
    ssh_private_key: Option<PathBuf>,
    /// Run this script once inside the guest after first boot.
    #[arg(long = "init-script")]
    init_script: Option<PathBuf>,
    /// Share a host directory into the guest as <host_path>:<guest_path>.
    #[arg(long = "share", value_parser = state::parse_share)]
    shares: Vec<ShareMount>,
    #[arg(long = "with-codex", hide = true)]
    with_codex: bool,
    #[arg(long = "with-claude", hide = true)]
    with_claude: bool,
    #[arg(long = "with-gh", hide = true)]
    with_gh: bool,
    #[arg(long = "with-ai", hide = true)]
    with_ai: bool,
    /// Disable Codex integration for this launch.
    #[arg(long = "no-codex", conflicts_with = "with_codex")]
    no_codex: bool,
    /// Disable Claude integration for this launch.
    #[arg(long = "no-claude", conflicts_with = "with_claude")]
    no_claude: bool,
    /// Disable GitHub CLI integration for this launch.
    #[arg(long = "no-gh", conflicts_with = "with_gh")]
    no_gh: bool,
    /// Disable Codex, Claude, and GitHub CLI integrations for this launch.
    #[arg(long = "no-ai", conflicts_with = "with_ai")]
    no_ai: bool,
    /// Print VM launch details and guest bootstrap progress.
    #[arg(long)]
    verbose: bool,
    /// Replace previously saved manual shares instead of reusing them.
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
    /// Target a standalone instance by name.
    #[arg(long, conflicts_with_all = ["repo", "branch", "base"])]
    name: Option<String>,
    /// Target the instance identified by this repo.
    #[arg(long, requires = "branch", conflicts_with = "base")]
    repo: Option<String>,
    /// Target the instance identified by this branch.
    #[arg(long, requires = "repo")]
    branch: Option<String>,
    /// Target a base-only instance identified by base name.
    #[arg(long, conflicts_with_all = ["name", "repo", "branch"])]
    base: Option<String>,
}

#[derive(Clone, Debug, Args)]
struct DestroyOptions {
    #[command(flatten)]
    target: TargetOptions,
    /// Skip the interactive confirmation prompt.
    #[arg(long = "yes", short = 'y')]
    yes: bool,
}

#[derive(Clone, Debug, Args)]
struct ExecOptions {
    #[command(flatten)]
    target: TargetOptions,
    /// Guest username to use for the SSH connection.
    #[arg(long = "user", default_value_t = cloud_init::default_cloud_user())]
    user: String,
    /// Path to the private SSH key used to connect to the guest.
    #[arg(long = "ssh-private-key")]
    ssh_private_key: Option<PathBuf>,
    /// Guest working directory for the command. Defaults to /workspace.
    #[arg(long = "cwd")]
    cwd: Option<String>,
    /// Extra environment variable to export before running the command, as NAME=VALUE.
    #[arg(long = "env", value_parser = parse_exec_env_var)]
    env: Vec<GuestEnvVar>,
    /// Print VM launch details and guest bootstrap progress.
    #[arg(long)]
    verbose: bool,
    /// Command and arguments to run inside the guest.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "yolobox",
    about = "Launch branch-scoped Linux VMs for local development on macOS",
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
    /// Manage immutable base images used to create instances.
    Base(ClapBaseCommand),
    /// Create or reopen an instance and optionally enter it.
    Launch(LaunchOptions),
    /// Run a non-interactive command inside an instance over SSH.
    Exec(ExecOptions),
    /// Show instance metadata and whether the VM is currently running.
    Status(TargetOptions),
    /// Stop a running VM without deleting its state.
    Stop(TargetOptions),
    /// Check whether host prerequisites for the built-in VM runtime are installed.
    Doctor,
    /// List all known instances.
    List,
    /// Delete an instance and all of its state.
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
        let cli = match ClapCli::try_parse_from(parse_input.clone()) {
            Ok(cli) => cli,
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                ) =>
            {
                return Ok(Self {
                    command: Command::Help(render_help_for_args(&parse_input)),
                });
            }
            Err(err) => return Err(err.to_string()),
        };

        let command = match cli.command {
            Some(ClapCommand::Base(command)) => Command::Base(command.command),
            Some(ClapCommand::Launch(options)) => {
                Command::Launch(normalize_launch_options(options))
            }
            Some(ClapCommand::Exec(options)) => Command::Exec(options),
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
    if options.clear_shares && !options.shares.is_empty() {
        return Err("--clear-shares cannot be combined with --share".to_string());
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
    let existing_instance = state::find_instance(
        instance_name,
        options.repo.as_deref(),
        options.branch.as_deref(),
        resolved_base.as_deref(),
    )?;
    let requested_shares = build_requested_shares(existing_instance.as_ref(), &options)?;
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
    let host_bridge_mounts = match plan.mode {
        LaunchMode::External(_) | LaunchMode::Krunkit => host_bridge::managed_mounts(
            &instance,
            prepared_cloud_init
                .as_ref()
                .map(|prepared| prepared.hostname.as_str()),
        )?,
        LaunchMode::Shell => Vec::new(),
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
        host_bridge_mounts,
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

fn exec(options: ExecOptions) -> Result<(), String> {
    let plan = runtime::resolve_runtime(false);
    if !matches!(plan.mode, LaunchMode::Krunkit) {
        return Err(
            "yolobox exec currently supports only the built-in krunkit runtime".to_string(),
        );
    }

    let instance = state::find_instance(
        options.target.name.as_deref(),
        options.target.repo.as_deref(),
        options.target.branch.as_deref(),
        options.target.base.as_deref(),
    )?
    .ok_or_else(|| missing_instance_message(&options.target))?;
    let ssh_private_key_path = options
        .ssh_private_key
        .clone()
        .or_else(cloud_init::discover_ssh_private_key)
        .ok_or_else(missing_ssh_guidance)?;
    let vmnet = network::resolve_for_instance(&instance)?;
    let host_bridge_mounts = host_bridge::managed_mounts(&instance, None)?;
    let launch_config = LaunchConfig {
        require_vm: true,
        cpus: runtime::default_cpus(),
        memory_mib: runtime::default_memory_mib(),
        cloud_init_image: None,
        cloud_init_user: Some(options.user.clone()),
        hostname: None,
        ssh_pubkey_path: None,
        ssh_private_key_path: Some(ssh_private_key_path),
        init_script_path: None,
        host_bridge_mounts,
        shares: instance.shares.clone(),
        guest_env: instance.guest_env.clone(),
        verbose: options.verbose,
        vmnet: Some(vmnet),
    };
    let command = GuestExecCommand {
        cwd: options.cwd.clone(),
        env: normalize_exec_env(options.env),
        command: options.command[0].clone(),
        args: options.command[1..].to_vec(),
    };
    let code = runtime::exec(plan, &instance, &launch_config, &command)?;
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

fn parse_exec_env_var(spec: &str) -> Result<GuestEnvVar, String> {
    let (name, value) = spec
        .split_once('=')
        .ok_or_else(|| format!("invalid env var {spec}; expected NAME=VALUE"))?;
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return Err(format!("invalid env var {spec}; name cannot be empty"));
    }
    Ok(GuestEnvVar {
        name: trimmed_name.to_string(),
        value: value.to_string(),
    })
}

fn normalize_exec_env(mut vars: Vec<GuestEnvVar>) -> Vec<GuestEnvVar> {
    vars.sort_by(|left, right| left.name.cmp(&right.name));
    vars.dedup_by(|left, right| left.name == right.name);
    vars
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
        options.with_gh = true;
    }
    if options.no_ai {
        options.no_codex = true;
        options.no_claude = true;
        options.no_gh = true;
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

fn render_help_for_args(args: &[String]) -> String {
    let mut command = ClapCli::command();

    for arg in args.iter().skip(1) {
        if arg == "-h" || arg == "--help" {
            break;
        }
        let Some(subcommand) = command.find_subcommand_mut(arg) else {
            break;
        };
        command = subcommand.clone();
    }

    let mut output = Vec::new();
    command
        .write_long_help(&mut output)
        .expect("writing clap help should succeed");
    String::from_utf8(output).expect("clap help should be valid UTF-8")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharePreference {
    Disabled,
    Optional,
    Required,
}

fn build_requested_shares(
    existing_instance: Option<&state::Instance>,
    options: &LaunchOptions,
) -> Result<Option<Vec<ShareMount>>, String> {
    let mut shares = if options.clear_shares || !options.shares.is_empty() {
        options.shares.clone()
    } else {
        existing_instance
            .map(|instance| {
                instance
                    .shares
                    .iter()
                    .filter(|share| !is_ai_managed_share(share, &options.cloud_user))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    add_optional_or_required_share(
        &mut shares,
        ".codex",
        &options.cloud_user,
        codex_share_preference(options),
    )?;
    add_optional_or_required_share(
        &mut shares,
        ".claude",
        &options.cloud_user,
        claude_share_preference(options),
    )?;
    if gh_enabled(options) {
        if let Ok(share) = default_credential_share(".config/gh", &options.cloud_user) {
            shares.push(share);
        }
    }

    shares.sort();
    shares.dedup();

    if let Some(instance) = existing_instance {
        if shares == instance.shares {
            return Ok(None);
        }
    } else if shares.is_empty() {
        return Ok(None);
    }

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

    if claude_enabled(options) {
        if let Some(value) = env::var_os("ANTHROPIC_API_KEY") {
            vars.push(GuestEnvVar {
                name: "ANTHROPIC_API_KEY".to_string(),
                value: value
                    .into_string()
                    .map_err(|_| "ANTHROPIC_API_KEY is not valid UTF-8".to_string())?,
            });
        }
    }

    if gh_enabled(options) {
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

fn add_optional_or_required_share(
    shares: &mut Vec<ShareMount>,
    dotdir: &str,
    cloud_user: &str,
    preference: SharePreference,
) -> Result<(), String> {
    match preference {
        SharePreference::Disabled => Ok(()),
        SharePreference::Optional => {
            if let Ok(share) = default_credential_share(dotdir, cloud_user) {
                shares.push(share);
            }
            Ok(())
        }
        SharePreference::Required => {
            shares.push(default_credential_share(dotdir, cloud_user)?);
            Ok(())
        }
    }
}

fn codex_share_preference(options: &LaunchOptions) -> SharePreference {
    share_preference(options.with_codex, options.no_ai || options.no_codex)
}

fn claude_share_preference(options: &LaunchOptions) -> SharePreference {
    share_preference(options.with_claude, options.no_ai || options.no_claude)
}

fn share_preference(explicit_enable: bool, explicit_disable: bool) -> SharePreference {
    if explicit_disable {
        SharePreference::Disabled
    } else if explicit_enable {
        SharePreference::Required
    } else {
        SharePreference::Optional
    }
}

fn claude_enabled(options: &LaunchOptions) -> bool {
    claude_share_preference(options) != SharePreference::Disabled
}

fn gh_enabled(options: &LaunchOptions) -> bool {
    !(options.no_ai || options.no_gh)
}

fn is_ai_managed_share(share: &ShareMount, cloud_user: &str) -> bool {
    [
        format!("/home/{cloud_user}/.codex"),
        format!("/home/{cloud_user}/.claude"),
        format!("/home/{cloud_user}/.config/gh"),
    ]
    .iter()
    .any(|path| share.guest_path == PathBuf::from(path))
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
        BaseCommand, Cli, Command, SharePreference, claude_share_preference,
        codex_share_preference, confirm_destroy_answer, gh_enabled, missing_base_guidance,
        missing_ssh_guidance, missing_vm_runtime_guidance, parse_exec_env_var, render_help,
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
    fn ai_integrations_are_enabled_by_default() {
        let cli = Cli::parse(vec!["--base".to_string(), "ubuntu".to_string()])
            .expect("parse should succeed");

        match cli.command {
            Command::Launch(options) => {
                assert_eq!(codex_share_preference(&options), SharePreference::Optional);
                assert_eq!(claude_share_preference(&options), SharePreference::Optional);
                assert!(gh_enabled(&options));
            }
            _ => panic!("expected launch command"),
        }
    }

    #[test]
    fn ai_integrations_can_be_opted_out_individually() {
        let cli = Cli::parse(vec![
            "--base".to_string(),
            "ubuntu".to_string(),
            "--no-codex".to_string(),
            "--no-claude".to_string(),
            "--no-gh".to_string(),
        ])
        .expect("parse should succeed");

        match cli.command {
            Command::Launch(options) => {
                assert_eq!(codex_share_preference(&options), SharePreference::Disabled);
                assert_eq!(claude_share_preference(&options), SharePreference::Disabled);
                assert!(!gh_enabled(&options));
            }
            _ => panic!("expected launch command"),
        }
    }

    #[test]
    fn no_ai_disables_all_integrations() {
        let cli = Cli::parse(vec![
            "--base".to_string(),
            "ubuntu".to_string(),
            "--no-ai".to_string(),
        ])
        .expect("parse should succeed");

        match cli.command {
            Command::Launch(options) => {
                assert_eq!(codex_share_preference(&options), SharePreference::Disabled);
                assert_eq!(claude_share_preference(&options), SharePreference::Disabled);
                assert!(!gh_enabled(&options));
            }
            _ => panic!("expected launch command"),
        }
    }

    #[test]
    fn help_mentions_public_ai_and_launch_flags() {
        let help = render_help();
        assert!(help.contains("--no-ai"));
        assert!(help.contains("--no-codex"));
        assert!(help.contains("--no-claude"));
        assert!(help.contains("--no-gh"));
        assert!(help.contains("--no-enter"));
        assert!(help.contains("Prepare the instance and exit before launching or attaching to it"));
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
    fn exec_subcommand_parses_command_and_env() {
        let cli = Cli::parse(vec![
            "exec".to_string(),
            "--name".to_string(),
            "repo-main".to_string(),
            "--env".to_string(),
            "CODEX_HOME=/workspace/.codex".to_string(),
            "--cwd".to_string(),
            "/workspace".to_string(),
            "--".to_string(),
            "codex".to_string(),
            "app-server".to_string(),
        ])
        .expect("parse should succeed");

        match cli.command {
            Command::Exec(options) => {
                assert_eq!(options.target.name.as_deref(), Some("repo-main"));
                assert_eq!(options.cwd.as_deref(), Some("/workspace"));
                assert_eq!(options.command, vec!["codex".to_string(), "app-server".to_string()]);
                assert_eq!(options.env.len(), 1);
                assert_eq!(options.env[0].name, "CODEX_HOME");
                assert_eq!(options.env[0].value, "/workspace/.codex");
            }
            _ => panic!("expected exec command"),
        }
    }

    #[test]
    fn exec_env_var_requires_name_value_syntax() {
        assert!(parse_exec_env_var("CODEX_HOME=/workspace/.codex").is_ok());
        assert!(parse_exec_env_var("broken").is_err());
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
