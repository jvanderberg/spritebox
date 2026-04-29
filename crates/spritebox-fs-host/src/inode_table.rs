//! Bidirectional ino ↔ path mapping.
//!
//! The protocol uses inode numbers as stable per-file handles, but
//! [`HostFs`](crate::HostFs) operates on paths. The host-side dispatcher
//! sits between them and needs to:
//!
//! - allocate a new ino when a path is first looked up,
//! - return the same ino for subsequent lookups of the same path,
//! - update an ino's path on rename without invalidating it,
//! - retire an ino when the file is unlinked,
//! - resolve an ino back to a path for read/write/etc.
//!
//! [`InodeTable`] is a thin BTreeMap-backed bidirectional map. Root
//! always lives at `ROOT_INO` mapped to the empty path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use spritebox_fs_protocol::{Ino, ROOT_INO};

#[derive(Debug)]
pub struct InodeTable {
    by_ino: BTreeMap<Ino, PathBuf>,
    by_path: BTreeMap<PathBuf, Ino>,
    next_ino: Ino,
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeTable {
    pub fn new() -> Self {
        let mut by_ino = BTreeMap::new();
        let mut by_path = BTreeMap::new();
        by_ino.insert(ROOT_INO, PathBuf::new());
        by_path.insert(PathBuf::new(), ROOT_INO);
        Self {
            by_ino,
            by_path,
            next_ino: ROOT_INO + 1,
        }
    }

    /// Get the path for an ino, if known.
    pub fn path(&self, ino: Ino) -> Option<&Path> {
        self.by_ino.get(&ino).map(|p| p.as_path())
    }

    /// Get the ino for a path, if known.
    pub fn ino(&self, path: &Path) -> Option<Ino> {
        self.by_path.get(path).copied()
    }

    /// Look up the ino for a path, allocating a new one if it doesn't exist.
    pub fn intern(&mut self, path: &Path) -> Ino {
        if let Some(existing) = self.by_path.get(path) {
            return *existing;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.by_ino.insert(ino, path.to_path_buf());
        self.by_path.insert(path.to_path_buf(), ino);
        ino
    }

    /// Retire an ino (file unlinked). Idempotent.
    pub fn forget_ino(&mut self, ino: Ino) {
        if let Some(p) = self.by_ino.remove(&ino) {
            self.by_path.remove(&p);
        }
    }

    /// Retire a path. Idempotent.
    pub fn forget_path(&mut self, path: &Path) {
        if let Some(ino) = self.by_path.remove(path) {
            self.by_ino.remove(&ino);
        }
    }

    /// Move an ino's path. Used after a successful rename. The old ino is
    /// preserved so client-side handles remain valid.
    ///
    /// Also moves any descendants under `from` to be under `to`. If `to`
    /// already has an entry, that entry is forgotten first (rename
    /// replacement semantics).
    pub fn rename(&mut self, from: &Path, to: &Path) {
        // Forget the destination first if it exists (and any descendants).
        let to_descendants: Vec<PathBuf> = self
            .by_path
            .keys()
            .filter(|p| p == &to || p.starts_with(to))
            .cloned()
            .collect();
        for p in to_descendants {
            if let Some(ino) = self.by_path.remove(&p) {
                self.by_ino.remove(&ino);
            }
        }

        // Find every entry rooted at `from`.
        let to_move: Vec<(PathBuf, Ino)> = self
            .by_path
            .iter()
            .filter(|(p, _)| p == &&from.to_path_buf() || p.starts_with(from))
            .map(|(p, ino)| (p.clone(), *ino))
            .collect();
        for (old_path, ino) in to_move {
            self.by_path.remove(&old_path);
            // Compute new path under `to`.
            let new_path = if old_path == from {
                to.to_path_buf()
            } else {
                let suffix = old_path.strip_prefix(from).unwrap();
                to.join(suffix)
            };
            self.by_path.insert(new_path.clone(), ino);
            self.by_ino.insert(ino, new_path);
        }
    }

    pub fn len(&self) -> usize {
        self.by_ino.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ino.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_pre_seeded() {
        let t = InodeTable::new();
        assert_eq!(t.path(ROOT_INO), Some(Path::new("")));
        assert_eq!(t.ino(Path::new("")), Some(ROOT_INO));
    }

    #[test]
    fn intern_is_stable() {
        let mut t = InodeTable::new();
        let a = t.intern(Path::new("a/b"));
        let b = t.intern(Path::new("a/b"));
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_paths_get_distinct_inos() {
        let mut t = InodeTable::new();
        let a = t.intern(Path::new("a"));
        let b = t.intern(Path::new("b"));
        assert_ne!(a, b);
    }

    #[test]
    fn rename_preserves_ino() {
        let mut t = InodeTable::new();
        let ino = t.intern(Path::new("a"));
        t.rename(Path::new("a"), Path::new("b"));
        assert_eq!(t.ino(Path::new("b")), Some(ino));
        assert_eq!(t.ino(Path::new("a")), None);
        assert_eq!(t.path(ino), Some(Path::new("b")));
    }

    #[test]
    fn rename_moves_descendants() {
        let mut t = InodeTable::new();
        let dir_ino = t.intern(Path::new("a"));
        let child_ino = t.intern(Path::new("a/x"));
        let grand_ino = t.intern(Path::new("a/x/y"));

        t.rename(Path::new("a"), Path::new("z"));

        assert_eq!(t.ino(Path::new("z")), Some(dir_ino));
        assert_eq!(t.ino(Path::new("z/x")), Some(child_ino));
        assert_eq!(t.ino(Path::new("z/x/y")), Some(grand_ino));
        assert_eq!(t.ino(Path::new("a")), None);
        assert_eq!(t.ino(Path::new("a/x")), None);
    }

    #[test]
    fn rename_replaces_dest() {
        let mut t = InodeTable::new();
        let src = t.intern(Path::new("a"));
        let _displaced = t.intern(Path::new("b"));
        t.rename(Path::new("a"), Path::new("b"));
        assert_eq!(t.ino(Path::new("b")), Some(src));
    }

    #[test]
    fn forget_path_removes_both_directions() {
        let mut t = InodeTable::new();
        let ino = t.intern(Path::new("a"));
        t.forget_path(Path::new("a"));
        assert_eq!(t.path(ino), None);
        assert_eq!(t.ino(Path::new("a")), None);
    }
}
