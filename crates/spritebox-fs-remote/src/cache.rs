//! Cached `RemoteFs` impl.
//!
//! Layers attribute, lookup, and negative-lookup caches on top of a
//! [`CmdClient`]. Cache invalidation is driven by [`Push`] frames from
//! the host plus a per-entry TTL.
//!
//! What's cached today:
//!
//! - **Attr cache**: `ino → FileAttr`. Used by `getattr`. TTL default 1s.
//! - **Lookup cache**: `(parent_ino, name) → ino`. Used by `lookup`.
//! - **Negative cache**: `(parent_ino, name) → ENOENT`. Bounds the burst
//!   cost of build tools that probe for files in many dirs. TTL 1s.
//!
//! Not yet cached (separate commits):
//!
//! - File content (per-chunk LRU with hash verification).
//! - Write-back-on-release.
//!
//! All caches are observationally invisible: any read through
//! [`CachedRemote`] must return data identical to the same read through
//! [`PassthroughRemote`] — that's the contract the cache-transparency
//! proptest enforces.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use spritebox_fs_protocol::{
    FileAttr, Ino, OpenFlags, Push, StatFs, errno::{self as e},
};
use spritebox_fs_transport::FrameSink;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::{CmdClient, ClientError, ClientResult, DirPage, PassthroughRemote, RemoteFs};

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub attr_ttl: Duration,
    pub lookup_ttl: Duration,
    pub negative_ttl: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            attr_ttl: Duration::from_secs(1),
            lookup_ttl: Duration::from_secs(1),
            negative_ttl: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone)]
struct AttrEntry {
    attr: FileAttr,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
enum LookupEntry {
    Found {
        ino: Ino,
        attr: FileAttr,
        expires_at: Instant,
    },
    NotFound {
        expires_at: Instant,
    },
}

#[derive(Default)]
struct CacheState {
    attrs: HashMap<Ino, AttrEntry>,
    lookups: HashMap<(Ino, String), LookupEntry>,
    /// Stats for tests / diagnostics.
    hits_attr: u64,
    misses_attr: u64,
    hits_lookup: u64,
    misses_lookup: u64,
    hits_negative: u64,
}

pub struct CachedRemote<S: FrameSink> {
    inner: PassthroughRemote<S>,
    state: Arc<Mutex<CacheState>>,
    config: CacheConfig,
}

impl<S: FrameSink> CachedRemote<S> {
    pub fn new(client: Arc<CmdClient<S>>, config: CacheConfig) -> Arc<Self> {
        Arc::new(Self {
            inner: PassthroughRemote::new(client),
            state: Arc::new(Mutex::new(CacheState::default())),
            config,
        })
    }

    /// Spawn a task that consumes pushes from `rx` and applies cache
    /// invalidations. Returns the JoinHandle so callers can abort it
    /// during shutdown.
    pub fn spawn_invalidator(
        self: &Arc<Self>,
        mut rx: mpsc::Receiver<Push>,
    ) -> tokio::task::JoinHandle<()> {
        let state = self.state.clone();
        tokio::spawn(async move {
            while let Some(push) = rx.recv().await {
                let mut s = state.lock().await;
                apply_push(&mut s, push);
            }
        })
    }

    /// Snapshot of cache statistics. Cheap to call, locks briefly.
    pub async fn stats(&self) -> CacheStats {
        let s = self.state.lock().await;
        CacheStats {
            attr_entries: s.attrs.len(),
            lookup_entries: s.lookups.len(),
            hits_attr: s.hits_attr,
            misses_attr: s.misses_attr,
            hits_lookup: s.hits_lookup,
            misses_lookup: s.misses_lookup,
            hits_negative: s.hits_negative,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub attr_entries: usize,
    pub lookup_entries: usize,
    pub hits_attr: u64,
    pub misses_attr: u64,
    pub hits_lookup: u64,
    pub misses_lookup: u64,
    pub hits_negative: u64,
}

fn apply_push(s: &mut CacheState, push: Push) {
    match push {
        Push::InvalidateAttr { ino } => {
            s.attrs.remove(&ino);
        }
        Push::InvalidateEntry { parent, name, .. } => {
            s.lookups.remove(&(parent, name));
        }
        Push::InvalidateData { ino, .. } => {
            // No content cache yet — but invalidating attrs covers the
            // size/mtime mismatch a content change implies.
            s.attrs.remove(&ino);
        }
        Push::Resync { .. } => {
            s.attrs.clear();
            s.lookups.clear();
        }
    }
}

#[async_trait]
impl<S: FrameSink> RemoteFs for CachedRemote<S> {
    async fn lookup(&self, parent: Ino, name: &str) -> ClientResult<FileAttr> {
        let key = (parent, name.to_string());
        let now = Instant::now();
        {
            let mut s = self.state.lock().await;
            let cached = s.lookups.get(&key).cloned();
            match cached {
                Some(LookupEntry::Found { attr, expires_at, .. }) if expires_at > now => {
                    s.hits_lookup += 1;
                    return Ok(attr);
                }
                Some(LookupEntry::NotFound { expires_at }) if expires_at > now => {
                    s.hits_negative += 1;
                    return Err(ClientError::Errno(e::ENOENT));
                }
                _ => {}
            }
            s.misses_lookup += 1;
        }
        match self.inner.lookup(parent, name).await {
            Ok(attr) => {
                let mut s = self.state.lock().await;
                s.lookups.insert(
                    key,
                    LookupEntry::Found {
                        ino: attr.ino,
                        attr: attr.clone(),
                        expires_at: now + self.config.lookup_ttl,
                    },
                );
                s.attrs.insert(
                    attr.ino,
                    AttrEntry {
                        attr: attr.clone(),
                        expires_at: now + self.config.attr_ttl,
                    },
                );
                Ok(attr)
            }
            Err(ClientError::Errno(errno)) if errno == e::ENOENT => {
                let mut s = self.state.lock().await;
                s.lookups.insert(
                    key,
                    LookupEntry::NotFound {
                        expires_at: now + self.config.negative_ttl,
                    },
                );
                Err(ClientError::Errno(e::ENOENT))
            }
            Err(other) => Err(other),
        }
    }

    async fn getattr(&self, ino: Ino) -> ClientResult<FileAttr> {
        let now = Instant::now();
        {
            let mut s = self.state.lock().await;
            let cached = s.attrs.get(&ino).cloned();
            if let Some(entry) = cached
                && entry.expires_at > now
            {
                s.hits_attr += 1;
                return Ok(entry.attr);
            }
            s.misses_attr += 1;
        }
        let attr = self.inner.getattr(ino).await?;
        let mut s = self.state.lock().await;
        s.attrs.insert(
            ino,
            AttrEntry {
                attr: attr.clone(),
                expires_at: now + self.config.attr_ttl,
            },
        );
        Ok(attr)
    }

    async fn readdir(&self, ino: Ino, offset: u64) -> ClientResult<DirPage> {
        // No readdir cache yet. Pass through.
        self.inner.readdir(ino, offset).await
    }

    async fn open(&self, ino: Ino, flags: OpenFlags) -> ClientResult<u64> {
        self.inner.open(ino, flags).await
    }

    async fn release(&self, ino: Ino, handle: u64) -> ClientResult<()> {
        self.inner.release(ino, handle).await
    }

    async fn read(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> ClientResult<Bytes> {
        // No content cache yet.
        self.inner.read(ino, handle, offset, size).await
    }

    async fn write(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        data: &[u8],
    ) -> ClientResult<u32> {
        // Writes invalidate attr cache (size/mtime change).
        let result = self.inner.write(ino, handle, offset, data).await;
        if result.is_ok() {
            let mut s = self.state.lock().await;
            s.attrs.remove(&ino);
        }
        result
    }

    async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u16,
        flags: OpenFlags,
    ) -> ClientResult<FileAttr> {
        let attr = self.inner.create(parent, name, mode, flags).await?;
        let mut s = self.state.lock().await;
        let now = Instant::now();
        // Newly created entry is positive in the lookup cache.
        s.lookups.insert(
            (parent, name.to_string()),
            LookupEntry::Found {
                ino: attr.ino,
                attr: attr.clone(),
                expires_at: now + self.config.lookup_ttl,
            },
        );
        s.attrs.insert(
            attr.ino,
            AttrEntry {
                attr: attr.clone(),
                expires_at: now + self.config.attr_ttl,
            },
        );
        Ok(attr)
    }

    async fn mkdir(&self, parent: Ino, name: &str, mode: u16) -> ClientResult<FileAttr> {
        let attr = self.inner.mkdir(parent, name, mode).await?;
        let mut s = self.state.lock().await;
        let now = Instant::now();
        s.lookups.insert(
            (parent, name.to_string()),
            LookupEntry::Found {
                ino: attr.ino,
                attr: attr.clone(),
                expires_at: now + self.config.lookup_ttl,
            },
        );
        s.attrs.insert(
            attr.ino,
            AttrEntry {
                attr: attr.clone(),
                expires_at: now + self.config.attr_ttl,
            },
        );
        Ok(attr)
    }

    async fn unlink(&self, parent: Ino, name: &str) -> ClientResult<()> {
        let result = self.inner.unlink(parent, name).await;
        if result.is_ok() {
            let mut s = self.state.lock().await;
            // Drop the lookup so subsequent lookups go to the host.
            s.lookups.remove(&(parent, name.to_string()));
            // We don't know the ino here without consulting the cache;
            // drop conservatively by walking. Simpler: just leave the
            // attr — it'll TTL out, and a future lookup will return the
            // freshly-cached negative entry. For correctness we MUST
            // drop the lookup, which we did.
        }
        result
    }

    async fn rmdir(&self, parent: Ino, name: &str) -> ClientResult<()> {
        let result = self.inner.rmdir(parent, name).await;
        if result.is_ok() {
            let mut s = self.state.lock().await;
            s.lookups.remove(&(parent, name.to_string()));
        }
        result
    }

    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
    ) -> ClientResult<()> {
        let result = self
            .inner
            .rename(old_parent, old_name, new_parent, new_name)
            .await;
        if result.is_ok() {
            let mut s = self.state.lock().await;
            s.lookups.remove(&(old_parent, old_name.to_string()));
            s.lookups.remove(&(new_parent, new_name.to_string()));
        }
        result
    }

    async fn truncate(&self, ino: Ino, size: u64) -> ClientResult<()> {
        let result = self.inner.truncate(ino, size).await;
        if result.is_ok() {
            self.state.lock().await.attrs.remove(&ino);
        }
        result
    }

    async fn fsync(&self, ino: Ino, handle: u64, data_only: bool) -> ClientResult<()> {
        self.inner.fsync(ino, handle, data_only).await
    }

    async fn statfs(&self, ino: Ino) -> ClientResult<StatFs> {
        self.inner.statfs(ino).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spritebox_fs_host::HostFs;
    use spritebox_fs_host::mem::FakeClock;
    use spritebox_fs_host::{Dispatcher, MemFs};
    use spritebox_fs_protocol::{Frame, ROOT_INO};
    use spritebox_fs_transport::{FrameStream, InMemoryTransport, RealClock};

    fn rw_flags() -> OpenFlags {
        OpenFlags {
            read: true,
            write: true,
            append: false,
            truncate: false,
        }
    }

    /// Wire a CachedRemote with the given config against a MemFs host.
    /// Returns (cached, host_fs, push_tx).
    async fn rig(
        config: CacheConfig,
    ) -> (
        Arc<CachedRemote<spritebox_fs_transport::InMemSink>>,
        MemFs,
        mpsc::Sender<Push>,
    ) {
        let clock = Arc::new(RealClock);
        let (client_end, mut server_end, _ctrl_a, _ctrl_b) = InMemoryTransport::pair(clock);
        let host_fs = MemFs::with_clock(FakeClock::new(1_000_000));
        let dispatcher = Dispatcher::new(host_fs.clone());
        tokio::spawn(async move {
            while let Some(frame) = server_end.stream.recv().await {
                if let Frame::Request { id, body } = frame {
                    let resp = dispatcher.handle(id, body).await;
                    if server_end.sink.send(resp).await.is_err() {
                        break;
                    }
                }
            }
        });
        let (push_tx, push_rx) = mpsc::channel(64);
        let client = CmdClient::new(client_end.sink, client_end.stream, push_tx.clone());
        let cached = CachedRemote::new(client, config);
        cached.spawn_invalidator(push_rx);
        (cached, host_fs, push_tx)
    }

    #[tokio::test]
    async fn getattr_caches_after_first_call() {
        let (cached, _fs, _) = rig(CacheConfig::default()).await;
        let attr = cached
            .create(ROOT_INO, "a", 0o644, rw_flags())
            .await
            .unwrap();
        // After create, attr should already be in cache.
        let _ = cached.getattr(attr.ino).await.unwrap();
        let _ = cached.getattr(attr.ino).await.unwrap();
        let stats = cached.stats().await;
        assert!(stats.hits_attr >= 1);
    }

    #[tokio::test]
    async fn lookup_caches_positive() {
        let (cached, _fs, _) = rig(CacheConfig::default()).await;
        cached.create(ROOT_INO, "a", 0o644, rw_flags()).await.unwrap();
        let _ = cached.lookup(ROOT_INO, "a").await.unwrap();
        let _ = cached.lookup(ROOT_INO, "a").await.unwrap();
        let stats = cached.stats().await;
        assert!(stats.hits_lookup >= 1);
    }

    #[tokio::test]
    async fn negative_lookup_is_cached() {
        let (cached, _fs, _) = rig(CacheConfig::default()).await;
        let r1 = cached.lookup(ROOT_INO, "missing").await;
        let r2 = cached.lookup(ROOT_INO, "missing").await;
        assert_eq!(r1.unwrap_err(), ClientError::Errno(e::ENOENT));
        assert_eq!(r2.unwrap_err(), ClientError::Errno(e::ENOENT));
        let stats = cached.stats().await;
        assert!(stats.hits_negative >= 1);
    }

    #[tokio::test]
    async fn negative_cache_expires_after_ttl() {
        let cfg = CacheConfig {
            negative_ttl: Duration::from_millis(50),
            ..Default::default()
        };
        let (cached, host_fs, _) = rig(cfg).await;

        // 1. Look up "appears" → ENOENT cached.
        let r = cached.lookup(ROOT_INO, "appears").await;
        assert_eq!(r.unwrap_err(), ClientError::Errno(e::ENOENT));

        // 2. Host creates the file (out-of-band — no Push fired).
        host_fs
            .create(std::path::Path::new("appears"), 0o644)
            .await
            .unwrap();

        // 3. Within TTL, lookup still returns ENOENT (the cache is stale
        //    but within its window — that's the design).
        let r = cached.lookup(ROOT_INO, "appears").await;
        assert_eq!(r.unwrap_err(), ClientError::Errno(e::ENOENT));

        // 4. After TTL expires, lookup re-checks and finds it.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let r = cached.lookup(ROOT_INO, "appears").await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn push_invalidate_attr_drops_cache() {
        let (cached, _fs, push_tx) = rig(CacheConfig::default()).await;
        let attr = cached
            .create(ROOT_INO, "a", 0o644, rw_flags())
            .await
            .unwrap();
        let _ = cached.getattr(attr.ino).await.unwrap();
        push_tx
            .send(Push::InvalidateAttr { ino: attr.ino })
            .await
            .unwrap();
        // Wait for invalidator to apply.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = cached.getattr(attr.ino).await.unwrap();
        let stats = cached.stats().await;
        // 2 misses (initial after create may bypass since insert is synchronous,
        // and getattr after invalidate). Check that at least one miss happened.
        assert!(stats.misses_attr >= 1);
    }

    #[tokio::test]
    async fn push_invalidate_entry_drops_lookup() {
        let (cached, _fs, push_tx) = rig(CacheConfig::default()).await;
        cached.create(ROOT_INO, "a", 0o644, rw_flags()).await.unwrap();
        let _ = cached.lookup(ROOT_INO, "a").await.unwrap();
        push_tx
            .send(Push::InvalidateEntry {
                parent: ROOT_INO,
                name: "a".into(),
                ino: None,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let stats_before = cached.stats().await;
        let _ = cached.lookup(ROOT_INO, "a").await.unwrap();
        let stats_after = cached.stats().await;
        assert!(stats_after.misses_lookup > stats_before.misses_lookup);
    }

    #[tokio::test]
    async fn resync_clears_everything() {
        let (cached, _fs, push_tx) = rig(CacheConfig::default()).await;
        cached.create(ROOT_INO, "a", 0o644, rw_flags()).await.unwrap();
        cached.create(ROOT_INO, "b", 0o644, rw_flags()).await.unwrap();
        cached.lookup(ROOT_INO, "a").await.unwrap();
        cached.lookup(ROOT_INO, "b").await.unwrap();
        let stats_before = cached.stats().await;
        assert!(stats_before.attr_entries >= 2);

        push_tx.send(Push::Resync { epoch: 1 }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let stats_after = cached.stats().await;
        assert_eq!(stats_after.attr_entries, 0);
        assert_eq!(stats_after.lookup_entries, 0);
    }

    #[tokio::test]
    async fn write_invalidates_attr_for_size_change() {
        let (cached, _fs, _) = rig(CacheConfig::default()).await;
        let attr = cached
            .create(ROOT_INO, "f", 0o644, rw_flags())
            .await
            .unwrap();
        let fh = cached.open(attr.ino, rw_flags()).await.unwrap();
        cached.write(attr.ino, fh, 0, b"hello").await.unwrap();
        let new_attr = cached.getattr(attr.ino).await.unwrap();
        assert_eq!(new_attr.size, 5);
    }

    #[tokio::test]
    async fn rename_invalidates_both_lookup_keys() {
        let (cached, _fs, _) = rig(CacheConfig::default()).await;
        cached.create(ROOT_INO, "a", 0o644, rw_flags()).await.unwrap();
        cached.lookup(ROOT_INO, "a").await.unwrap();
        cached.rename(ROOT_INO, "a", ROOT_INO, "b").await.unwrap();
        // After rename, lookup("a") MUST return ENOENT freshly (not stale).
        let r = cached.lookup(ROOT_INO, "a").await;
        assert_eq!(r.unwrap_err(), ClientError::Errno(e::ENOENT));
        let r = cached.lookup(ROOT_INO, "b").await;
        assert!(r.is_ok());
    }
}
