//! Chunked content cache for the [`super::CachedRemote`].
//!
//! Files are split into fixed-size chunks (default 64 KiB). Each chunk
//! is cached as a `Bytes` keyed by `(ino, chunk_index)` with simple LRU
//! eviction by chunk count.
//!
//! Hash verification: chunks come from [`crate::PassthroughRemote::read`]
//! which already verifies the SHA-256 in the response — so anything that
//! makes it into the cache is trusted in-memory and need not be re-hashed
//! on hit. Eviction or invalidation forces a refetch which re-verifies.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use spritebox_fs_protocol::Ino;

pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;
pub const DEFAULT_MAX_CHUNKS: usize = 256; // 16 MiB at 64 KiB chunks

#[derive(Clone, Copy, Debug)]
pub struct ContentCacheConfig {
    pub chunk_size: usize,
    pub max_chunks: usize,
}

impl Default for ContentCacheConfig {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_chunks: DEFAULT_MAX_CHUNKS,
        }
    }
}

#[derive(Default)]
pub struct ContentCache {
    chunks: HashMap<(Ino, u64), Bytes>,
    lru: VecDeque<(Ino, u64)>,
    config: ContentCacheConfig,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

impl ContentCache {
    pub fn new(config: ContentCacheConfig) -> Self {
        Self {
            chunks: HashMap::new(),
            lru: VecDeque::new(),
            config,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    pub fn chunk_size(&self) -> usize {
        self.config.chunk_size
    }

    pub fn config_snapshot(&self) -> ContentCacheConfig {
        self.config
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

    /// Insert a chunk and evict if over cap.
    pub fn insert(&mut self, ino: Ino, chunk_idx: u64, data: Bytes) {
        let key = (ino, chunk_idx);
        if self.chunks.insert(key, data).is_some() {
            self.touch(ino, chunk_idx);
        } else {
            self.lru.push_back(key);
        }
        while self.chunks.len() > self.config.max_chunks
            && let Some(victim) = self.lru.pop_front()
        {
            if self.chunks.remove(&victim).is_some() {
                self.evictions += 1;
            }
        }
    }

    /// Drop all chunks for an inode (covers truncate, full invalidation).
    pub fn invalidate_ino(&mut self, ino: Ino) {
        self.chunks.retain(|(i, _), _| *i != ino);
        self.lru.retain(|(i, _)| *i != ino);
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
        for idx in start_idx..end_idx {
            self.chunks.remove(&(ino, idx));
        }
        self.lru
            .retain(|(i, idx)| !(*i == ino && *idx >= start_idx && *idx < end_idx));
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
    fn hit_then_miss_after_eviction() {
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 8,
            max_chunks: 2,
        });
        c.insert(1, 0, Bytes::from_static(b"AAAAAAAA"));
        c.insert(1, 1, Bytes::from_static(b"BBBBBBBB"));
        assert!(c.get(1, 0).is_some());
        assert!(c.get(1, 1).is_some());

        // Inserting a third forces eviction. Chunk 0 was just touched
        // (most recently), chunk 1 was touched after it; the LRU victim
        // depends on the order.
        c.insert(2, 0, Bytes::from_static(b"CCCCCCCC"));
        assert_eq!(c.len(), 2);
        assert_eq!(c.evictions, 1);
    }

    #[test]
    fn invalidate_ino_drops_all_chunks() {
        let mut c = ContentCache::new(ContentCacheConfig::default());
        c.insert(1, 0, Bytes::from_static(b"AAA"));
        c.insert(1, 1, Bytes::from_static(b"BBB"));
        c.insert(2, 0, Bytes::from_static(b"CCC"));
        c.invalidate_ino(1);
        assert!(c.get(1, 0).is_none());
        assert!(c.get(1, 1).is_none());
        assert!(c.get(2, 0).is_some());
    }

    #[test]
    fn invalidate_range_only_drops_intersecting() {
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 100,
            max_chunks: 100,
        });
        c.insert(1, 0, Bytes::from_static(&[0u8; 100])); // bytes 0-99
        c.insert(1, 1, Bytes::from_static(&[0u8; 100])); // bytes 100-199
        c.insert(1, 2, Bytes::from_static(&[0u8; 100])); // bytes 200-299
        c.invalidate_range(1, 150, 30); // bytes 150-179, only chunk 1
        assert!(c.get(1, 0).is_some());
        assert!(c.get(1, 1).is_none());
        assert!(c.get(1, 2).is_some());
    }

    #[test]
    fn lru_promotes_on_hit() {
        let mut c = ContentCache::new(ContentCacheConfig {
            chunk_size: 8,
            max_chunks: 2,
        });
        c.insert(1, 0, Bytes::from_static(b"AAAAAAAA"));
        c.insert(1, 1, Bytes::from_static(b"BBBBBBBB"));
        // Touch chunk 0 so it becomes most recent.
        let _ = c.get(1, 0);
        c.insert(1, 2, Bytes::from_static(b"CCCCCCCC"));
        // Chunk 1 should have been evicted.
        assert!(c.get(1, 0).is_some());
        assert!(c.get(1, 1).is_none());
        assert!(c.get(1, 2).is_some());
    }
}
