//! Host-side filesystem abstraction.
//!
//! The `HostFs` trait is the port that host-side logic uses to talk to
//! the underlying filesystem. Two implementations live here:
//!
//! - [`MemFs`]: an in-memory tree, used as the test oracle.
//! - [`TokioFs`]: a `tokio::fs`-backed adapter rooted at a real path on
//!   disk, used in production.
//!
//! Path scoping is the responsibility of `HostFs` impls: any path passed
//! in is interpreted relative to the share root, and `..` traversal that
//! escapes the root must return [`HostError::PathEscape`].

#![forbid(unsafe_code)]

use async_trait::async_trait;
use bytes::Bytes;
use spritebox_fs_protocol::{FileAttr, FileKind};
use std::path::{Path, PathBuf};

pub mod dispatch;
pub mod inode_table;
pub mod mem;
pub mod tokio_fs;
pub mod watcher;

pub use dispatch::Dispatcher;
pub use inode_table::InodeTable;
pub use mem::MemFs;
pub use tokio_fs::TokioFs;
pub use watcher::{Watcher, WatcherConfig, WatcherError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    NotFound,
    NotADirectory,
    IsADirectory,
    AlreadyExists,
    NotEmpty,
    PermissionDenied,
    PathEscape,
    InvalidName,
    NoSpace,
    Io(String),
}

impl HostError {
    /// Map to a POSIX errno usable in protocol responses.
    pub fn errno(&self) -> i32 {
        use spritebox_fs_protocol::errno::*;
        match self {
            HostError::NotFound => ENOENT,
            HostError::NotADirectory => ENOTDIR,
            HostError::IsADirectory => EISDIR,
            HostError::AlreadyExists => EEXIST,
            HostError::NotEmpty => ENOTEMPTY,
            HostError::PermissionDenied => EACCES,
            HostError::PathEscape => EACCES,
            HostError::InvalidName => EINVAL,
            HostError::NoSpace => ENOSPC,
            HostError::Io(_) => EIO,
        }
    }
}

pub type Result<T> = std::result::Result<T, HostError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirChild {
    pub name: String,
    pub kind: FileKind,
}

/// Host-side filesystem operations. All paths are interpreted relative to
/// the share root; absolute paths and `..` escapes are rejected with
/// [`HostError::PathEscape`].
#[async_trait]
pub trait HostFs: Send + Sync + 'static {
    async fn stat(&self, path: &Path) -> Result<FileAttr>;
    async fn read(&self, path: &Path, offset: u64, size: u32) -> Result<Bytes>;
    /// Returns total bytes now in the file after the write.
    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> Result<u64>;
    async fn create(&self, path: &Path, mode: u16) -> Result<FileAttr>;
    async fn mkdir(&self, path: &Path, mode: u16) -> Result<FileAttr>;
    async fn unlink(&self, path: &Path) -> Result<()>;
    async fn rmdir(&self, path: &Path) -> Result<()>;
    async fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    async fn truncate(&self, path: &Path, size: u64) -> Result<()>;
    async fn chmod(&self, path: &Path, mode: u16) -> Result<()>;
    async fn fsync(&self, path: &Path) -> Result<()>;
    async fn list_dir(&self, path: &Path) -> Result<Vec<DirChild>>;
    /// Snapshot the entire tree as `(path, kind, content)` triples.
    /// Used by tests as an equivalence oracle.
    async fn snapshot(&self) -> Vec<(PathBuf, FileKind, Option<Bytes>)>;
}

/// Validate a path is relative, has no NUL bytes, and contains no `..`
/// components. The host-fs root is always treated as the empty path.
pub(crate) fn check_relative(path: &Path) -> Result<()> {
    use std::path::Component;
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(HostError::InvalidName);
    }
    for c in path.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => return Err(HostError::PathEscape),
            Component::RootDir | Component::Prefix(_) => return Err(HostError::PathEscape),
        }
    }
    Ok(())
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn rejects_parent_traversal() {
        assert_eq!(
            check_relative(Path::new("a/../b")).unwrap_err(),
            HostError::PathEscape
        );
    }

    #[test]
    fn rejects_absolute() {
        assert_eq!(
            check_relative(Path::new("/etc/passwd")).unwrap_err(),
            HostError::PathEscape
        );
    }

    #[test]
    fn allows_relative() {
        check_relative(Path::new("a/b/c")).unwrap();
        check_relative(Path::new("./a")).unwrap();
        check_relative(Path::new("")).unwrap();
    }
}
