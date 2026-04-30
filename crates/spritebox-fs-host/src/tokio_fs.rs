//! `tokio::fs`-backed `HostFs` impl rooted at a real path on disk.
//!
//! This is the production implementation. It does *not* try to maintain a
//! separate inode table — `FileAttr.ino` is derived from the OS inode for
//! native files, with a stable ROOT_INO for the share root. That keeps
//! the host-side simple and lets the on-disk filesystem be the source of
//! truth.
//!
//! Path scoping: every incoming path is joined onto `root` and then
//! canonicalized via [`crate::check_relative`] before any I/O. There is
//! no `realpath` call here — symlinks inside the share are honored by
//! the OS, and the upper layers can choose to disallow them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use spritebox_fs_protocol::{FileAttr, FileKind, ROOT_INO};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

use crate::{DirChild, HostError, HostFs, Result, check_relative};

#[derive(Clone)]
pub struct TokioFs {
    root: Arc<PathBuf>,
}

impl TokioFs {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: Arc::new(root),
        }
    }

    fn resolve(&self, path: &Path) -> Result<PathBuf> {
        check_relative(path)?;
        Ok(self.root.join(path))
    }

    async fn stat_inner(abs: &Path) -> Result<FileAttr> {
        let meta = fs::metadata(abs).await.map_err(map_io)?;
        let kind = if meta.is_dir() {
            FileKind::Directory
        } else if meta.file_type().is_symlink() {
            FileKind::Symlink
        } else {
            FileKind::Regular
        };
        let to_ns = |t: std::io::Result<std::time::SystemTime>| -> i64 {
            t.ok()
                .and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0)
        };
        let mtime = to_ns(meta.modified());
        let atime = to_ns(meta.accessed());
        let ctime = to_ns(meta.created());
        let (ino, mode, nlink, uid, gid, blocks) = unix_attr(&meta);
        Ok(FileAttr {
            ino,
            size: meta.len(),
            blocks,
            atime_ns: atime,
            mtime_ns: mtime,
            ctime_ns: ctime,
            kind,
            mode,
            nlink,
            uid,
            gid,
        })
    }
}

#[cfg(unix)]
fn unix_attr(meta: &std::fs::Metadata) -> (u64, u16, u32, u32, u32, u64) {
    use std::os::unix::fs::MetadataExt;
    let ino = if meta.is_dir() && meta.ino() == 0 {
        ROOT_INO
    } else {
        meta.ino()
    };
    let mode = (meta.mode() & 0o7777) as u16;
    (
        ino,
        mode,
        meta.nlink() as u32,
        meta.uid(),
        meta.gid(),
        meta.blocks(),
    )
}

#[cfg(not(unix))]
fn unix_attr(meta: &std::fs::Metadata) -> (u64, u16, u32, u32, u32, u64) {
    let mode = if meta.is_dir() { 0o755 } else { 0o644 };
    (ROOT_INO, mode, 1, 0, 0, meta.len().div_ceil(512))
}

fn map_io(err: std::io::Error) -> HostError {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => HostError::NotFound,
        ErrorKind::PermissionDenied => HostError::PermissionDenied,
        ErrorKind::AlreadyExists => HostError::AlreadyExists,
        ErrorKind::InvalidInput | ErrorKind::InvalidData => HostError::InvalidName,
        _ => {
            let raw = err.raw_os_error();
            if raw == Some(20) {
                HostError::NotADirectory
            } else if raw == Some(21) {
                HostError::IsADirectory
            } else if raw == Some(39) || raw == Some(66) {
                HostError::NotEmpty
            } else if raw == Some(28) {
                HostError::NoSpace
            } else {
                HostError::Io(err.to_string())
            }
        }
    }
}

#[async_trait]
impl HostFs for TokioFs {
    async fn stat(&self, path: &Path) -> Result<FileAttr> {
        let abs = self.resolve(path)?;
        Self::stat_inner(&abs).await
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> Result<Bytes> {
        let abs = self.resolve(path)?;
        let mut f = fs::File::open(&abs).await.map_err(map_io)?;
        f.seek(SeekFrom::Start(offset)).await.map_err(map_io)?;
        let mut buf = vec![0u8; size as usize];
        let n = f.read(&mut buf).await.map_err(map_io)?;
        buf.truncate(n);
        Ok(Bytes::from(buf))
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> Result<u64> {
        let abs = self.resolve(path)?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(false)
            .open(&abs)
            .await
            .map_err(map_io)?;
        f.seek(SeekFrom::Start(offset)).await.map_err(map_io)?;
        f.write_all(data).await.map_err(map_io)?;
        let meta = f.metadata().await.map_err(map_io)?;
        Ok(meta.len())
    }

    async fn create(&self, path: &Path, mode: u16) -> Result<FileAttr> {
        let abs = self.resolve(path)?;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode as u32);
        }
        let _f = opts.open(&abs).await.map_err(map_io)?;
        // Re-apply the mode explicitly: open(2) ANDs with umask, so the
        // file may have ended up with fewer bits than requested.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode as u32);
            let _ = fs::set_permissions(&abs, perms).await;
        }
        Self::stat_inner(&abs).await
    }

    async fn mkdir(&self, path: &Path, mode: u16) -> Result<FileAttr> {
        let abs = self.resolve(path)?;
        fs::create_dir(&abs).await.map_err(map_io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode as u32);
            let _ = fs::set_permissions(&abs, perms).await;
        }
        Self::stat_inner(&abs).await
    }

    async fn unlink(&self, path: &Path) -> Result<()> {
        let abs = self.resolve(path)?;
        fs::remove_file(&abs).await.map_err(map_io)
    }

    async fn rmdir(&self, path: &Path) -> Result<()> {
        let abs = self.resolve(path)?;
        fs::remove_dir(&abs).await.map_err(map_io)
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let from_abs = self.resolve(from)?;
        let to_abs = self.resolve(to)?;
        fs::rename(&from_abs, &to_abs).await.map_err(map_io)
    }

    async fn truncate(&self, path: &Path, size: u64) -> Result<()> {
        let abs = self.resolve(path)?;
        let f = fs::OpenOptions::new()
            .write(true)
            .open(&abs)
            .await
            .map_err(map_io)?;
        f.set_len(size).await.map_err(map_io)
    }

    async fn chmod(&self, path: &Path, mode: u16) -> Result<()> {
        let abs = self.resolve(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode as u32);
            fs::set_permissions(&abs, perms).await.map_err(map_io)
        }
        #[cfg(not(unix))]
        {
            let _ = (abs, mode);
            Ok(())
        }
    }

    async fn fsync(&self, path: &Path) -> Result<()> {
        let abs = self.resolve(path)?;
        let f = fs::File::open(&abs).await.map_err(map_io)?;
        f.sync_all().await.map_err(map_io)
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<DirChild>> {
        let abs = self.resolve(path)?;
        let mut entries = Vec::new();
        let mut iter = fs::read_dir(&abs).await.map_err(map_io)?;
        while let Some(entry) = iter.next_entry().await.map_err(map_io)? {
            let name = entry
                .file_name()
                .to_str()
                .ok_or(HostError::InvalidName)?
                .to_string();
            let ft = entry.file_type().await.map_err(map_io)?;
            let kind = if ft.is_dir() {
                FileKind::Directory
            } else if ft.is_symlink() {
                FileKind::Symlink
            } else {
                FileKind::Regular
            };
            entries.push(DirChild { name, kind });
        }
        Ok(entries)
    }

    async fn snapshot(&self) -> Vec<(PathBuf, FileKind, Option<Bytes>)> {
        let mut out = Vec::new();
        snapshot_walk(&self.root, Path::new(""), &mut out).await;
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

async fn snapshot_walk(
    root: &Path,
    rel: &Path,
    out: &mut Vec<(PathBuf, FileKind, Option<Bytes>)>,
) {
    let abs = root.join(rel);
    let mut iter = match fs::read_dir(&abs).await {
        Ok(i) => i,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = iter.next_entry().await {
        let name = entry.file_name();
        let child_rel = rel.join(&name);
        let ft = match entry.file_type().await {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            out.push((child_rel.clone(), FileKind::Directory, None));
            Box::pin(snapshot_walk(root, &child_rel, out)).await;
        } else if ft.is_file() {
            let bytes = fs::read(entry.path()).await.ok().map(Bytes::from);
            out.push((child_rel, FileKind::Regular, bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make() -> (TokioFs, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let fs = TokioFs::new(dir.path().to_path_buf());
        (fs, dir)
    }

    #[tokio::test]
    async fn create_read_write() {
        let (fs, _g) = make();
        fs.create(Path::new("a.txt"), 0o644).await.unwrap();
        fs.write(Path::new("a.txt"), 0, b"hello").await.unwrap();
        let bytes = fs.read(Path::new("a.txt"), 0, 100).await.unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[tokio::test]
    async fn rename_replaces_dest() {
        let (fs, _g) = make();
        fs.create(Path::new("a"), 0o644).await.unwrap();
        fs.write(Path::new("a"), 0, b"new").await.unwrap();
        fs.create(Path::new("b"), 0o644).await.unwrap();
        fs.write(Path::new("b"), 0, b"old").await.unwrap();
        fs.rename(Path::new("a"), Path::new("b")).await.unwrap();
        let bytes = fs.read(Path::new("b"), 0, 10).await.unwrap();
        assert_eq!(&bytes[..], b"new");
    }

    #[tokio::test]
    async fn rmdir_rejects_nonempty() {
        let (fs, _g) = make();
        fs.mkdir(Path::new("d"), 0o755).await.unwrap();
        fs.create(Path::new("d/x"), 0o644).await.unwrap();
        let err = fs.rmdir(Path::new("d")).await.unwrap_err();
        assert_eq!(err, HostError::NotEmpty);
    }

    #[tokio::test]
    async fn unlink_then_stat_is_notfound() {
        let (fs, _g) = make();
        fs.create(Path::new("a"), 0o644).await.unwrap();
        fs.unlink(Path::new("a")).await.unwrap();
        assert_eq!(
            fs.stat(Path::new("a")).await.unwrap_err(),
            HostError::NotFound
        );
    }

    #[tokio::test]
    async fn path_escape_rejected() {
        let (fs, _g) = make();
        assert_eq!(
            fs.stat(Path::new("../etc")).await.unwrap_err(),
            HostError::PathEscape
        );
    }

    #[tokio::test]
    async fn snapshot_matches_tree() {
        let (fs, _g) = make();
        fs.mkdir(Path::new("d"), 0o755).await.unwrap();
        fs.create(Path::new("d/x"), 0o644).await.unwrap();
        fs.write(Path::new("d/x"), 0, b"X").await.unwrap();
        let snap = fs.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].0, PathBuf::from("d"));
        assert_eq!(snap[0].1, FileKind::Directory);
        assert_eq!(snap[1].0, PathBuf::from("d/x"));
        assert_eq!(snap[1].1, FileKind::Regular);
        assert_eq!(snap[1].2.as_ref().unwrap()[..], b"X"[..]);
    }
}
