//! Host-side filesystem watcher that produces [`Push`] frames.
//!
//! Wraps `notify` (with the `notify-debouncer-full` debouncer) to
//! translate filesystem events under the share root into protocol-level
//! cache invalidations. The watcher is the bridge between local edits
//! made on the host (in an editor, by `git checkout`, by build tools)
//! and the sprite-side cache.
//!
//! Translation rules (kept conservative — over-invalidating is fine,
//! under-invalidating breaks coherence):
//!
//! - `Create` of a regular file → `InvalidateEntry` on the parent.
//! - `Modify` of any kind → `InvalidateAttr` and `InvalidateData(len=0)`
//!   for the inode (covers content edits, mtime touches, mode changes).
//! - `Remove` → `InvalidateEntry` on the parent.
//! - Rename pairs → `InvalidateEntry` on both the from-parent and the
//!   to-parent.
//! - On any error or unrecognized event, emit `Resync` to drop the whole
//!   cache; correctness over throughput.
//!
//! The watcher needs read access to the [`InodeTable`] so it can map
//! paths back to inos. If the path isn't interned (sprite hasn't asked
//! about it yet), the relevant push is just skipped — the sprite's
//! lazy-load path will see fresh state on the next access.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent, new_debouncer};
use spritebox_fs_protocol::{Ino, Push, ROOT_INO};
use tokio::sync::{Mutex, mpsc};

use crate::dispatch::Generations;
use crate::inode_table::InodeTable;

#[derive(Debug, Clone, Copy)]
pub struct WatcherConfig {
    /// How long to debounce raw filesystem events before applying. Higher
    /// values reduce push volume but lengthen the coherence window.
    pub debounce: Duration,
    /// How many pushes to buffer before backpressure kicks in.
    pub channel_capacity: usize,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(100),
            channel_capacity: 1024,
        }
    }
}

#[derive(Debug)]
pub enum WatcherError {
    Notify(String),
}

impl std::fmt::Display for WatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatcherError::Notify(s) => write!(f, "notify error: {s}"),
        }
    }
}

impl std::error::Error for WatcherError {}

pub struct Watcher {
    /// Holds the underlying notify watcher; dropping shuts it down.
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
    /// Background task that translates events to pushes.
    pub join: tokio::task::JoinHandle<()>,
}

/// Spawn a watcher rooted at `root`. Pushes are sent to `tx`. The
/// `inodes` arc is consulted to map paths to known inos.
pub fn spawn(
    root: PathBuf,
    inodes: Arc<Mutex<InodeTable>>,
    generations: Arc<Mutex<Generations>>,
    tx: mpsc::Sender<Push>,
    config: WatcherConfig,
) -> Result<Watcher, WatcherError> {
    // Canonicalize the root so paths returned by notify (which the OS
    // canonicalizes — e.g. /var → /private/var on macOS) align with our
    // strip_prefix comparison.
    let root = root
        .canonicalize()
        .map_err(|e| WatcherError::Notify(e.to_string()))?;

    // notify-debouncer-full calls our callback from its own (non-tokio)
    // thread. Bridge through a std mpsc and a small forwarding task —
    // tokio's `blocking_send` deadlocks on a current-thread runtime, and
    // `try_send` from there would silently drop bursts.
    let (std_tx, std_rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    let (raw_tx, mut raw_rx) = mpsc::channel::<DebounceEventResult>(config.channel_capacity);

    std::thread::spawn(move || {
        while let Ok(result) = std_rx.recv() {
            if raw_tx.blocking_send(result).is_err() {
                break;
            }
        }
    });

    let mut debouncer = new_debouncer(
        config.debounce,
        None,
        move |result: DebounceEventResult| {
            let _ = std_tx.send(result);
        },
    )
    .map_err(|e| WatcherError::Notify(e.to_string()))?;

    debouncer
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| WatcherError::Notify(e.to_string()))?;

    let root_clone = root.clone();
    let resync_epoch = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let join = tokio::spawn(async move {
        while let Some(result) = raw_rx.recv().await {
            match result {
                Ok(events) => {
                    for ev in events {
                        translate(&root_clone, &inodes, &generations, &tx, &ev).await;
                    }
                }
                Err(_errs) => {
                    // Bump epoch on every error storm so the sprite can
                    // tell repeated resyncs apart.
                    let epoch = resync_epoch
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    let _ = tx.send(Push::Resync { epoch }).await;
                }
            }
        }
    });

    Ok(Watcher {
        _debouncer: debouncer,
        join,
    })
}

async fn translate(
    root: &Path,
    inodes: &Mutex<InodeTable>,
    generations: &Mutex<Generations>,
    tx: &mpsc::Sender<Push>,
    ev: &DebouncedEvent,
) {
    use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

    let pushes: Vec<Push> = match &ev.event.kind {
        EventKind::Create(CreateKind::File | CreateKind::Folder | CreateKind::Other) => {
            collect_entry_invalidations(root, inodes, &ev.event.paths).await
        }
        // Distinguish content changes from metadata-only changes:
        // - Data → InvalidateData (drops content cache + attr)
        // - Metadata → InvalidateAttr only (mode/mtime change, content
        //   is still valid)
        EventKind::Modify(ModifyKind::Data(_)) => {
            let mut out = Vec::new();
            for path in &ev.event.paths {
                if let Some(ino) = ino_for_path(root, inodes, path).await {
                    let new_gen = generations.lock().await.bump(ino);
                    out.push(Push::InvalidateData {
                        ino,
                        offset: 0,
                        len: 0,
                        generation: new_gen,
                    });
                }
            }
            out
        }
        EventKind::Modify(ModifyKind::Metadata(_)) => {
            let mut out = Vec::new();
            for path in &ev.event.paths {
                if let Some(ino) = ino_for_path(root, inodes, path).await {
                    out.push(Push::InvalidateAttr { ino });
                }
            }
            out
        }
        EventKind::Modify(ModifyKind::Other) => {
            // Unspecified modify — over-invalidate to be safe.
            let mut out = Vec::new();
            for path in &ev.event.paths {
                if let Some(ino) = ino_for_path(root, inodes, path).await {
                    let new_gen = generations.lock().await.bump(ino);
                    out.push(Push::InvalidateAttr { ino });
                    out.push(Push::InvalidateData {
                        ino,
                        offset: 0,
                        len: 0,
                        generation: new_gen,
                    });
                }
            }
            out
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            // notify gives us From and To paths. Invalidate entries on
            // both parents so a fresh lookup goes to the host.
            collect_entry_invalidations(root, inodes, &ev.event.paths).await
        }
        EventKind::Modify(ModifyKind::Name(_)) => {
            collect_entry_invalidations(root, inodes, &ev.event.paths).await
        }
        EventKind::Remove(RemoveKind::File | RemoveKind::Folder | RemoveKind::Other) => {
            collect_entry_invalidations(root, inodes, &ev.event.paths).await
        }
        _ => {
            // Unknown event kind — over-invalidate. Epoch=0 is fine here;
            // a hard error path uses fetch_add in the caller.
            vec![Push::Resync { epoch: 0 }]
        }
    };

    for p in pushes {
        if tx.send(p).await.is_err() {
            return;
        }
    }
}

async fn collect_entry_invalidations(
    root: &Path,
    inodes: &Mutex<InodeTable>,
    paths: &[PathBuf],
) -> Vec<Push> {
    let mut out = Vec::new();
    let it = inodes.lock().await;
    for p in paths {
        if let Some((parent_ino, name)) = parent_and_name(root, &it, p) {
            out.push(Push::InvalidateEntry {
                parent: parent_ino,
                name,
                ino: it
                    .ino(p.strip_prefix(root).unwrap_or(p))
                    .filter(|i| *i != ROOT_INO),
            });
        }
    }
    out
}

async fn ino_for_path(
    root: &Path,
    inodes: &Mutex<InodeTable>,
    abs: &Path,
) -> Option<Ino> {
    let rel = abs.strip_prefix(root).ok()?;
    let it = inodes.lock().await;
    it.ino(rel)
}

fn parent_and_name(
    root: &Path,
    it: &InodeTable,
    abs: &Path,
) -> Option<(Ino, String)> {
    let rel = abs.strip_prefix(root).ok()?;
    let parent_rel = rel.parent()?;
    let name = rel.file_name()?.to_str()?.to_string();
    let parent_ino = if parent_rel.as_os_str().is_empty() {
        ROOT_INO
    } else {
        it.ino(parent_rel)?
    };
    Some((parent_ino, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::Mutex;
    use tokio::time::timeout;

    fn populate_inode(it: &mut InodeTable, paths: &[&str]) {
        for p in paths {
            it.intern(Path::new(p));
        }
    }

    /// Wait for the next push that satisfies `pred` within `dur`.
    async fn wait_for(
        rx: &mut mpsc::Receiver<Push>,
        dur: Duration,
        pred: impl Fn(&Push) -> bool,
    ) -> Option<Push> {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match timeout(remaining, rx.recv()).await {
                Ok(Some(push)) => {
                    if pred(&push) {
                        return Some(push);
                    }
                }
                _ => return None,
            }
        }
    }

    #[tokio::test]
    async fn watcher_emits_entry_invalidation_on_create() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let it = Arc::new(Mutex::new(InodeTable::new()));
        let (tx, mut rx) = mpsc::channel(64);
        let gens = Arc::new(Mutex::new(Generations::default()));
        let _w = spawn(
            root.clone(),
            it.clone(),
            gens,
            tx,
            WatcherConfig {
                debounce: Duration::from_millis(50),
                channel_capacity: 64,
            },
        )
        .unwrap();

        std::fs::write(root.join("hello.txt"), b"hi").unwrap();

        let push = wait_for(&mut rx, Duration::from_secs(2), |p| {
            matches!(p, Push::InvalidateEntry { name, .. } if name == "hello.txt")
        })
        .await;
        assert!(push.is_some(), "no InvalidateEntry for hello.txt");
    }

    #[tokio::test]
    async fn watcher_emits_data_invalidation_on_modify() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a"), b"first").unwrap();

        let it = Arc::new(Mutex::new(InodeTable::new()));
        {
            let mut g = it.lock().await;
            populate_inode(&mut g, &["a"]);
        }

        let (tx, mut rx) = mpsc::channel(64);
        let gens = Arc::new(Mutex::new(Generations::default()));
        let _w = spawn(
            root.clone(),
            it.clone(),
            gens,
            tx,
            WatcherConfig {
                debounce: Duration::from_millis(50),
                channel_capacity: 64,
            },
        )
        .unwrap();

        // Wait long enough for the watcher to settle past any historical
        // events macOS FSEvents may replay.
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Drain any backlog.
        while rx.try_recv().is_ok() {}

        // Edit the file. On macOS FSEvents may report modifications as
        // either Modify(Data) or as a fresh Create depending on inode
        // reuse, so we accept either of the data-relevant pushes.
        std::fs::write(root.join("a"), b"second").unwrap();

        let push = wait_for(&mut rx, Duration::from_secs(5), |p| {
            matches!(
                p,
                Push::InvalidateData { .. }
                    | Push::InvalidateEntry { .. }
                    | Push::InvalidateAttr { .. }
            )
        })
        .await;
        assert!(push.is_some(), "no data/entry invalidation on modify");
    }

    // macOS FSEvents drops single-file Remove events outside long batch
    // windows; this test only runs reliably on Linux's inotify backend.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn watcher_emits_entry_invalidation_on_remove() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("doomed"), b"x").unwrap();

        let it = Arc::new(Mutex::new(InodeTable::new()));
        {
            let mut g = it.lock().await;
            populate_inode(&mut g, &["doomed"]);
        }
        let (tx, mut rx) = mpsc::channel(64);
        let gens = Arc::new(Mutex::new(Generations::default()));
        let _w = spawn(
            root.clone(),
            it.clone(),
            gens,
            tx,
            WatcherConfig {
                debounce: Duration::from_millis(50),
                channel_capacity: 64,
            },
        )
        .unwrap();

        // Wait past historical replays then drain.
        tokio::time::sleep(Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        std::fs::remove_file(root.join("doomed")).unwrap();

        let push = wait_for(&mut rx, Duration::from_secs(5), |p| {
            matches!(p, Push::InvalidateEntry { name, .. } if name == "doomed")
        })
        .await;
        assert!(push.is_some(), "no InvalidateEntry on remove");
    }
}
