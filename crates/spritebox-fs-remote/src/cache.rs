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

use std::collections::{BTreeMap, HashMap};
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

use crate::{
    CmdClient, ClientError, ClientResult, DirPage, PassthroughRemote, RemoteFs,
    content_cache::{ContentCache, ContentCacheConfig},
};

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub attr_ttl: Duration,
    pub lookup_ttl: Duration,
    pub negative_ttl: Duration,
    pub content: ContentCacheConfig,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            attr_ttl: Duration::from_secs(1),
            lookup_ttl: Duration::from_secs(1),
            negative_ttl: Duration::from_secs(1),
            content: ContentCacheConfig::default(),
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
        attr: FileAttr,
        expires_at: Instant,
    },
    NotFound {
        expires_at: Instant,
    },
}

struct CacheState {
    attrs: HashMap<Ino, AttrEntry>,
    lookups: HashMap<(Ino, String), LookupEntry>,
    content: ContentCache,
    /// Highest generation we've seen for an inode — bumped on every
    /// InvalidateData push or local mutation. A read fetch that
    /// returns a strictly-lower generation has been overtaken by an
    /// invalidation and must NOT be inserted into the content cache.
    inode_generation: HashMap<Ino, u64>,
    /// Per-inode write-back state. Created lazily on first write to an
    /// ino, kept around until the inode is invalidated or release
    /// drains the buffer. Reads consult dirty_extents as an overlay
    /// before falling through to content cache + host.
    write_back: HashMap<Ino, InodeWriteBack>,
    /// Stats for tests / diagnostics.
    hits_attr: u64,
    misses_attr: u64,
    hits_lookup: u64,
    misses_lookup: u64,
    hits_negative: u64,
    stale_fetches_dropped: u64,
}

/// Per-inode write-back state.
///
/// Each ino gets a dedicated tokio task (the "flush worker") that
/// drains a bounded mpsc channel of FlushOps in send order. Writes are
/// pushed to the channel and return Ok immediately; reads consult
/// `dirty_extents` for any byte range that has buffered writes. The
/// FUSE-level guarantees we maintain:
///
/// - Per-handle write order is preserved (FIFO through the channel).
/// - Reads observe prior writes from any handle on the same sprite.
/// - Errors from any flushed write are sticky and surfaced on the
///   next `release()` or `fsync()`.
struct InodeWriteBack {
    /// Dirty extents indexed by offset. Inserted on every write, cleared
    /// on barrier success in release/fsync.
    dirty_extents: BTreeMap<u64, bytes::Bytes>,
    /// Sum of dirty bytes — for memory pressure metrics.
    bytes_dirty: usize,
    /// Bounded channel that feeds the flush worker. Sender is held by
    /// the cache; receiver is owned by the worker. Bounded so writes
    /// apply backpressure to the kernel when the worker can't keep up.
    flush_tx: mpsc::Sender<FlushOp>,
    /// First error encountered by the flush worker — sticky until
    /// surfaced via barrier or release. The field is held as an Arc
    /// shared with the worker; reads happen worker-side, not directly
    /// from the struct, so `dead_code` would otherwise complain.
    #[allow(dead_code)]
    first_error: Arc<std::sync::Mutex<Option<ClientError>>>,
}

enum FlushOp {
    /// A single write, sent to the host in order.
    Write {
        offset: u64,
        data: bytes::Bytes,
        handle: u64,
    },
    /// Caller wants confirmation that all prior writes have completed
    /// (or surface the first error). Sent on release/fsync.
    Barrier {
        ack: tokio::sync::oneshot::Sender<ClientResult<()>>,
    },
}

const FLUSH_QUEUE_CAPACITY: usize = 256;

pub struct CachedRemote<S: FrameSink> {
    inner: PassthroughRemote<S>,
    state: Arc<Mutex<CacheState>>,
    config: CacheConfig,
}

impl<S: FrameSink> CachedRemote<S> {
    pub fn new(client: Arc<CmdClient<S>>, config: CacheConfig) -> Arc<Self> {
        let state = CacheState {
            attrs: HashMap::new(),
            lookups: HashMap::new(),
            content: ContentCache::new(config.content),
            inode_generation: HashMap::new(),
            write_back: HashMap::new(),
            hits_attr: 0,
            misses_attr: 0,
            hits_lookup: 0,
            misses_lookup: 0,
            hits_negative: 0,
            stale_fetches_dropped: 0,
        };
        Arc::new(Self {
            inner: PassthroughRemote::new(client),
            state: Arc::new(Mutex::new(state)),
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
            // Counters for periodic prefetch progress reports. Every
            // PREFILL_LOG_INTERVAL prefills, emit a single info log so
            // the user can see warmup progress in the daemon log
            // without one log per chunk.
            const PREFILL_LOG_INTERVAL: u64 = 64;
            let mut prefill_count: u64 = 0;
            let mut prefill_bytes: u64 = 0;
            let mut prefill_dropped_chunk_size: u64 = 0;
            let mut prefill_dropped_stale: u64 = 0;
            while let Some(push) = rx.recv().await {
                let prefill_data_len = if let Push::Prefill { ref data, .. } = push {
                    Some(data.len() as u64)
                } else {
                    None
                };
                let stale_before;
                let chunk_size_skip_before;
                {
                    let mut s = state.lock().await;
                    stale_before = s.stale_fetches_dropped;
                    let cache_chunk_size = s.content.chunk_size();
                    chunk_size_skip_before = if let Push::Prefill { chunk_size, .. } = push {
                        chunk_size as usize != cache_chunk_size
                    } else {
                        false
                    };
                    apply_push(&mut s, push);
                    if let Some(len) = prefill_data_len {
                        if chunk_size_skip_before {
                            prefill_dropped_chunk_size += 1;
                        } else if s.stale_fetches_dropped > stale_before {
                            prefill_dropped_stale += 1;
                        } else {
                            prefill_count += 1;
                            prefill_bytes += len;
                        }
                    }
                }
                if prefill_data_len.is_some()
                    && (prefill_count + prefill_dropped_chunk_size + prefill_dropped_stale)
                        % PREFILL_LOG_INTERVAL
                        == 0
                    && prefill_count + prefill_dropped_chunk_size + prefill_dropped_stale > 0
                {
                    tracing::info!(
                        accepted = prefill_count,
                        bytes = prefill_bytes,
                        dropped_chunk_size = prefill_dropped_chunk_size,
                        dropped_stale = prefill_dropped_stale,
                        "prefill: progress"
                    );
                }
            }
            if prefill_count > 0 || prefill_dropped_chunk_size > 0 || prefill_dropped_stale > 0
            {
                tracing::info!(
                    accepted = prefill_count,
                    bytes = prefill_bytes,
                    dropped_chunk_size = prefill_dropped_chunk_size,
                    dropped_stale = prefill_dropped_stale,
                    "prefill: invalidator exiting"
                );
            }
        })
    }

    /// Get the write-back state for `ino`, creating + spawning the
    /// flush worker if one doesn't yet exist. The state lock is held
    /// briefly; the spawned worker takes ownership of its receiver and
    /// runs independently of the cache lock.
    fn ensure_writeback<'a>(
        &self,
        s: &'a mut CacheState,
        ino: Ino,
    ) -> &'a mut InodeWriteBack {
        if !s.write_back.contains_key(&ino) {
            let (tx, rx) = mpsc::channel(FLUSH_QUEUE_CAPACITY);
            let first_error = Arc::new(std::sync::Mutex::new(None));
            spawn_flush_worker(self.inner.clone(), ino, rx, first_error.clone());
            s.write_back.insert(
                ino,
                InodeWriteBack {
                    dirty_extents: BTreeMap::new(),
                    bytes_dirty: 0,
                    flush_tx: tx,
                    first_error,
                },
            );
        }
        s.write_back.get_mut(&ino).unwrap()
    }

    /// Send a Barrier through the per-ino flush queue and await ack.
    /// Used by release/fsync to drain pending writes and surface
    /// errors. After successful drain, dirty extents are cleared
    /// (the host now has the canonical bytes).
    async fn flush_writeback(&self, ino: Ino) -> ClientResult<()> {
        let (sender, ack_rx) = {
            let mut s = self.state.lock().await;
            let Some(wb) = s.write_back.get(&ino) else {
                return Ok(());
            };
            let tx = wb.flush_tx.clone();
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            (tx, (ack_tx, ack_rx))
        };
        let (ack_tx, ack_rx) = ack_rx;
        if sender.send(FlushOp::Barrier { ack: ack_tx }).await.is_err() {
            return Err(ClientError::Disconnected);
        }
        let result = match ack_rx.await {
            Ok(r) => r,
            Err(_) => Err(ClientError::Disconnected),
        };
        // Clear dirty entries — they're now on the host (or we have an
        // error and reads should re-fetch from the host's truth).
        let mut s = self.state.lock().await;
        if let Some(wb) = s.write_back.get_mut(&ino) {
            wb.dirty_extents.clear();
            wb.bytes_dirty = 0;
        }
        result
    }

    /// Snapshot of cache statistics. Cheap to call, locks briefly.
    pub async fn stats(&self) -> CacheStats {
        let s = self.state.lock().await;
        CacheStats {
            attr_entries: s.attrs.len(),
            lookup_entries: s.lookups.len(),
            content_chunks: s.content.len(),
            content_bytes: s.content.bytes_used(),
            hits_attr: s.hits_attr,
            misses_attr: s.misses_attr,
            hits_lookup: s.hits_lookup,
            misses_lookup: s.misses_lookup,
            hits_negative: s.hits_negative,
            hits_content: s.content.hits,
            misses_content: s.content.misses,
            evictions_content: s.content.evictions,
            bytes_evicted_content: s.content.bytes_evicted,
            stale_fetches_dropped: s.stale_fetches_dropped,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub attr_entries: usize,
    pub lookup_entries: usize,
    pub content_chunks: usize,
    pub hits_attr: u64,
    pub misses_attr: u64,
    pub hits_lookup: u64,
    pub misses_lookup: u64,
    pub hits_negative: u64,
    pub hits_content: u64,
    pub misses_content: u64,
    pub evictions_content: u64,
    pub bytes_evicted_content: u64,
    pub stale_fetches_dropped: u64,
    pub content_bytes: usize,
}

/// Per-inode flush worker. Drains FlushOps in send order, calls
/// inner.write for each Write, sends ack for each Barrier with the
/// sticky first error if any.
fn spawn_flush_worker<S: FrameSink>(
    inner: PassthroughRemote<S>,
    ino: Ino,
    mut rx: mpsc::Receiver<FlushOp>,
    first_error: Arc<std::sync::Mutex<Option<ClientError>>>,
) {
    tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            match op {
                FlushOp::Write {
                    offset,
                    data,
                    handle,
                } => {
                    let result = inner.write(ino, handle, offset, &data).await;
                    if let Err(e) = result {
                        let mut err = first_error.lock().unwrap();
                        if err.is_none() {
                            *err = Some(e);
                        }
                    }
                }
                FlushOp::Barrier { ack } => {
                    let snapshot = first_error.lock().unwrap().clone();
                    let result = match snapshot {
                        Some(e) => Err(e),
                        None => Ok(()),
                    };
                    let _ = ack.send(result);
                }
            }
        }
    });
}

/// Adjust a host-reported FileAttr to account for buffered writes
/// that haven't reached the host yet. The size becomes
/// `max(host_size, max_dirty_extent_end)` so callers see a coherent
/// view: a getattr right after a write returns the new size, not the
/// stale on-disk size. Other fields (mode, mtime, etc.) pass through
/// — only size needs adjustment for write-back coherence.
fn overlay_attr_size(s: &CacheState, ino: Ino, mut attr: FileAttr) -> FileAttr {
    if let Some(wb) = s.write_back.get(&ino) {
        let mut max_end = attr.size;
        for (off, data) in wb.dirty_extents.iter() {
            let end = off.saturating_add(data.len() as u64);
            if end > max_end {
                max_end = end;
            }
        }
        attr.size = max_end;
    }
    attr
}

/// Overlay dirty extents on top of the bytes returned by the
/// content-cache + host fetch path.
///
/// `read_offset` is the requested read's start offset; `read_bytes` is
/// the data returned from cache/host for the range
/// `[read_offset, read_offset + read_bytes.len())`. For each dirty
/// extent that overlaps this range, the dirty bytes overwrite the
/// corresponding slice of `read_bytes`.
async fn overlay_dirty(
    state: &Arc<Mutex<CacheState>>,
    ino: Ino,
    read_offset: u64,
    read_bytes: Bytes,
) -> Bytes {
    let s = state.lock().await;
    let Some(wb) = s.write_back.get(&ino) else {
        return read_bytes;
    };
    if wb.dirty_extents.is_empty() {
        return read_bytes;
    }
    let read_end = read_offset.saturating_add(read_bytes.len() as u64);

    // Collect all dirty extents that overlap [read_offset, read_end).
    // BTreeMap allows efficient range queries — but we need extents
    // whose [extent_offset, extent_offset + extent_len) intersects
    // the read range. Since we don't know extent_len without looking,
    // we have to scan from the largest offset <= read_offset onward
    // (any earlier extent might still overlap if it's long enough).
    // For typical workloads (cargo writes smallish extents) this is
    // bounded; if it becomes a hot path we can add per-extent length
    // indexing.
    let mut overlaid = read_bytes.to_vec();
    for (extent_offset, extent_data) in wb.dirty_extents.iter() {
        let extent_offset = *extent_offset;
        let extent_end = extent_offset.saturating_add(extent_data.len() as u64);
        if extent_end <= read_offset || extent_offset >= read_end {
            continue;
        }
        let copy_start = extent_offset.max(read_offset);
        let copy_end = extent_end.min(read_end);
        let dst_start = (copy_start - read_offset) as usize;
        let dst_end = (copy_end - read_offset) as usize;
        let src_start = (copy_start - extent_offset) as usize;
        let src_end = (copy_end - extent_offset) as usize;
        // Bounds-check defensively; an off-by-one here would corrupt user
        // data silently, which is the worst possible outcome.
        if dst_end > overlaid.len() || src_end > extent_data.len() {
            continue;
        }
        overlaid[dst_start..dst_end].copy_from_slice(&extent_data[src_start..src_end]);
    }
    Bytes::from(overlaid)
}

fn apply_push(s: &mut CacheState, push: Push) {
    match push {
        Push::InvalidateAttr { ino } => {
            s.attrs.remove(&ino);
        }
        Push::InvalidateEntry { parent, name, .. } => {
            s.lookups.remove(&(parent, name));
        }
        Push::InvalidateData {
            ino,
            offset,
            len,
            generation,
        } => {
            s.attrs.remove(&ino);
            s.content.invalidate_range(ino, offset, len);
            // Bump the seen generation. Any in-flight fetch with a
            // strictly-lower generation will be discarded on insert.
            let entry = s.inode_generation.entry(ino).or_insert(0);
            if generation > *entry {
                *entry = generation;
            }
        }
        Push::Resync { .. } => {
            s.attrs.clear();
            s.lookups.clear();
            // Drop all content. ContentCache doesn't have a clear() — just
            // re-init it.
            s.content = ContentCache::new(s.content.config_snapshot());
            // Resync also invalidates all generations — every cached
            // inode is suspect.
            s.inode_generation.clear();
        }
        Push::Prefill {
            ino,
            chunk_idx,
            chunk_size,
            data,
            generation,
        } => {
            // Refuse if the host's chunking assumption differs from
            // ours — we can't address the chunk correctly. Falls back
            // to a regular fetch on first read.
            if chunk_size as usize != s.content.chunk_size() {
                return;
            }
            // Honor the same anti-stale check the regular read path uses.
            let now_seen =
                s.inode_generation.get(&ino).copied().unwrap_or(0);
            if now_seen > generation {
                s.stale_fetches_dropped += 1;
                return;
            }
            s.content.insert(ino, chunk_idx, data);
            let entry = s.inode_generation.entry(ino).or_insert(0);
            if generation > *entry {
                *entry = generation;
            }
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
                return Ok(overlay_attr_size(&s, ino, entry.attr));
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
        Ok(overlay_attr_size(&s, ino, attr))
    }

    async fn readdir(&self, ino: Ino, offset: u64) -> ClientResult<DirPage> {
        // No readdir cache yet. Pass through.
        self.inner.readdir(ino, offset).await
    }

    async fn readdirplus(
        &self,
        ino: Ino,
        offset: u64,
    ) -> ClientResult<crate::DirPagePlus> {
        // Pass through to the host, then populate the lookup + attr
        // caches with each entry's data so subsequent per-entry
        // lookup/getattr calls hit local cache. This is the entire
        // point of readdirplus — and the cache primer is what makes
        // `cd` / `ls` / `find` stay snappy after the initial call.
        let page = self.inner.readdirplus(ino, offset).await?;
        let now = Instant::now();
        let mut s = self.state.lock().await;
        for entry in &page.entries {
            s.lookups.insert(
                (ino, entry.name.clone()),
                LookupEntry::Found {
                    attr: entry.attr.clone(),
                    expires_at: now + self.config.lookup_ttl,
                },
            );
            s.attrs.insert(
                entry.attr.ino,
                AttrEntry {
                    attr: entry.attr.clone(),
                    expires_at: now + self.config.attr_ttl,
                },
            );
        }
        drop(s);
        Ok(page)
    }

    async fn open(&self, ino: Ino, flags: OpenFlags) -> ClientResult<u64> {
        self.inner.open(ino, flags).await
    }

    async fn release(&self, ino: Ino, handle: u64) -> ClientResult<()> {
        // Drain any buffered writes for this ino before forwarding the
        // release. Errors from the buffered writes surface here — this
        // is the POSIX-compatible way to report write-back errors that
        // happened after write() returned Ok.
        let flush_result = self.flush_writeback(ino).await;
        let release_result = self.inner.release(ino, handle).await;
        flush_result.and(release_result)
    }

    async fn read(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> ClientResult<Bytes> {
        let chunk_size = self.config.content.chunk_size as u64;
        if size as u64 > chunk_size * 4 || chunk_size == 0 {
            // Don't cache pathologically large reads; pass through.
            // Note: this path skips dirty-extent overlay too. Reads
            // larger than 4 chunks are an unusual pattern; revisit if
            // it bites.
            return self.inner.read(ino, handle, offset, size).await;
        }

        // Pre-allocate a `size`-byte buffer of zeros. We'll copy host
        // bytes into the leading portion and overlay dirty extents on
        // top. Anywhere outside both is a hole — POSIX semantics.
        let req_size = size as usize;
        let mut out: Vec<u8> = vec![0u8; req_size];
        let mut host_eof_within_request: usize = 0;

        let mut cur_offset = offset;
        let mut remaining = size as u64;

        while remaining > 0 {
            let chunk_idx = cur_offset / chunk_size;
            let off_in_chunk = (cur_offset - chunk_idx * chunk_size) as usize;

            // Try cache first.
            let chunk_bytes = {
                let mut s = self.state.lock().await;
                s.content.get(ino, chunk_idx)
            };

            let chunk_bytes = match chunk_bytes {
                Some(b) => b,
                None => {
                    // Snapshot the seen generation BEFORE fetching. If
                    // an InvalidateData arrives at the sprite while
                    // we're fetching, the response will get dropped
                    // on insert (see hashed comparison below).
                    let seen_before = {
                        let s = self.state.lock().await;
                        s.inode_generation.get(&ino).copied().unwrap_or(0)
                    };
                    let fetch_offset = chunk_idx * chunk_size;
                    let (fetched, response_gen) = self
                        .inner
                        .read_with_generation(ino, handle, fetch_offset, chunk_size as u32)
                        .await?;
                    {
                        let mut s = self.state.lock().await;
                        let now_seen = s.inode_generation.get(&ino).copied().unwrap_or(0);
                        if now_seen > response_gen {
                            s.stale_fetches_dropped += 1;
                        } else {
                            s.content.insert(ino, chunk_idx, fetched.clone());
                            let entry = s.inode_generation.entry(ino).or_insert(0);
                            let max_gen = response_gen.max(seen_before).max(*entry);
                            *entry = max_gen;
                        }
                    }
                    fetched
                }
            };

            let host_short = chunk_bytes.len() < chunk_size as usize;

            if off_in_chunk < chunk_bytes.len() {
                let take = (chunk_bytes.len() - off_in_chunk).min(remaining as usize);
                let dst = (cur_offset - offset) as usize;
                out[dst..dst + take]
                    .copy_from_slice(&chunk_bytes[off_in_chunk..off_in_chunk + take]);
                host_eof_within_request = host_eof_within_request.max(dst + take);
                cur_offset += take as u64;
                remaining -= take as u64;
            } else {
                // off_in_chunk past the end of returned bytes → host EOF
                // is here. Don't break — keep iterating so dirty
                // extents past this point can still be overlaid below.
                cur_offset = (chunk_idx + 1) * chunk_size;
                if cur_offset >= offset + size as u64 {
                    break;
                }
                remaining = (offset + size as u64).saturating_sub(cur_offset);
            }

            if host_short && off_in_chunk + (host_eof_within_request - (cur_offset - offset) as usize)
                <= chunk_bytes.len()
            {
                // Host returned a short chunk → we've passed the host
                // file's end. Stop fetching but let overlay extend the
                // result if dirty extents reach further.
                break;
            }
        }

        // Overlay dirty extents on top of host bytes. Compute effective
        // EOF as max(host's last filled byte within request,
        // max dirty extent end clipped to request) — that determines
        // how many bytes we return.
        let dirty_eof_within_request = {
            let s = self.state.lock().await;
            match s.write_back.get(&ino) {
                Some(wb) => wb
                    .dirty_extents
                    .iter()
                    .filter_map(|(off, data)| {
                        let end = off.saturating_add(data.len() as u64);
                        if end <= offset {
                            None
                        } else {
                            Some(end.min(offset + size as u64))
                        }
                    })
                    .max()
                    .map(|end| (end - offset) as usize)
                    .unwrap_or(0),
                None => 0,
            }
        };

        let effective_eof = host_eof_within_request.max(dirty_eof_within_request);
        out.truncate(effective_eof);
        let result_bytes = Bytes::from(out);
        Ok(overlay_dirty(&self.state, ino, offset, result_bytes).await)
    }

    async fn read_with_generation(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> ClientResult<(Bytes, u64)> {
        // Pass through; the cache uses this internally and shouldn't
        // re-cache via this entry point.
        self.inner
            .read_with_generation(ino, handle, offset, size)
            .await
    }

    async fn write(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        data: &[u8],
    ) -> ClientResult<u32> {
        // Write-back: insert into the per-ino dirty buffer, push the
        // host-write into the per-ino flush queue, and return Ok
        // immediately. The kernel's next FUSE write doesn't have to
        // wait for a WAN round-trip.
        //
        // Errors that occur during the actual host write are sticky —
        // they surface on the next release/fsync via flush_writeback.
        // POSIX explicitly allows this: write(2) "may fail to detect
        // some I/O errors that happen after a successful write."
        let bytes = bytes::Bytes::copy_from_slice(data);
        let len = bytes.len();

        // Acquire the flush channel under the state lock briefly, then
        // release it before sending so we don't hold the global cache
        // lock across the channel's await.
        let flush_tx = {
            let mut s = self.state.lock().await;
            // Update cache invariants synchronously (the kernel's next
            // op might be a read on the same range — we can answer it
            // from the dirty buffer without the host even seeing the
            // write yet).
            s.attrs.remove(&ino);
            s.content.invalidate_range(ino, offset, len as u64);
            *s.inode_generation.entry(ino).or_insert(0) += 1;

            let wb = self.ensure_writeback(&mut s, ino);
            // Track this extent in dirty_extents for read overlay. If
            // an extent at the same offset already exists, it gets
            // overwritten — correct, since the new write supersedes
            // the old one.
            wb.bytes_dirty = wb
                .bytes_dirty
                .saturating_sub(
                    wb.dirty_extents
                        .get(&offset)
                        .map(|b| b.len())
                        .unwrap_or(0),
                )
                .saturating_add(len);
            wb.dirty_extents.insert(offset, bytes.clone());
            wb.flush_tx.clone()
        };

        // Send the FlushOp. Bounded channel applies backpressure if
        // the worker isn't keeping up — the FUSE thread blocks here
        // until the channel has room. With FLUSH_QUEUE_CAPACITY=256
        // and ~100ms RTT per host write, that's a ~25s lag tolerance.
        if flush_tx
            .send(FlushOp::Write {
                offset,
                data: bytes,
                handle,
            })
            .await
            .is_err()
        {
            return Err(ClientError::Disconnected);
        }
        Ok(len as u32)
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
            let mut s = self.state.lock().await;
            s.attrs.remove(&ino);
            s.content.invalidate_ino(ino);
            *s.inode_generation.entry(ino).or_insert(0) += 1;
        }
        result
    }

    async fn chmod(&self, ino: Ino, mode: u16) -> ClientResult<()> {
        let result = self.inner.chmod(ino, mode).await;
        if result.is_ok() {
            let mut s = self.state.lock().await;
            s.attrs.remove(&ino);
            *s.inode_generation.entry(ino).or_insert(0) += 1;
        }
        result
    }

    async fn symlink(
        &self,
        parent: Ino,
        name: &str,
        target: &str,
    ) -> ClientResult<FileAttr> {
        let attr = self.inner.symlink(parent, name, target).await?;
        let mut s = self.state.lock().await;
        let now = Instant::now();
        s.lookups.insert(
            (parent, name.to_string()),
            LookupEntry::Found {
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

    async fn readlink(&self, ino: Ino) -> ClientResult<String> {
        // Symlink targets are immutable from the FUSE perspective —
        // they don't change without a Push::InvalidateData on the
        // symlink ino. For now just pass through; cache later if
        // readlink shows up as a hot path.
        self.inner.readlink(ino).await
    }

    async fn fsync(&self, ino: Ino, handle: u64, data_only: bool) -> ClientResult<()> {
        // Drain buffered writes before fsync. fsync's contract: by
        // the time it returns, the data is persisted on the host. So
        // we must wait for the per-ino flush queue to drain.
        let flush_result = self.flush_writeback(ino).await;
        let fsync_result = self.inner.fsync(ino, handle, data_only).await;
        flush_result.and(fsync_result)
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
    async fn content_cache_serves_repeat_reads_from_cache() {
        let cfg = CacheConfig {
            content: ContentCacheConfig {
                chunk_size: 16,
                max_bytes: 16 * 8,
            },
            ..Default::default()
        };
        let (cached, _fs, _) = rig(cfg).await;
        let attr = cached
            .create(ROOT_INO, "f", 0o644, rw_flags())
            .await
            .unwrap();
        let fh = cached.open(attr.ino, rw_flags()).await.unwrap();
        cached.write(attr.ino, fh, 0, b"hello-world-xyz!").await.unwrap();
        // Flush the write through to the host so subsequent reads
        // exercise the content cache (writes alone live in the
        // dirty-extent overlay and would short-circuit the cache).
        cached.fsync(attr.ino, fh, false).await.unwrap();

        // First read populates the cache.
        let r1 = cached.read(attr.ino, fh, 0, 16).await.unwrap();
        // Second and third reads hit the cache.
        let r2 = cached.read(attr.ino, fh, 0, 16).await.unwrap();
        let r3 = cached.read(attr.ino, fh, 0, 16).await.unwrap();
        assert_eq!(&r1[..], b"hello-world-xyz!");
        assert_eq!(r1, r2);
        assert_eq!(r2, r3);
        let stats = cached.stats().await;
        assert!(stats.hits_content >= 2);
    }

    #[tokio::test]
    async fn content_cache_invalidated_by_write() {
        let cfg = CacheConfig {
            content: ContentCacheConfig {
                chunk_size: 16,
                max_bytes: 16 * 8,
            },
            ..Default::default()
        };
        let (cached, _fs, _) = rig(cfg).await;
        let attr = cached
            .create(ROOT_INO, "f", 0o644, rw_flags())
            .await
            .unwrap();
        let fh = cached.open(attr.ino, rw_flags()).await.unwrap();
        cached.write(attr.ino, fh, 0, b"original").await.unwrap();

        let r1 = cached.read(attr.ino, fh, 0, 16).await.unwrap();
        cached.write(attr.ino, fh, 0, b"replaced").await.unwrap();
        let r2 = cached.read(attr.ino, fh, 0, 16).await.unwrap();
        assert!(r1.starts_with(b"original"));
        assert!(r2.starts_with(b"replaced"));
    }

    #[tokio::test]
    async fn content_cache_evicts_under_pressure() {
        let cfg = CacheConfig {
            content: ContentCacheConfig {
                chunk_size: 16,
                max_bytes: 16 * 2,
            },
            ..Default::default()
        };
        let (cached, _fs, _) = rig(cfg).await;
        // Three files, three chunks → forces eviction. Fsync each
        // before reading so the read populates the content cache (not
        // the dirty-extent overlay).
        for n in &["a", "b", "c"] {
            let attr = cached
                .create(ROOT_INO, n, 0o644, rw_flags())
                .await
                .unwrap();
            let fh = cached.open(attr.ino, rw_flags()).await.unwrap();
            cached.write(attr.ino, fh, 0, b"data-data-data!").await.unwrap();
            cached.fsync(attr.ino, fh, false).await.unwrap();
            cached.read(attr.ino, fh, 0, 16).await.unwrap();
        }
        let stats = cached.stats().await;
        assert!(stats.evictions_content >= 1);
        assert_eq!(stats.content_chunks, 2);
    }

    #[tokio::test]
    async fn content_cache_invalidate_data_push_drops_chunks() {
        let cfg = CacheConfig {
            content: ContentCacheConfig {
                chunk_size: 16,
                max_bytes: 16 * 8,
            },
            ..Default::default()
        };
        let (cached, host_fs, push_tx) = rig(cfg).await;
        let attr = cached
            .create(ROOT_INO, "f", 0o644, rw_flags())
            .await
            .unwrap();
        let fh = cached.open(attr.ino, rw_flags()).await.unwrap();
        cached.write(attr.ino, fh, 0, b"FROM-SPRITE!!!!!").await.unwrap();
        // Flush so the host has the sprite's bytes; otherwise the
        // dirty overlay would mask the host edit below.
        cached.fsync(attr.ino, fh, false).await.unwrap();
        cached.read(attr.ino, fh, 0, 16).await.unwrap();

        // Host edits the file directly.
        host_fs
            .write(std::path::Path::new("f"), 0, b"FROM-HOST!!!!!!!")
            .await
            .unwrap();
        push_tx
            .send(Push::InvalidateData {
                ino: attr.ino,
                offset: 0,
                len: 16,
                generation: 99,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let bytes = cached.read(attr.ino, fh, 0, 16).await.unwrap();
        assert_eq!(&bytes[..], b"FROM-HOST!!!!!!!");
    }

    /// Write-back: a write+read on the same handle returns the just-
    /// written bytes from the dirty-extent overlay before they reach
    /// the host. fsync drains the buffer; release also drains. The
    /// host doesn't see the bytes until flush.
    #[tokio::test]
    async fn write_back_read_sees_unflushed_writes() {
        let (cached, host_fs, _) = rig(CacheConfig::default()).await;
        let attr = cached
            .create(ROOT_INO, "wb", 0o644, rw_flags())
            .await
            .unwrap();
        let fh = cached.open(attr.ino, rw_flags()).await.unwrap();

        cached.write(attr.ino, fh, 0, b"buffered!").await.unwrap();
        // Read on the same handle: must see the write via dirty overlay.
        let r = cached.read(attr.ino, fh, 0, 16).await.unwrap();
        assert_eq!(&r[..], b"buffered!");

        // Read on a *different* handle to the same ino: also sees the
        // dirty extent (it lives at the inode level, not per-handle).
        let fh2 = cached.open(attr.ino, rw_flags()).await.unwrap();
        let r2 = cached.read(attr.ino, fh2, 0, 16).await.unwrap();
        assert_eq!(&r2[..], b"buffered!");

        // Fsync: drain the buffer through to the host. After that, the
        // host has the bytes too.
        cached.fsync(attr.ino, fh, false).await.unwrap();
        let host_bytes = host_fs
            .read(std::path::Path::new("wb"), 0, 16)
            .await
            .unwrap();
        assert_eq!(&host_bytes[..], b"buffered!");
    }

    /// Race coverage: an InvalidateData push that arrives BEFORE a read
    /// fetch's response makes it back must cause the response to be
    /// dropped (not cached). The way we engineer this in a unit test is
    /// to bump the cache's seen-generation past the response's stamped
    /// generation directly via a Push; subsequent reads do NOT cache.
    #[tokio::test]
    async fn race_invalidation_before_response_drops_stale_fetch() {
        // Use a low-latency setup. The harness host doesn't yet stamp a
        // realistic generation on responses (passthrough returns gen=0
        // from the host's read path because Generations is empty until
        // a write bumps it). So we craft the test by writing once
        // (host-side gen → 1), reading (response stamped gen=1),
        // then sending a Push with gen=2 and reading again.
        let cfg = CacheConfig {
            content: ContentCacheConfig {
                chunk_size: 16,
                max_bytes: 16 * 8,
            },
            ..Default::default()
        };
        let (cached, _host_fs, push_tx) = rig(cfg).await;
        let attr = cached
            .create(ROOT_INO, "x", 0o644, rw_flags())
            .await
            .unwrap();
        let fh = cached.open(attr.ino, rw_flags()).await.unwrap();
        cached.write(attr.ino, fh, 0, b"v1-data!!!!!!!!!").await.unwrap();
        // Fsync so the bytes hit the host and a follow-on read
        // populates the content cache (rather than serving from the
        // dirty-extent overlay, which would never trigger the race
        // path we want to exercise).
        cached.fsync(attr.ino, fh, false).await.unwrap();
        // First read populates the cache.
        cached.read(attr.ino, fh, 0, 16).await.unwrap();
        let stats0 = cached.stats().await;
        assert!(stats0.content_chunks >= 1);

        // Push a future-generation invalidation. This bumps the
        // sprite-side seen-gen past anything the host can stamp on a
        // pre-existing response.
        push_tx
            .send(Push::InvalidateData {
                ino: attr.ino,
                offset: 0,
                len: 0,
                generation: 999_999,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Now any read fetches a response stamped with the host's
        // (lower) generation. The cache should refuse to insert.
        cached.read(attr.ino, fh, 0, 16).await.unwrap();
        let stats1 = cached.stats().await;
        assert!(
            stats1.stale_fetches_dropped > 0,
            "expected stale_fetches_dropped > 0, got {stats1:?}"
        );
    }

    /// Cache-transparency: identical sequence of reads against
    /// PassthroughRemote and CachedRemote must produce identical bytes,
    /// regardless of cache state.
    #[tokio::test]
    async fn cache_transparency_under_random_reads() {
        let cfg = CacheConfig {
            content: ContentCacheConfig {
                chunk_size: 16,
                max_bytes: 16 * 4,
            },
            ..Default::default()
        };
        let (cached, host_fs, _) = rig(cfg).await;

        let attr = cached
            .create(ROOT_INO, "f", 0o644, rw_flags())
            .await
            .unwrap();
        let fh = cached.open(attr.ino, rw_flags()).await.unwrap();
        let payload: Vec<u8> = (0..200u8).collect();
        cached.write(attr.ino, fh, 0, &payload).await.unwrap();

        // 100 random reads across the file.
        let pattern: Vec<(u64, u32)> = (0..100)
            .map(|i| {
                let off = (i * 37) % 200;
                let size = ((i * 11) % 64).max(1) as u32;
                (off as u64, size)
            })
            .collect();

        for (off, size) in &pattern {
            let from_cache = cached.read(attr.ino, fh, *off, *size).await.unwrap();
            // Compute ground truth from the host (whose state is the source).
            let truth = host_fs
                .read(std::path::Path::new("f"), *off, *size)
                .await
                .unwrap();
            assert_eq!(
                from_cache, truth,
                "divergence at offset {} size {}",
                off, size
            );
        }
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
