//! Sprite-side filesystem daemon.
//!
//! Runs on the sprite (Linux). Mounts FUSE at `--mount` and exchanges
//! `Frame`s with the host over stdin/stdout. The Sprites exec WebSocket
//! transparently carries the bytes (verified binary-clean by the
//! `ws_binary_roundtrip` probe in spritebox).
//!
//! Topology (from sprite's perspective):
//!
//! ```text
//!   FUSE mount  ──▶  CachedRemote  ──▶  CmdClient  ──┐
//!                                                    │
//!     stdin (Push, Response)  ◀──────────────────────┤
//!     stdout (Request)        ──────────────────────▶┘
//! ```
//!
//! The host side runs the Dispatcher backed by TokioFs, plus a Watcher
//! that emits Pushes on local edits.

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

use spritebox_fs_remote::{CacheConfig, CachedRemote, CmdClient};
use spritebox_fs_transport::{StdioSink, StdioStream};
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(version, about = "spritebox virtual filesystem daemon (sprite side)")]
struct Args {
    /// Mount point for the FUSE filesystem.
    #[arg(long)]
    mount: PathBuf,

    /// Run with extra logging.
    #[arg(long)]
    verbose: bool,
}

fn main() -> Result<(), String> {
    let args = Args::parse();

    let env_filter = if args.verbose {
        tracing_subscriber::EnvFilter::new("info,spritebox_fs_fuse=debug")
    } else {
        tracing_subscriber::EnvFilter::from_default_env()
    };
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(mount = %args.mount.display(), verbose = args.verbose, "starting daemon");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build runtime: {e}"))?;

    runtime.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let stream = StdioStream::new(stdin);
        let sink = StdioSink::new(stdout);

        let (push_tx, push_rx) = mpsc::channel(64);
        let client = CmdClient::new(sink, stream, push_tx);
        let cached = CachedRemote::new(client, CacheConfig::default());
        cached.spawn_invalidator(push_rx);

        run_fuse(&args.mount, cached, runtime_handle()).await
    })
}

#[cfg(target_os = "linux")]
async fn run_fuse(
    mount: &std::path::Path,
    remote: Arc<CachedRemote<spritebox_fs_transport::StdioSink<tokio::io::Stdout>>>,
    runtime: tokio::runtime::Handle,
) -> Result<(), String> {
    use spritebox_fs_fuse::SpriteboxFs;

    let fs = SpriteboxFs::new(runtime, remote);
    // We run as root (via sudo from the host) but the user's shell runs
    // as a non-root user; AllowOther lets that user access the mount.
    // Requires `user_allow_other` in /etc/fuse.conf, which the host's
    // provision step ensures.
    let options = vec![
        fuser::MountOption::FSName("spritebox".into()),
        fuser::MountOption::AllowOther,
    ];
    tokio::task::spawn_blocking({
        let mount = mount.to_path_buf();
        move || {
            fuser::mount2(fs, &mount, &options)
                .map_err(|e| format!("fuser::mount2 failed: {e}"))
        }
    })
    .await
    .map_err(|e| format!("blocking task failed: {e}"))?
}

#[cfg(not(target_os = "linux"))]
async fn run_fuse(
    _mount: &std::path::Path,
    _remote: Arc<CachedRemote<spritebox_fs_transport::StdioSink<tokio::io::Stdout>>>,
    _runtime: tokio::runtime::Handle,
) -> Result<(), String> {
    Err("spritebox-fsd only runs on Linux (FUSE)".into())
}

fn runtime_handle() -> tokio::runtime::Handle {
    tokio::runtime::Handle::current()
}
