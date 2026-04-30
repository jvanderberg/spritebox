//! Chunked content cache for the [`super::CachedRemote`].
//!
//! Files are split into fixed-size chunks. Each chunk is cached as a
//! `Bytes` keyed by `(ino, chunk_index)` with byte-count LRU eviction
//! — when the total cached bytes exceeds `max_bytes`, the
//! least-recently-used chunks are dropped until it fits again.
//!
//! Chunk size is the bandwidth-amplification knob (each cold read
//! fetches one chunk's worth from the host); `max_bytes` is the memory
//! cap. Defaults: 16 KiB chunks, 100 MiB cap — sized to hold a typical
//! source-code repository in memory while keeping per-read fetch sizes
//! reasonable for a constrained-bandwidth WAN.
//!
//! Hash verification: chunks come from [`crate::PassthroughRemote::read`]
//! which already verifies the SHA-256 in the response — so anything that
//! makes it into the cache is trusted in-memory and need not be re-hashed
//! on hit. Eviction or invalidation forces a refetch which re-verifies.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use spritebox_fs_protocol::Ino;

pub const DEFAULT_CHUNK_SIZE: usize = 16 * 1024;
pub const DEFAULT_MAX_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct ContentCacheConfig {
    pub chunk_size: usize,
    /// Maximum total cached bytes. Eviction is LRU on byte cost: a
    /// 1 KiB chunk evicts 1 KiB, a 16 KiB chunk evicts 16 KiB. Replaces
    /// the previous chunk-count cap, which gave bad behavior on
    /// repos with many small files (slot exhaustion at trivial bytes).
    pub max_bytes: usize,
}

impl Default for ContentCacheConfig {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

#[derive(Default)]
pub struct ContentCache {
    chunks: HashMap<(Ino, u64), Bytes>,
    lru: VecDeque<(Ino, u64)>,
    /// Sum of the lengths of every cached chunk.
    bytes_used: usize,
    config: ContentCacheConfig,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub bytes_evicted: u64,
}

impl ContentCache {
    pub fn new(config: ContentCacheConfig) -> Self {
        Self {
            chunks: HashMap::new(),
            lru: VecDeque::new(),
            bytes_used: 0,
            config,
            hits: 0,
            misses: 0,
            evictions: 0,
            bytes_evicted: 0,
        }
    }

    pub fn chunk_size(&self) -> usize {
        self.config.chunk_size
    }

    pub fn config_snapshot(&self) -> ContentCacheConfig {
        self.config
    }

    /// Bytes currently cached, summed across all chunks.
    pub fn bytes_used(&self) -> usize {
        self.bytes_used
    }

    /// Look up an in-cache chunk. Updates LRU on hit.
    pub fn get(&mut self, ino: Ino, chunk_idx: u64) -> Option<Bytes> {
        if let Some(data) = self.chunks.get(&(ino, chunk_idx)).cloned() {
            self.hits += 1;
            self.touch(ino, chunk_idx);
            Some(data)
        } else {
            self.misses += 1;
            None
        }
    }

    /// Insert a chunk and evict by bytes if over cap.
    pub fn insert(&mut self, ino: Ino, chunk_idx: u64, data: Bytes) {
        let key = (ino, chunk_idx);
        let new_len = data.len();
        // Refuse pathological single chunks that exceed the whole cap.
        if new_len > self.config.max_bytes {
            return;
        }
        if let Some(prev) = self.chunks.insert(key, data) {
            self.bytes_used = self.bytes_used.saturating_sub(prev.len());
            self.touch(ino, chunk_idx);
        } else {
            self.lru.push_back(key);
        }
        self.bytes_used += new_len;
        self.evict_to_fit();
    }

    fn evict_to_fit(&mut self) {
        while self.bytes_used > self.config.max_bytes
            && let Some(victim) = self.lru.pop_front()
        {
            if let Some(b) = self.chunks.remove(&victim) {
                self.evictions += 1;
                self.bytes_evicted = self.bytes_evicted.saturating_add(b.len() as u64);
                self.bytes_used = self.bytes_used.saturating_sub(b.len());
            }
        }
    }

    /// Drop all chunks for an inode (covers truncate, full invalidation).
    pub fn invalidate_ino(&mut self, ino: Ino) {
        let mut freed = 0usize;
        self.chunks.retain(|(i, _), data| {
            if *i == ino {
                freed += data.len();
                false
            } else {
                true
            }
        });
        self.lru.retain(|(i, _)| *i != ino);
        self.bytes_used = self.bytes_used.saturating_sub(freed);
    }

    /// Drop chunks intersecting `[offset, offset + len)` for `ino`. Pass
    /// `len == 0` to invalidate everything for the inode.
    pub fn invalidate_range(&mut self, ino: Ino, offset: u64, len: u64) {
        if len == 0 {
            self.invalidate_ino(ino);
            return;
        }
        let chunk_size = self.config.chunk_size as u64;
        let start_idx = offset / chunk_size;
        let end_byte = offset.saturating_add(len);
        let end_idx = end_byte.div_ceil(chunk_size);
        let mut freed = 0usize;
        for idx in start_idx..end_idx {
            if let Some(b) = self.chunks.remove(&(ino, idx)) {
                freed += b.len();
            }
        }
        self.lru
            .retain(|(i, idx)| !(*i == ino && *idx >= start_idx && *idx < end_idx));
        self.bytes_used = self.bytes_used.saturating_sub(freed);
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    fn touch(&mut self, ino: Ino, chunk_idx: u64) {
        let key = (ino, chunk_idx);
        if let Some(pos) = self.lru.iter().position(|k| *k == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_then_miss_after_eviction_by_bytes() {
        // 16-byte cap, 8-byte chunks → can hold 2 chunks before eviction.
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 8,
            max_bytes: 16,
        });
        c.insert(1, 0, Bytes::from_static(b"AAAAAAAA"));
        c.insert(1, 1, Bytes::from_static(b"BBBBBBBB"));
        assert!(c.get(1, 0).is_some());
        assert!(c.get(1, 1).is_some());
        assert_eq!(c.bytes_used(), 16);

        c.insert(2, 0, Bytes::from_static(b"CCCCCCCC"));
        assert!(c.bytes_used() <= 16);
        assert_eq!(c.evictions, 1);
        assert_eq!(c.bytes_evicted, 8);
    }

    #[test]
    fn many_small_entries_fit_under_byte_cap() {
        // The point of byte-count LRU: lots of tiny chunks shouldn't
        // exhaust slot count the way the old chunk-count cap did. With
        // 1 KiB max_bytes and 16-byte entries, we should hold 64 of them.
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 16,
            max_bytes: 1024,
        });
        for i in 0..64u64 {
            c.insert(i, 0, Bytes::from_static(&[0xAB; 16]));
        }
        assert_eq!(c.len(), 64);
        assert_eq!(c.bytes_used(), 1024);
        assert_eq!(c.evictions, 0);

        // One more pushes us over.
        c.insert(64, 0, Bytes::from_static(&[0xCD; 16]));
        assert_eq!(c.evictions, 1);
        assert!(c.bytes_used() <= 1024);
    }

    #[test]
    fn variable_size_chunks_evict_proportionally() {
        // A short final chunk (e.g., file tail) should only cost its
        // actual bytes, not a full chunk_size of cap.
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 100,
            max_bytes: 200,
        });
        c.insert(1, 0, Bytes::from_static(&[0u8; 100])); // full chunk
        c.insert(1, 1, Bytes::from_static(&[0u8; 30])); // short tail (e.g. last chunk of 130-byte file)
        assert_eq!(c.bytes_used(), 130);

        // Insert another full chunk — total 230, must evict.
        c.insert(2, 0, Bytes::from_static(&[0u8; 100]));
        assert!(c.bytes_used() <= 200);
    }

    #[test]
    fn invalidate_ino_returns_bytes_to_pool() {
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 100,
            max_bytes: 1000,
        });
        c.insert(1, 0, Bytes::from_static(&[0u8; 100]));
        c.insert(1, 1, Bytes::from_static(&[0u8; 100]));
        c.insert(2, 0, Bytes::from_static(&[0u8; 100]));
        assert_eq!(c.bytes_used(), 300);
        c.invalidate_ino(1);
        assert_eq!(c.bytes_used(), 100);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn invalidate_range_only_drops_intersecting() {
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 100,
            max_bytes: 10000,
        });
        c.insert(1, 0, Bytes::from_static(&[0u8; 100])); // bytes 0-99
        c.insert(1, 1, Bytes::from_static(&[0u8; 100])); // bytes 100-199
        c.insert(1, 2, Bytes::from_static(&[0u8; 100])); // bytes 200-299
        c.invalidate_range(1, 150, 30); // bytes 150-179, only chunk 1
        assert!(c.get(1, 0).is_some());
        assert!(c.get(1, 1).is_none());
        assert!(c.get(1, 2).is_some());
        assert_eq!(c.bytes_used(), 200);
    }

    #[test]
    fn lru_promotes_on_hit() {
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 8,
            max_bytes: 16,
        });
        c.insert(1, 0, Bytes::from_static(b"AAAAAAAA"));
        c.insert(1, 1, Bytes::from_static(b"BBBBBBBB"));
        let _ = c.get(1, 0);
        c.insert(1, 2, Bytes::from_static(b"CCCCCCCC"));
        // Chunk 1 should have been evicted (least recent).
        assert!(c.get(1, 0).is_some());
        assert!(c.get(1, 1).is_none());
        assert!(c.get(1, 2).is_some());
    }

    #[test]
    fn refuses_chunk_larger_than_cap() {
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 100,
            max_bytes: 50,
        });
        c.insert(1, 0, Bytes::from_static(&[0u8; 100])); // bigger than cap
        assert_eq!(c.len(), 0);
        assert_eq!(c.bytes_used(), 0);
    }
}
