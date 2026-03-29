use crate::auth;
use crate::git;
use crate::sprites_api::{CreateSpriteRequest, SpritesClient};
use crate::state;
use clap::error::ErrorKind;
use clap::{ArgGroup, Args, CommandFactory, Parser, Subcommand};
use std::env;
use std::io::{self, IsTerminal, Write};
use std::process::Command as ProcessCommand;

pub async fn run() -> Result<(), String> {
    // Ensure terminal is in a sane state (previous run may have crashed in raw mode)
    let _ = crossterm::terminal::disable_raw_mode();

    // Restore terminal on panic
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        default_hook(info);
    }));

    let cli = Cli::parse_args(env::args().skip(1).collect())?;

    match cli.command {
        Command::Launch(options) => launch(options).await,
        Command::Exec(options) => exec_command(options).await,
        Command::Auth(cmd) => auth_command(cmd).await,
        Command::List => list_sprites().await,
        Command::Stop(target) => stop(target).await,
        Command::Destroy(options) => destroy(options).await,
        Command::Doctor => doctor().await,
        Command::Help(text) => {
            print!("{text}");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// CLI types
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Command {
    Launch(LaunchOptions),
    Exec(ExecOptions),
    Auth(AuthCommand),
    List,
    Stop(TargetOptions),
    Destroy(DestroyOptions),
    Doctor,
    Help(String),
}

#[derive(Clone, Debug, Subcommand)]
enum AuthCommand {
    /// Authenticate with Fly.io and store a Sprites API token.
    Login {
        /// Fly.io organization slug (default: "personal").
        #[arg(long, default_value = "personal")]
        org: String,
    },
    /// Store a Claude Code OAuth token for use in sprites.
    SetupClaude,
    /// Store an OpenAI API key for Codex in sprites.
    SetupCodex,
    /// Show current authentication status.
    Status,
    /// Remove stored authentication.
    Logout,
}

#[derive(Debug)]
struct Cli {
    command: Command,
}

#[derive(Clone, Debug, Args)]
#[command(group(
    ArgGroup::new("launch_selector")
        .required(false)
        .args(["name", "repo"])
))]
struct LaunchOptions {
    /// Launch a standalone sprite with this explicit name.
    #[arg(long, conflicts_with_all = ["repo", "branch"])]
    name: Option<String>,
    /// Git remote URL to clone inside the sprite.
    #[arg(long, conflicts_with = "name")]
    repo: Option<String>,
    /// Branch to check out inside the sprite.
    #[arg(long, requires = "repo")]
    branch: Option<String>,
    /// Guest username to use inside the sprite.
    #[arg(long = "user", default_value_t = default_user())]
    user: String,
    /// Do not install Claude Code in the sprite.
    #[arg(long)]
    no_claude: bool,
    /// Do not install Codex in the sprite.
    #[arg(long)]
    no_codex: bool,
    /// Print verbose output.
    #[arg(long)]
    verbose: bool,
}

#[derive(Clone, Debug, Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .args(["name", "repo"])
))]
struct TargetOptions {
    /// Target a standalone sprite by name.
    #[arg(long, conflicts_with_all = ["repo", "branch"])]
    name: Option<String>,
    /// Target the sprite identified by this repo.
    #[arg(long, requires = "branch")]
    repo: Option<String>,
    /// Target the sprite identified by this branch.
    #[arg(long, requires = "repo")]
    branch: Option<String>,
}

#[derive(Clone, Debug, Args)]
struct ExecOptions {
    #[command(flatten)]
    target: TargetOptions,
    /// Print verbose output.
    #[arg(long)]
    verbose: bool,
    /// Command to run inside the sprite.
    #[arg(last = true, required = true)]
    cmd: Vec<String>,
}

#[derive(Clone, Debug, Args)]
struct DestroyOptions {
    #[command(flatten)]
    target: TargetOptions,
    /// Skip the interactive confirmation prompt.
    #[arg(long = "yes", short = 'y')]
    yes: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "spritebox",
    about = "Branch-scoped development environments powered by Fly.io Sprites",
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
    /// Create or reopen a sprite and enter it.
    Launch(LaunchOptions),
    /// Run a command inside a sprite.
    Exec(ExecOptions),
    /// Manage authentication.
    Auth(ClapAuthCommand),
    /// List all sprites.
    List,
    /// Stop a running sprite.
    Stop(TargetOptions),
    /// Delete a sprite.
    Destroy(DestroyOptions),
    /// Check prerequisites.
    Doctor,
    Help,
}

#[derive(Debug, Args)]
struct ClapAuthCommand {
    #[command(subcommand)]
    command: AuthCommand,
}

impl Cli {
    fn parse_args(args: Vec<String>) -> Result<Self, String> {
        let parse_input = std::iter::once("spritebox".to_string())
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
            Some(ClapCommand::Launch(options)) => Command::Launch(options),
            Some(ClapCommand::Exec(options)) => Command::Exec(options),
            Some(ClapCommand::Auth(cmd)) => Command::Auth(cmd.command),
            Some(ClapCommand::List) => Command::List,
            Some(ClapCommand::Stop(target)) => Command::Stop(target),
            Some(ClapCommand::Destroy(options)) => Command::Destroy(options),
            Some(ClapCommand::Doctor) => Command::Doctor,
            Some(ClapCommand::Help) => Command::Help(render_help()),
            None => Command::Launch(cli.launch),
        };

        Ok(Self { command })
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async fn exec_command(options: ExecOptions) -> Result<(), String> {
    let mut client = create_client()?;
    client.set_verbose(options.verbose);

    let sprite_name = state::sprite_name(
        options.target.name.as_deref(),
        options.target.repo.as_deref(),
        options.target.branch.as_deref(),
    )?;

    let cmd_strs: Vec<&str> = options.cmd.iter().map(|s| s.as_str()).collect();
    let result = client.exec(&sprite_name, &cmd_strs, &[], None).await?;

    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }

    if result.exit_code != 0 {
        std::process::exit(result.exit_code);
    }
    Ok(())
}

async fn launch(options: LaunchOptions) -> Result<(), String> {
    let options = resolve_launch_selector(options)?;
    let mut client = create_client()?;
    client.set_verbose(options.verbose);

    let sprite_name = state::sprite_name(
        options.name.as_deref(),
        options.repo.as_deref(),
        options.branch.as_deref(),
    )?;

    // Check if sprite already exists
    let existing = client.get_sprite(&sprite_name).await?;
    if let Some(ref info) = existing {
        if options.verbose {
            eprintln!("sprite {sprite_name} exists (status: {})", info.status);
        }
    } else {
        eprintln!("creating sprite {sprite_name}...");
        client
            .create_sprite(&CreateSpriteRequest {
                name: sprite_name.clone(),
                config: Some(crate::sprites_api::SpriteConfig {
                    cpus: Some(4),
                    ram_mb: Some(8192),
                    region: None,
                    storage_gb: None,
                }),
                environment: None,
            })
            .await?;
        eprintln!("sprite {sprite_name} created");
    }

    // Wait for sprite to be ready (wakes cold sprites via HTTP exec poke)
    wait_for_sprite(&client, &sprite_name).await?;

    // Ensure user exists (idempotent -- safe to run every launch)
    if options.verbose {
        eprintln!("setting up user {user}...", user = options.user);
    }
    let setup_cmds = format!(
        "apt-get install -y -qq ncurses-base > /dev/null 2>&1; \
         id -u {user} >/dev/null 2>&1 || useradd -m -s /bin/bash -G sudo {user}; \
         echo '{user} ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/{user} \
         && chown root:root /etc/sudoers.d/{user} && chmod 0440 /etc/sudoers.d/{user}; \
         chmod a+r /etc/profile.d/languages_* 2>/dev/null; \
         touch /etc/profile.d/languages_env /etc/profile.d/languages_paths; \
         chmod a+rw /etc/profile.d/languages_env /etc/profile.d/languages_paths || true",
        user = options.user
    );
    let result = client
        .exec(&sprite_name, &["bash", "-c", &setup_cmds], &[], None)
        .await?;
    if result.exit_code != 0 {
        return Err(format!(
            "user setup failed (exit {}): {}",
            result.exit_code,
            result.stderr.trim()
        ));
    }

    // Write profile.d script to fix PATH for non-sprite users.
    // For non-sprite users, override CARGO_HOME/RUSTUP_HOME to user-local dirs
    // and add the real rustup toolchain binaries to PATH (bypassing /.sprite/bin wrappers
    // which assume CARGO_HOME points to the sprite user's cargo dir).
    let sprite_path_sh = r#"export PATH="/home/sprite/.local/bin:/.sprite/bin:$PATH"
if [ "$(id -un)" != "sprite" ]; then
  export CARGO_HOME="$HOME/.cargo"
  export RUSTUP_HOME="$HOME/.rustup"
  # Find the real rustc/cargo from the sprite's rustup toolchain
  for d in /.sprite/languages/rust/rustup/toolchains/*/bin; do
    [ -d "$d" ] && export PATH="$d:$PATH" && break
  done
fi
"#;
    client
        .exec_with_stdin(
            &sprite_name,
            &["bash", "-c", "cat > /etc/profile.d/sprite-path.sh && chmod a+r /etc/profile.d/sprite-path.sh"],
            &[], None,
            sprite_path_sh.as_bytes(),
        )
        .await?;

    if options.verbose {
        eprintln!("user setup done");
    }

    // Install bridge shim scripts
    if options.verbose {
        eprintln!("installing bridge scripts...");
    }
    install_bridge_scripts(&client, &sprite_name).await?;

    // Clone repo if needed
    if let (Some(repo), Some(branch)) = (options.repo.as_deref(), options.branch.as_deref()) {
        let gh_token = host_gh_auth_token()?;
        let mut env_vars: Vec<(&str, &str)> = Vec::new();
        if let Some(ref token) = gh_token {
            env_vars.push(("GH_TOKEN", token));
        }

        // Check if repo already cloned
        let check = client
            .exec(
                &sprite_name,
                &["test", "-d", "/workspace/.git"],
                &env_vars,
                None,
            )
            .await?;

        if check.exit_code != 0 {
            eprintln!("cloning {repo} (branch: {branch})...");

            // Convert SSH URL to HTTPS if we have GH_TOKEN
            let clone_url = if gh_token.is_some() {
                ssh_to_https(repo)
            } else {
                repo.to_string()
            };

            // Set up git credential helper for GH_TOKEN if available
            let clone_script = if gh_token.is_some() {
                format!(
                    concat!(
                        "git config --global credential.helper '!f() {{ echo \"password=$GH_TOKEN\"; }}; f' && ",
                        "git clone --branch {branch} {url} /workspace && chown -R {user}:{user} /workspace"
                    ),
                    branch = shell_escape(branch),
                    url = shell_escape(&clone_url),
                    user = options.user,
                )
            } else {
                format!(
                    "git clone --branch {branch} {url} /workspace && chown -R {user}:{user} /workspace",
                    branch = shell_escape(branch),
                    url = shell_escape(repo),
                    user = options.user,
                )
            };

            let result = client
                .exec(&sprite_name, &["bash", "-c", &clone_script], &env_vars, None)
                .await?;
            if result.exit_code != 0 {
                return Err(format!(
                    "git clone failed (exit {}):\n{}{}",
                    result.exit_code,
                    result.stdout,
                    result.stderr
                ));
            }
            eprintln!("repo cloned to /workspace");
        } else if options.verbose {
            eprintln!("repo already cloned at /workspace");
        }
    }

    // Fetch sprite URL for the session
    let sprite_url = client
        .get_sprite(&sprite_name)
        .await?
        .and_then(|info| info.url);

    // Build env vars for the session
    let mut session_env: Vec<(String, String)> = Vec::new();
    if let Some(token) = host_gh_auth_token()? {
        session_env.push(("GH_TOKEN".to_string(), token));
    }
    if let Some(token) = auth::load_claude_token() {
        session_env.push(("CLAUDE_CODE_OAUTH_TOKEN".to_string(), token));
    } else if let Ok(key) = env::var("ANTHROPIC_API_KEY") {
        session_env.push(("ANTHROPIC_API_KEY".to_string(), key));
    }
    if let Some(key) = auth::load_openai_key() {
        session_env.push(("OPENAI_API_KEY".to_string(), key));
    } else if let Ok(key) = env::var("OPENAI_API_KEY") {
        session_env.push(("OPENAI_API_KEY".to_string(), key));
    }
    if let Some(name) = host_git_config("user.name")? {
        session_env.push(("GIT_AUTHOR_NAME".to_string(), name.clone()));
        session_env.push(("GIT_COMMITTER_NAME".to_string(), name));
    }
    if let Some(email) = host_git_config("user.email")? {
        session_env.push(("GIT_AUTHOR_EMAIL".to_string(), email.clone()));
        session_env.push(("GIT_COMMITTER_EMAIL".to_string(), email));
    }
    session_env.push(("TERM".to_string(), env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string())));
    // Pass sprites API token so the in-VM `sprite` CLI works
    if let Some(token) = auth::load_token() {
        session_env.push(("SPRITE_TOKEN".to_string(), token));
    }
    // Sprite URL and name for LLM context
    session_env.push(("SPRITE_NAME".to_string(), sprite_name.clone()));
    if let Some(ref url) = sprite_url {
        session_env.push(("SPRITE_URL".to_string(), url.clone()));
    }

    // Write env vars to sprite so they survive sudo
    if options.verbose {
        eprintln!("writing session env ({} vars)...", session_env.len());
    }
    setup_session_env(&client, &sprite_name, &options.user, &session_env).await?;

    // Configure git credentials for the user (gh uses GH_TOKEN from env)
    if host_gh_auth_token()?.is_some() {
        let gh_setup = format!(
            "sudo -u {user} -i bash -c 'gh auth setup-git'",
            user = options.user
        );
        let _ = client.exec(&sprite_name, &["bash", "-c", &gh_setup], &[], None).await;
    }

    // Sync config directories from host
    if options.verbose {
        eprintln!("syncing host configs...");
    }
    sync_host_configs(&client, &sprite_name, &options).await?;

    // Write spritebox skills doc for LLMs
    if options.verbose {
        eprintln!("installing skills doc...");
    }
    install_skills_doc(
        &client,
        &sprite_name,
        &options.user,
        sprite_url.as_deref(),
    )
    .await?;

    // Open interactive console
    let dir = if options.repo.is_some() {
        Some("/workspace")
    } else {
        None
    };
    eprintln!("connecting to {sprite_name}...");
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let shell = format!(
        "exec sudo -u {user} -i bash -c 'stty rows {rows} cols {cols} 2>/dev/null; export COLUMNS={cols} LINES={rows}; cd {dir} && exec bash --login'",
        user = options.user,
        rows = term_rows,
        cols = term_cols,
        dir = if options.repo.is_some() { "/workspace" } else { "~" },
    );
    let env_refs: Vec<(&str, &str)> = session_env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let code = client
        .console(&sprite_name, &["bash", "-c", &shell], &env_refs, dir)
        .await?;

    // 130 = SIGINT (Ctrl-C), normal for interactive shells
    if code != 0 && code != 130 {
        return Err(format!("session exited with code {code}"));
    }
    Ok(())
}

async fn auth_command(cmd: AuthCommand) -> Result<(), String> {
    match cmd {
        AuthCommand::Login { org } => {
            eprintln!("getting Fly.io token via `fly auth token`...");
            let fly_token = auth::fly_auth_token()?;

            eprintln!("exchanging for Sprites API token (org: {org})...");
            let sprites_token = SpritesClient::exchange_fly_token(&fly_token, &org).await?;

            auth::save_token(&sprites_token, &org)?;
            eprintln!("authenticated. Token stored.");

            // Verify it works
            let client = SpritesClient::new(sprites_token)?;
            let list = client.list_sprites().await?;
            eprintln!("connected ({} existing sprites)", list.sprites.len());
            Ok(())
        }
        AuthCommand::SetupClaude => {
            eprintln!("Run `claude setup-token` and paste the token here.");
            let token = rpassword::prompt_password("Claude OAuth token: ")
                .map_err(|e| format!("failed to read token: {e}"))?;
            let token = token.trim().to_string();
            if token.is_empty() {
                return Err("no token provided".to_string());
            }
            auth::save_claude_token(&token)?;
            println!("Claude token stored.");
            Ok(())
        }
        AuthCommand::SetupCodex => {
            let key = rpassword::prompt_password("OpenAI API key: ")
                .map_err(|e| format!("failed to read key: {e}"))?;
            let key = key.trim().to_string();
            if key.is_empty() {
                return Err("no key provided".to_string());
            }
            auth::save_openai_key(&key)?;
            println!("OpenAI key stored.");
            Ok(())
        }
        AuthCommand::Status => {
            // Show Fly.io identity
            match fly_whoami() {
                Some(email) => println!("fly.io: {email}"),
                None => println!("fly.io: not logged in (run `fly auth login`)"),
            }

            match auth::load_token() {
                Some(_) => {
                    let org = auth::load_org().unwrap_or_else(|| "unknown".to_string());
                    println!("sprites: authenticated (org: {org})");

                    // Try to validate
                    let client = create_client()?;
                    match client.list_sprites().await {
                        Ok(list) => println!("api: ok ({} sprites)", list.sprites.len()),
                        Err(e) => println!("api: error ({e})"),
                    }
                }
                None => {
                    println!("sprites: not authenticated. Run `spritebox auth login`.");
                }
            }
            println!(
                "claude: {}",
                if auth::load_claude_token().is_some() { "ok" } else { "not configured (run `spritebox auth setup-claude`)" }
            );
            println!(
                "codex: {}",
                if auth::load_openai_key().is_some() { "ok" } else { "not configured (run `spritebox auth setup-codex`)" }
            );
            Ok(())
        }
        AuthCommand::Logout => {
            if auth::remove_token()? {
                println!("logged out. Stored token removed.");
            } else {
                println!("no stored token found.");
            }
            Ok(())
        }
    }
}

async fn list_sprites() -> Result<(), String> {
    let client = create_client()?;
    let list = client.list_sprites().await?;

    if list.sprites.is_empty() {
        println!("no sprites");
        return Ok(());
    }

    for sprite in &list.sprites {
        println!(
            "{}  {}  {}",
            sprite.name,
            sprite.status,
            sprite.url.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

async fn stop(target: TargetOptions) -> Result<(), String> {
    let client = create_client()?;
    let sprite_name = state::sprite_name(
        target.name.as_deref(),
        target.repo.as_deref(),
        target.branch.as_deref(),
    )?;

    let existing = client.get_sprite(&sprite_name).await?;
    match existing {
        None => {
            println!("no sprite named {sprite_name}");
        }
        Some(info) if info.status == "stopped" || info.status == "sleeping" => {
            println!("{sprite_name} is already stopped");
        }
        Some(_) => {
            client.stop_sprite(&sprite_name).await?;
            println!("stopped {sprite_name}");
        }
    }
    Ok(())
}

async fn destroy(options: DestroyOptions) -> Result<(), String> {
    let client = create_client()?;
    let sprite_name = state::sprite_name(
        options.target.name.as_deref(),
        options.target.repo.as_deref(),
        options.target.branch.as_deref(),
    )?;

    let existing = client.get_sprite(&sprite_name).await?;
    if existing.is_none() {
        println!("no sprite named {sprite_name}");
        return Ok(());
    }

    if !options.yes && !confirm_destroy(&sprite_name)? {
        println!("aborted");
        return Ok(());
    }

    client.delete_sprite(&sprite_name).await?;
    println!("destroyed {sprite_name}");
    Ok(())
}

async fn doctor() -> Result<(), String> {
    let token = auth::load_token();
    let token_ok = token.is_some();
    let org = auth::load_org().unwrap_or_else(|| "-".to_string());

    println!(
        "auth: {}",
        if token_ok {
            format!("ok (org: {org})")
        } else {
            "missing (run `spritebox auth login`)".to_string()
        }
    );

    if let Some(token) = token {
        match SpritesClient::new(token) {
            Ok(client) => match client.list_sprites().await {
                Ok(list) => println!("api: ok ({} sprites)", list.sprites.len()),
                Err(e) => println!("api: error ({e})"),
            },
            Err(e) => println!("api: error ({e})"),
        }
    }

    let gh = host_gh_auth_token().unwrap_or(None);
    println!(
        "gh_token: {}",
        if gh.is_some() {
            "ok (gh auth token)"
        } else {
            "missing (install gh CLI and run gh auth login)"
        }
    );

    let git_name = host_git_config("user.name").unwrap_or(None);
    let git_email = host_git_config("user.email").unwrap_or(None);
    println!(
        "git_user: {}",
        match (git_name, git_email) {
            (Some(name), Some(email)) => format!("ok ({name} <{email}>)"),
            _ => "missing (git config --global user.name/user.email)".to_string(),
        }
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn setup_session_env(
    client: &SpritesClient,
    sprite_name: &str,
    user: &str,
    env_vars: &[(String, String)],
) -> Result<(), String> {
    if env_vars.is_empty() {
        return Ok(());
    }

    // Build env file content
    let mut content = String::new();
    for (k, v) in env_vars {
        let escaped = v.replace('\'', "'\\''");
        content.push_str(&format!("export {k}='{escaped}'\n"));
    }

    // Write env file via stdin pipe, then source from .bashrc
    let user_home = format!("/home/{user}");
    let cmd = format!(
        concat!(
            "cat > {home}/.spritebox_env",
            " && chown {user}:{user} {home}/.spritebox_env",
            " && chmod 0600 {home}/.spritebox_env",
            " && for rc in {home}/.bashrc {home}/.profile; do",
            " grep -q spritebox_env \"$rc\" 2>/dev/null",
            " || printf '\\n[ -f ~/.spritebox_env ] && . ~/.spritebox_env\\n' >> \"$rc\";",
            " done",
        ),
        home = user_home,
        user = user,
    );

    let result = client
        .exec_with_stdin(sprite_name, &["bash", "-c", &cmd], &[], None, content.as_bytes())
        .await?;
    if result.exit_code != 0 {
        return Err(format!(
            "env setup failed (exit {}): {}",
            result.exit_code,
            result.stderr.trim()
        ));
    }

    Ok(())
}

async fn sync_host_configs(
    client: &SpritesClient,
    sprite_name: &str,
    options: &LaunchOptions,
) -> Result<(), String> {
    let home = env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let home_path = std::path::Path::new(&home);

    // Collect individual files to sync (only auth/config, not caches/history)
    let mut paths: Vec<String> = Vec::new();
    if !options.no_claude {
        for f in [
            ".claude.json",
            ".claude/.credentials.json",
            ".claude/settings.json",
            ".claude/CLAUDE.md",
        ] {
            if home_path.join(f).exists() {
                paths.push(f.to_string());
            }
        }
    }
    if !options.no_codex {
        for f in [
            ".codex/auth.json",
            ".codex/config.toml",
            ".codex/rules",
        ] {
            if home_path.join(f).exists() {
                paths.push(f.to_string());
            }
        }
    }

    if paths.is_empty() {
        return Ok(());
    }

    eprintln!("syncing config...");

    // Create tarball on host (raw bytes, no base64)
    let tar_cmd = format!(
        "tar czf - --no-mac-metadata --exclude='._*' -C {} {}",
        shell_escape(&home),
        paths.iter().map(|p| shell_escape(p)).collect::<Vec<_>>().join(" "),
    );
    let output = ProcessCommand::new("bash")
        .args(["-c", &tar_cmd])
        .output()
        .map_err(|e| format!("failed to create config archive: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("failed to archive configs: {stderr}"));
    }

    // Extract on sprite via stdin pipe
    let user_home = format!("/home/{}", options.user);
    let extract_cmd = format!(
        "mkdir -p {home}/.claude && tar xzf - -C {home} && chown -R {user}:{user} {home}/.claude {home}/.claude.json {home}/.codex 2>/dev/null; true",
        home = shell_escape(&user_home),
        user = options.user,
    );

    let result = client
        .exec_with_stdin(sprite_name, &["bash", "-c", &extract_cmd], &[], None, &output.stdout)
        .await?;
    if result.exit_code != 0 {
        return Err(format!(
            "config sync failed (exit {}): {}",
            result.exit_code,
            result.stderr.trim()
        ));
    }

    Ok(())
}

async fn install_skills_doc(
    client: &SpritesClient,
    sprite_name: &str,
    user: &str,
    sprite_url: Option<&str>,
) -> Result<(), String> {
    let url_line = match sprite_url {
        Some(url) => format!("- Your sprite's public URL is: {url}\n"),
        None => String::new(),
    };

    let doc = format!(
        r#"# Spritebox Environment

You are running inside a Fly.io Sprite — a remote Linux microVM managed by spritebox.
The host machine is macOS.

## Sprite Info

- Sprite name: {sprite_name}
{url_line}- Run `sprite-env info` to get your sprite's name, URL, and version.
- The URL is also available as `$SPRITE_URL` in your environment.

## Opening Files on the Host

To open a file on the host machine (in its default application):
```
sprite-open <path>
```
The host user will see a confirmation dialog before the file is downloaded and opened.
Allowed file types: html, htm, svg, png, jpg, jpeg, pdf, md, txt, rtf, csv.

## Opening URLs on the Host

To open a URL in the host's browser, run:
```
sprite-browser <url>
```
Do NOT use `open`, `xdg-open`, or any other command — they will not work.
`sprite-browser` is the ONLY way to open URLs on the host from inside this VM.

## Clipboard Image Import

To import a screenshot or image from the host's clipboard:
```
spritebox-paste-image <absolute-destination-path>
```
The host user will see a confirmation dialog before the image is transferred.

## Services and Checkpoints

Sprites have built-in service management and checkpoints. See:
- `/.sprite/llm.txt` — quick reference for sprite capabilities
- `sprite-env services --help` — manage long-running services
- `sprite-env checkpoints --help` — create and restore checkpoints

## HTTP Services

When you start an HTTP service, it's accessible via the sprite's public URL
(get it from `sprite-env info` or `$SPRITE_URL`). By default the proxy routes
to port 8080. Use `sprite-env services create` with `--http-port` to route to
a different port. To open the URL on the host, use `sprite-browser $SPRITE_URL`.
Never tell the user to visit `localhost` — use the sprite URL instead.

## Important

- This is a Linux VM, not macOS. GUI tools don't work here.
- Do not use `pbpaste`, `xclip`, or other clipboard tools — use `spritebox-paste-image`.
- To open a file on the host, use `sprite-open <path>` — do NOT use `open` or `xdg-open`.
- Files under `/workspace` are the project workspace.
- See `/.sprite/docs/agent-context.md` for full environment documentation.
- See `/.sprite/llm.txt` for a quick reference of all sprite capabilities.
"#,
        sprite_name = sprite_name,
        url_line = url_line,
    );

    let user_home = format!("/home/{user}");

    // Write the skills doc to /spritebox/skills.md
    let write_cmd =
        "mkdir -p /spritebox && cat > /spritebox/skills.md && chmod 644 /spritebox/skills.md";
    client
        .exec_with_stdin(
            sprite_name,
            &["bash", "-c", write_cmd],
            &[],
            None,
            doc.as_bytes(),
        )
        .await?;

    // Write spritebox section to ~/.claude/CLAUDE.md idempotently.
    // Strip any existing spritebox block, then append the fresh one.
    let claude_cmd = format!(
        concat!(
            "mkdir -p {home}/.claude && ",
            "touch {home}/.claude/CLAUDE.md && ",
            "sed -i '/<!-- spritebox environment -->/,/<!-- end spritebox -->/d' {home}/.claude/CLAUDE.md && ",
            "cat >> {home}/.claude/CLAUDE.md && ",
            "chown -R {user}:{user} {home}/.claude",
        ),
        home = user_home,
        user = user,
    );
    let mut claude_content = Vec::new();
    claude_content.extend_from_slice(b"\n<!-- spritebox environment -->\n");
    claude_content.extend_from_slice(doc.as_bytes());
    claude_content.extend_from_slice(b"<!-- end spritebox -->\n");
    client
        .exec_with_stdin(
            sprite_name,
            &["bash", "-c", &claude_cmd],
            &[],
            None,
            &claude_content,
        )
        .await?;

    // Write codex instructions to ~/.codex/instructions.md
    let codex_cmd = format!(
        "mkdir -p {home}/.codex && cat > {home}/.codex/instructions.md && chown -R {user}:{user} {home}/.codex",
        home = user_home,
        user = user,
    );
    client
        .exec_with_stdin(
            sprite_name,
            &["bash", "-c", &codex_cmd],
            &[],
            None,
            doc.as_bytes(),
        )
        .await?;

    Ok(())
}

async fn install_bridge_scripts(
    client: &SpritesClient,
    sprite_name: &str,
) -> Result<(), String> {
    let paste_image_script = r#"#!/bin/bash
# spritebox-paste-image — request host clipboard image via bridge escape sequence
if [ "$#" -ne 1 ]; then
  echo "usage: spritebox-paste-image <absolute-destination-path>" >&2
  exit 2
fi
dest="$1"
case "$dest" in
  /*) ;;
  *) echo "path must be absolute" >&2; exit 2 ;;
esac
emit_escape() {
  printf '\033]9999;paste-image;%s\033\\' "$dest" > "$1"
}
# Emit OSC 9999 escape sequence — try /dev/tty first, fall back to walking
# the process tree to find the session's PTY.
sent=false
if ( emit_escape /dev/tty ) 2>/dev/null; then
  sent=true
else
  pid=$$
  while [ "$pid" != "1" ] && [ -n "$pid" ]; do
    tty_dev=$(readlink /proc/"$pid"/fd/1 2>/dev/null || true)
    case "$tty_dev" in
      /dev/pts/*|/dev/tty*)
        emit_escape "$tty_dev"
        sent=true
        break
        ;;
    esac
    pid=$(awk '{print $4}' /proc/"$pid"/stat 2>/dev/null || echo 1)
  done
fi
if [ "$sent" = false ]; then
  echo "error: no TTY found to emit bridge escape sequence" >&2
  exit 1
fi
# Wait for the host to push the file (up to 30s)
waited=0
while [ ! -f "$dest" ] && [ "$waited" -lt 30 ]; do
  sleep 1
  waited=$((waited + 1))
done
if [ -f "$dest" ]; then
  exit 0
else
  echo "error: timed out waiting for clipboard image (30s)" >&2
  exit 1
fi
"#;
    client
        .exec_with_stdin(
            sprite_name,
            &["bash", "-c", "cat > /usr/local/bin/spritebox-paste-image && chmod +x /usr/local/bin/spritebox-paste-image"],
            &[],
            None,
            paste_image_script.as_bytes(),
        )
        .await?;

    // sprite-open — open a file from the sprite on the host machine
    let open_script = r#"#!/bin/bash
# sprite-open — open a file on the host machine via bridge escape sequence
if [ "$#" -ne 1 ]; then
  echo "usage: sprite-open <absolute-path>" >&2
  exit 2
fi
path="$1"
case "$path" in
  /*) ;;
  *) path="$(cd "$(dirname "$path")" 2>/dev/null && pwd)/$(basename "$path")" ;;
esac
if [ ! -f "$path" ]; then
  echo "error: file not found: $path" >&2
  exit 1
fi
emit_escape() {
  printf '\033]9999;open;%s\033\\' "$path" > "$1"
}
sent=false
if ( emit_escape /dev/tty ) 2>/dev/null; then
  sent=true
else
  pid=$$
  while [ "$pid" != "1" ] && [ -n "$pid" ]; do
    tty_dev=$(readlink /proc/"$pid"/fd/1 2>/dev/null || true)
    case "$tty_dev" in
      /dev/pts/*|/dev/tty*)
        emit_escape "$tty_dev"
        sent=true
        break
        ;;
    esac
    pid=$(awk '{print $4}' /proc/"$pid"/stat 2>/dev/null || echo 1)
  done
fi
if [ "$sent" = false ]; then
  echo "error: no TTY found to emit bridge escape sequence" >&2
  exit 1
fi
echo "opening $path on host..."
"#;
    client
        .exec_with_stdin(
            sprite_name,
            &["bash", "-c", "cat > /usr/local/bin/sprite-open && chmod +x /usr/local/bin/sprite-open"],
            &[],
            None,
            open_script.as_bytes(),
        )
        .await?;

    Ok(())
}

async fn install_tools(
    client: &SpritesClient,
    sprite_name: &str,
    options: &LaunchOptions,
) -> Result<(), String> {
    let mut packages: Vec<&str> = Vec::new();
    if !options.no_claude {
        packages.push("@anthropic-ai/claude-code");
    }
    if !options.no_codex {
        packages.push("@openai/codex");
    }
    if packages.is_empty() {
        return Ok(());
    }

    // Check which packages are already installed (verify via npm list, not just which)
    let mut to_install: Vec<&str> = Vec::new();
    for pkg in &packages {
        let check_cmd = format!("npm list -g {pkg} 2>/dev/null | grep -q {pkg}");
        let check = client
            .exec(sprite_name, &["bash", "-c", &check_cmd], &[], None)
            .await?;
        if check.exit_code != 0 {
            to_install.push(pkg);
        }
    }

    if to_install.is_empty() {
        if options.verbose {
            eprintln!("dev tools already installed");
        }
        return Ok(());
    }

    // Ensure a modern Node.js is available (claude-code needs 18+)
    let need_node = {
        let check = client
            .exec(sprite_name, &["bash", "-c", "node --version 2>/dev/null | sed 's/v//'"], &[], None)
            .await?;
        if check.exit_code != 0 {
            true
        } else {
            let ver = check.stdout.trim().to_string();
            let major: u32 = ver.split('.').next().and_then(|s| s.parse().ok()).unwrap_or(0);
            major < 18
        }
    };

    if need_node {
        eprintln!("installing node.js 22...");
        let install_node = concat!(
            "curl -fsSL https://deb.nodesource.com/setup_22.x | bash - > /dev/null 2>&1",
            " && apt-get install -y -qq nodejs > /dev/null 2>&1"
        );
        let result = client
            .exec(sprite_name, &["bash", "-c", install_node], &[], None)
            .await?;
        if result.exit_code != 0 {
            return Err(format!(
                "failed to install node.js (exit {}):\n{}{}",
                result.exit_code, result.stdout, result.stderr
            ));
        }
    }

    let pkg_list = to_install.join(" ");
    eprintln!("installing {pkg_list} (this may take a minute)...");
    let install_cmd = format!(
        concat!(
            "apt-get install -y -qq bubblewrap > /dev/null 2>&1;",
            " npm install -g {pkgs} 2>&1",
            " && NODE_BIN=$(which node)",
            " && [ -n \"$NODE_BIN\" ] && ln -sf \"$NODE_BIN\" /usr/local/bin/node",
            " && NPM_BIN=$(npm prefix -g)/bin",
            " && for bin in claude codex; do",
            "   [ -f \"$NPM_BIN/$bin\" ] && ln -sf \"$NPM_BIN/$bin\" /usr/local/bin/$bin;",
            " done",
        ),
        pkgs = pkg_list,
    );

    // Run install with a spinner since it takes a while
    let client2 = client.clone();
    let name2 = sprite_name.to_string();
    let mut handle = tokio::spawn(async move {
        client2
            .exec_with_timeout(
                &name2,
                &["bash", "-c", &install_cmd],
                &[],
                None,
                &[],
                std::time::Duration::from_secs(300),
            )
            .await
    });

    const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let start = std::time::Instant::now();
    let mut frame = 0usize;
    loop {
        tokio::select! {
            result = &mut handle => {
                eprint!("\r\x1b[K");
                let result = result.map_err(|e| format!("install task failed: {e}"))??;
                if result.exit_code != 0 {
                    return Err(format!(
                        "failed to install tools (exit {}):\n{}{}",
                        result.exit_code, result.stdout, result.stderr
                    ));
                }
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                let elapsed = start.elapsed().as_secs();
                let ch = SPINNER[frame % SPINNER.len()];
                eprint!("\r{ch} installing... {elapsed}s");
                frame += 1;
            }
        }
    }

    eprintln!("dev tools installed");
    Ok(())
}

/// Wait for a sprite to exist.
///
/// IMPORTANT: Do NOT add status polling, wake pokes, or any other readiness
/// logic here. The Go SDK (github.com/superfly/sprites-go) does not gate on
/// sprite status before connecting. Cold/warm/running sprites all wake
/// automatically when a WebSocket control or exec connection is opened.
/// The connection itself IS the wake mechanism. Just verify the sprite exists
/// and let the subsequent exec/console calls handle the rest.
async fn wait_for_sprite(client: &SpritesClient, name: &str) -> Result<(), String> {
    match client.get_sprite(name).await? {
        Some(_) => Ok(()),
        None => Err(format!("sprite {name} not found")),
    }
}

fn fly_whoami() -> Option<String> {
    let output = ProcessCommand::new("fly")
        .args(["auth", "whoami"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let email = String::from_utf8(output.stdout).ok()?;
    let trimmed = email.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

fn create_client() -> Result<SpritesClient, String> {
    let token = auth::load_token().ok_or_else(|| {
        "not authenticated. Run `spritebox auth login` or set SPRITEBOX_TOKEN.".to_string()
    })?;
    SpritesClient::new(token)
}

fn default_user() -> String {
    env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_else(|_| "dev".to_string())
}

fn resolve_launch_selector(mut options: LaunchOptions) -> Result<LaunchOptions, String> {
    if options.name.is_none() && options.repo.is_none() {
        return Err("provide --repo/--branch or --name to launch a sprite".to_string());
    }

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
        if let Ok(index) = selection.parse::<usize>()
            && (1..=branches.len()).contains(&index)
        {
            return Ok(branches[index - 1].clone());
        }
        eprintln!(
            "Invalid selection. Enter a number from 1 to {}.",
            branches.len()
        );
    }
}

fn confirm_destroy(sprite_name: &str) -> Result<bool, String> {
    print!("Destroy sprite {sprite_name}? [y/N] ");
    io::stdout().flush().map_err(|err| err.to_string())?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|err| err.to_string())?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
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
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return Ok(None), // gh not installed
    };

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

/// Convert a git SSH URL to HTTPS format for token-based auth.
/// e.g. git@github.com:org/repo.git -> https://github.com/org/repo.git
fn ssh_to_https(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        return format!("https://github.com/{rest}");
    }
    if let Some(rest) = url.strip_prefix("git@gitlab.com:") {
        return format!("https://gitlab.com/{rest}");
    }
    // Already HTTPS or other format, return as-is
    url.to_string()
}

fn shell_escape(s: &str) -> String {
    // Simple single-quote escaping for shell
    format!("'{}'", s.replace('\'', "'\\''"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_defaults_to_launch() {
        let cli = Cli::parse_args(Vec::new()).expect("parse should succeed");
        assert!(matches!(cli.command, Command::Launch(_)));
    }

    #[test]
    fn repo_and_branch_parse_as_launch() {
        let cli = Cli::parse_args(vec![
            "--repo".to_string(),
            "git@github.com:org/repo.git".to_string(),
            "--branch".to_string(),
            "main".to_string(),
        ])
        .expect("parse should succeed");

        match cli.command {
            Command::Launch(options) => {
                assert_eq!(options.repo.as_deref(), Some("git@github.com:org/repo.git"));
                assert_eq!(options.branch.as_deref(), Some("main"));
            }
            _ => panic!("expected launch command"),
        }
    }

    #[test]
    fn destroy_parses_with_yes_flag() {
        let cli = Cli::parse_args(vec![
            "destroy".to_string(),
            "--name".to_string(),
            "my-sprite".to_string(),
            "--yes".to_string(),
        ])
        .expect("parse should succeed");

        match cli.command {
            Command::Destroy(options) => {
                assert_eq!(options.target.name.as_deref(), Some("my-sprite"));
                assert!(options.yes);
            }
            _ => panic!("expected destroy command"),
        }
    }

    #[test]
    fn ssh_to_https_converts_github() {
        assert_eq!(
            ssh_to_https("git@github.com:org/repo.git"),
            "https://github.com/org/repo.git"
        );
    }

    #[test]
    fn ssh_to_https_passthrough_https() {
        let url = "https://github.com/org/repo.git";
        assert_eq!(ssh_to_https(url), url);
    }
}
