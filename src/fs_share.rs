//! `spritebox share` — host-side runner for the virtual filesystem.
//!
//! Runs a long-lived FS share between a local directory and a sprite's
//! mount point. On invocation:
//!
//! 1. Ensure /dev/fuse is openable on the sprite (`chmod 666`).
//! 2. Enable `user_allow_other` in `/etc/fuse.conf`.
//! 3. Push the cross-compiled `spritebox-fsd` binary to the sprite.
//! 4. mkdir the mount point.
//! 5. Open an exec WebSocket running the daemon.
//! 6. Wrap the WS as a `FrameSink`/`FrameStream` pair.
//! 7. Run the host-side `Dispatcher` backed by a `TokioFs` rooted at the
//!    local directory; serve `Frame::Request`s from the daemon.
//! 8. Spawn a `notify` watcher on the local directory that pushes
//!    `Push` frames back to the sprite as files change.
//!
//! The daemon binary must be pre-built for `x86_64-unknown-linux-gnu`
//! (or whatever the sprite arch is). For local development build via:
//!
//! ```bash
//! docker run --rm -v "$(pwd)":/work -w /work rust:1 \\
//!     cargo build --release -p spritebox-fsd --target x86_64-unknown-linux-gnu
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spritebox_fs_host::watcher::WatcherConfig;
use spritebox_fs_host::{Dispatcher, PrefetchConfig, TokioFs, prefetch, watcher};
use spritebox_fs_protocol::Frame;
use spritebox_fs_transport::{FrameSink, FrameStream, WsFrameSink, WsFrameStream};
use tokio::sync::{Mutex, mpsc};

use crate::sprites_api::SpritesClient;

#[derive(Debug, Clone)]
pub struct ShareSpec {
    pub local: PathBuf,
    pub remote: String,
}

impl ShareSpec {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (local_str, remote_str) = spec
            .split_once(':')
            .ok_or_else(|| "share must be LOCAL:REMOTE".to_string())?;
        let local = PathBuf::from(local_str);
        if !local.is_absolute() {
            return Err(format!(
                "local share path must be absolute: {local_str}"
            ));
        }
        if !local.is_dir() {
            return Err(format!(
                "local share path is not a directory: {local_str}"
            ));
        }
        if !remote_str.starts_with('/') {
            return Err(format!(
                "remote mount point must be absolute: {remote_str}"
            ));
        }
        Ok(ShareSpec {
            local,
            remote: remote_str.to_string(),
        })
    }
}

/// Per-mount daemon log path on the sprite. Slug the mount path so
/// concurrent shares don't collide.
fn log_path_for(remote_mount: &str) -> String {
    let slug: String = remote_mount
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("/tmp/spritebox-fsd-{}.log", slug.trim_matches('_'))
}

/// Default search paths for a pre-built `spritebox-fsd` binary.
fn locate_daemon_binary(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        if !p.exists() {
            return Err(format!(
                "daemon binary not found at: {}",
                p.display()
            ));
        }
        return Ok(p.to_path_buf());
    }
    let candidates = [
        "target/x86_64-unknown-linux-gnu/release/spritebox-fsd",
        "target/x86_64-unknown-linux-gnu/debug/spritebox-fsd",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(format!(
        "spritebox-fsd binary not found. Build it via:\n\
         \tdocker run --rm -v \"$(pwd)\":/work -w /work rust:1 \\\n\
         \t    cargo build --release -p spritebox-fsd --target x86_64-unknown-linux-gnu\n\
         \tor pass --daemon <path>"
    ))
}

/// Provision the sprite for FS sharing: chmod /dev/fuse, enable
/// `user_allow_other`, install the daemon binary, mkdir the mount
/// point.
async fn provision(
    client: &SpritesClient,
    sprite_name: &str,
    daemon_path: &Path,
    remote_mount: &str,
) -> Result<(), String> {
    eprintln!("ensuring /dev/fuse is openable...");
    let r = client
        .exec(sprite_name, &["sudo", "chmod", "666", "/dev/fuse"], &[], None)
        .await?;
    if r.exit_code != 0 {
        return Err(format!(
            "chmod /dev/fuse failed (exit={}): {}",
            r.exit_code, r.stderr
        ));
    }

    eprintln!("ensuring /etc/fuse.conf enables user_allow_other...");
    let r = client
        .exec(
            sprite_name,
            &[
                "sudo",
                "sh",
                "-c",
                "grep -qxF user_allow_other /etc/fuse.conf 2>/dev/null || printf '%s\\n' user_allow_other >> /etc/fuse.conf",
            ],
            &[],
            None,
        )
        .await?;
    if r.exit_code != 0 {
        return Err(format!(
            "enable user_allow_other failed (exit={}): {}",
            r.exit_code, r.stderr
        ));
    }

    eprintln!("installing spritebox-fsd...");
    let bytes = std::fs::read(daemon_path)
        .map_err(|e| format!("read daemon binary: {e}"))?;
    let r = client
        .exec_with_stdin(
            sprite_name,
            &[
                "bash",
                "-c",
                "cat > /usr/local/bin/spritebox-fsd && chmod +x /usr/local/bin/spritebox-fsd",
            ],
            &[],
            None,
            &bytes,
        )
        .await?;
    if r.exit_code != 0 {
        return Err(format!(
            "install daemon failed (exit={}): {}",
            r.exit_code, r.stderr
        ));
    }

    // Self-heal: lazily unmount the path if it's already a (possibly
    // stale) FUSE mount, then ensure the directory exists. `umount -l`
    // detaches the mount from the filesystem hierarchy immediately
    // even if there are still open file descriptors; the orphan
    // daemon holding the old mount becomes harmless and exits when
    // its kernel channel goes away. The umount may fail (path not
    // mounted on a fresh sprite); we ignore that. Only mkdir failure
    // is fatal.
    eprintln!("preparing mount point {remote_mount}...");
    let cleanup_cmd = format!(
        "sudo umount -l {mount} 2>/dev/null; sudo mkdir -p {mount}",
        mount = remote_mount,
    );
    let r = client
        .exec(sprite_name, &["sh", "-c", &cleanup_cmd], &[], None)
        .await?;
    if r.exit_code != 0 {
        return Err(format!(
            "mkdir {remote_mount} failed (exit={}): {}",
            r.exit_code, r.stderr
        ));
    }
    Ok(())
}

/// Provision the sprite and open the daemon connection. All
/// user-facing progress messages are printed here, before any terminal
/// raw-mode console takes over. Returns a ready-to-spawn [`Share`].
pub async fn prepare(
    client: SpritesClient,
    sprite_name: &str,
    spec: ShareSpec,
    daemon_override: Option<&Path>,
) -> Result<Share, String> {
    let daemon = locate_daemon_binary(daemon_override)?;
    eprintln!("daemon: {}", daemon.display());

    provision(&client, sprite_name, &daemon, &spec.remote).await?;

    eprintln!("opening exec WebSocket...");
    // The daemon needs CAP_SYS_ADMIN to mount FUSE; run under sudo so it
    // executes as root. Stderr is captured to a per-mount log file so
    // multiple concurrent shares don't trample each other and so it
    // survives the Sprites exec WS not relaying 0x02 stderr frames in
    // this session mode (verified empirically — even a direct write
    // to /proc/<pid>/fd/2 doesn't make it through). To inspect:
    //     spritebox exec --name <sprite> -- sudo cat <log_path>
    let log_path = log_path_for(&spec.remote);
    eprintln!("daemon log on sprite: {log_path}");
    let cmd = format!(
        "exec sudo -E sh -c 'exec /usr/local/bin/spritebox-fsd \
         --mount \"$1\" --verbose 2>>\"$2\"' sh \"$0\" \"{log_path}\"",
    );
    let ws = client
        .open_exec(
            sprite_name,
            &["sh", "-c", &cmd, &spec.remote],
            &[("RUST_LOG", "info")],
            None,
        )
        .await?;
    let (sink, stream) = ws.split();
    let frame_sink = WsFrameSink::new(sink);

    // Forward daemon stderr to our own stderr so mount errors etc. are
    // visible. Spawn a small task to drain the channel.
    let (stderr_tx, mut stderr_rx) =
        tokio::sync::mpsc::unbounded_channel::<bytes::Bytes>();
    let frame_stream = WsFrameStream::new(stream).with_stderr(stderr_tx);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut stderr = tokio::io::stderr();
        while let Some(chunk) = stderr_rx.recv().await {
            let prefixed: Vec<u8> = b"[fsd] ".iter().copied().chain(chunk.iter().copied()).collect();
            let _ = stderr.write_all(&prefixed).await;
            let _ = stderr.flush().await;
        }
    });

    Ok(Share {
        spec,
        frame_sink,
        frame_stream,
    })
}

/// A prepared share — daemon launched, WebSocket open, ready to drive.
pub struct Share {
    pub spec: ShareSpec,
    frame_sink: WsFrameSink<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    frame_stream:
        WsFrameStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

impl Share {
    /// Spawn the dispatch loop and watcher. Returns a JoinHandle —
    /// `abort()` it to tear down. Silent unless something fails.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        let Share {
            spec,
            frame_sink,
            mut frame_stream,
        } = self;

        let host_fs = TokioFs::new(spec.local.clone());
        let dispatcher = Dispatcher::new(host_fs);
        let inodes = dispatcher.inodes();
        let generations = dispatcher.generations();

        tokio::spawn(async move {
            // Single writer task owns the sink.
            let (out_tx, mut out_rx) = mpsc::channel::<Frame>(128);
            let frame_sink = Arc::new(Mutex::new(frame_sink));
            let writer = tokio::spawn({
                let frame_sink = frame_sink.clone();
                async move {
                    while let Some(frame) = out_rx.recv().await {
                        let mut g = frame_sink.lock().await;
                        if g.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
            });

            let (push_tx, mut push_rx) = mpsc::channel(64);
            let _watcher_guard = watcher::spawn(
                spec.local.clone(),
                inodes.clone(),
                generations.clone(),
                push_tx.clone(),
                WatcherConfig::default(),
            )
            .ok();

            // Background prefetch: walk the share root and ship the
            // contents of small files first, populating the sprite's
            // content cache before the kernel asks for them.
            let prefetch_handle = prefetch::spawn(
                std::path::PathBuf::new(),
                Arc::new(TokioFs::new(spec.local.clone())),
                inodes,
                generations,
                push_tx,
                PrefetchConfig::default(),
            );

            let pusher = tokio::spawn({
                let out_tx = out_tx.clone();
                async move {
                    while let Some(push) = push_rx.recv().await {
                        if out_tx.send(Frame::Push(push)).await.is_err() {
                            break;
                        }
                    }
                }
            });

            let dispatcher = Arc::new(dispatcher);
            while let Some(frame) = frame_stream.recv().await {
                if let Frame::Request { id, body } = frame {
                    let dispatcher = dispatcher.clone();
                    let out_tx = out_tx.clone();
                    tokio::spawn(async move {
                        let resp = dispatcher.handle(id, body).await;
                        let _ = out_tx.send(resp).await;
                    });
                }
            }

            drop(out_tx);
            prefetch_handle.abort();
            let _ = writer.await;
            pusher.abort();
            let _ = pusher.await;
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = format!("{}:/workspace/shared", tmp.path().display());
        let parsed = ShareSpec::parse(&spec).unwrap();
        assert_eq!(parsed.remote, "/workspace/shared");
        assert_eq!(parsed.local, tmp.path());
    }

    #[test]
    fn rejects_relative_local() {
        assert!(ShareSpec::parse("relative/path:/workspace").is_err());
    }

    #[test]
    fn rejects_relative_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = format!("{}:relative", tmp.path().display());
        assert!(ShareSpec::parse(&spec).is_err());
    }

    #[test]
    fn rejects_no_separator() {
        assert!(ShareSpec::parse("/just/local").is_err());
    }
}
