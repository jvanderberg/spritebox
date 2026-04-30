//! In-memory `HostFs` impl. Used as the test oracle.
//!
//! The tree lives behind a single async mutex. This is fine for tests —
//! we're not optimizing for concurrent throughput, we're optimizing for
//! determinism and easy state inspection.
//!
//! mtime is driven by an injectable [`Clock`] so tests can assert exact
//! timestamps without relying on wall-clock time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use spritebox_fs_protocol::{FileAttr, FileKind, Ino, ROOT_INO};
use tokio::sync::Mutex;

use crate::{DirChild, HostError, HostFs, Result, check_relative};

/// Injectable clock so tests can advance time deterministically.
pub trait Clock: Send + Sync + 'static {
    /// Nanoseconds since Unix epoch.
    fn now_ns(&self) -> i64;
}

/// Wall-clock implementation. Default for non-test use.
#[derive(Default, Clone)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i64,
            Err(e) => -(e.duration().as_nanos() as i64),
        }
    }
}

/// Manually-advanced clock for tests.
pub struct FakeClock {
    ns: std::sync::atomic::AtomicI64,
}

impl FakeClock {
    pub fn new(initial_ns: i64) -> Arc<Self> {
        Arc::new(Self {
            ns: std::sync::atomic::AtomicI64::new(initial_ns),
        })
    }

    pub fn advance_ns(&self, delta: i64) {
        self.ns
            .fetch_add(delta, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ns(&self) -> i64 {
        self.ns.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
enum Node {
    File { content: Bytes, mode: u16 },
    Dir { children: BTreeMap<String, Ino>, mode: u16 },
}

#[derive(Debug, Clone)]
struct InodeEntry {
    parent: Option<Ino>,
    name: String,
    node: Node,
    atime_ns: i64,
    mtime_ns: i64,
    ctime_ns: i64,
}

impl InodeEntry {
    fn kind(&self) -> FileKind {
        match self.node {
            Node::File { .. } => FileKind::Regular,
            Node::Dir { .. } => FileKind::Directory,
        }
    }
}

struct Inner {
    inodes: BTreeMap<Ino, InodeEntry>,
    next_ino: Ino,
}

impl Inner {
    fn new(now_ns: i64) -> Self {
        let mut inodes = BTreeMap::new();
        inodes.insert(
            ROOT_INO,
            InodeEntry {
                parent: None,
                name: String::new(),
                node: Node::Dir {
                    children: BTreeMap::new(),
                    mode: 0o755,
                },
                atime_ns: now_ns,
                mtime_ns: now_ns,
                ctime_ns: now_ns,
            },
        );
        Self {
            inodes,
            next_ino: ROOT_INO + 1,
        }
    }

    fn alloc_ino(&mut self) -> Ino {
        let n = self.next_ino;
        self.next_ino += 1;
        n
    }

    fn resolve(&self, path: &Path) -> Result<Ino> {
        let mut cur = ROOT_INO;
        for c in path.components() {
            use std::path::Component;
            match c {
                Component::CurDir => {}
                Component::Normal(name) => {
                    let name_str = name.to_str().ok_or(HostError::InvalidName)?;
                    let entry = self
                        .inodes
                        .get(&cur)
                        .ok_or_else(|| HostError::Io("dangling parent".into()))?;
                    let Node::Dir { children, .. } = &entry.node else {
                        return Err(HostError::NotADirectory);
                    };
                    let child_ino = children.get(name_str).ok_or(HostError::NotFound)?;
                    cur = *child_ino;
                }
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(HostError::PathEscape);
                }
            }
        }
        Ok(cur)
    }

    fn split_parent<'a>(&self, path: &'a Path) -> Result<(Ino, &'a str)> {
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        let name = path
            .file_name()
            .ok_or(HostError::InvalidName)?
            .to_str()
            .ok_or(HostError::InvalidName)?;
        if name.is_empty() || name == "." || name == ".." {
            return Err(HostError::InvalidName);
        }
        if name.contains('\0') {
            return Err(HostError::InvalidName);
        }
        let parent_ino = self.resolve(parent)?;
        Ok((parent_ino, name))
    }

    fn attr(&self, ino: Ino) -> Result<FileAttr> {
        let entry = self.inodes.get(&ino).ok_or(HostError::NotFound)?;
        let (size, mode, kind, blocks, nlink) = match &entry.node {
            Node::File { content, mode } => (
                content.len() as u64,
                *mode,
                FileKind::Regular,
                content.len().div_ceil(512) as u64,
                1,
            ),
            Node::Dir { children, mode } => {
                (children.len() as u64, *mode, FileKind::Directory, 1, 2)
            }
        };
        Ok(FileAttr {
            ino,
            size,
            blocks,
            atime_ns: entry.atime_ns,
            mtime_ns: entry.mtime_ns,
            ctime_ns: entry.ctime_ns,
            kind,
            mode,
            nlink,
            uid: 0,
            gid: 0,
        })
    }
}

#[derive(Clone)]
pub struct MemFs {
    inner: Arc<Mutex<Inner>>,
    clock: Arc<dyn Clock>,
}

impl Default for MemFs {
    fn default() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }
}

impl MemFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        let now = clock.now_ns();
        Self {
            inner: Arc::new(Mutex::new(Inner::new(now))),
            clock,
        }
    }
}

#[async_trait]
impl HostFs for MemFs {
    async fn stat(&self, path: &Path) -> Result<FileAttr> {
        check_relative(path)?;
        let inner = self.inner.lock().await;
        let ino = inner.resolve(path)?;
        inner.attr(ino)
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> Result<Bytes> {
        check_relative(path)?;
        let inner = self.inner.lock().await;
        let ino = inner.resolve(path)?;
        let entry = inner.inodes.get(&ino).ok_or(HostError::NotFound)?;
        let Node::File { content, .. } = &entry.node else {
            return Err(HostError::IsADirectory);
        };
        if offset >= content.len() as u64 {
            return Ok(Bytes::new());
        }
        let start = offset as usize;
        let end = (start + size as usize).min(content.len());
        Ok(content.slice(start..end))
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> Result<u64> {
        check_relative(path)?;
        let mut inner = self.inner.lock().await;
        let ino = inner.resolve(path)?;
        let now = self.clock.now_ns();
        let entry = inner.inodes.get_mut(&ino).ok_or(HostError::NotFound)?;
        let Node::File { content, .. } = &mut entry.node else {
            return Err(HostError::IsADirectory);
        };
        let start = offset as usize;
        let end = start + data.len();
        let mut buf: Vec<u8> = if end > content.len() {
            let mut v = content.to_vec();
            v.resize(end, 0);
            v
        } else {
            content.to_vec()
        };
        buf[start..end].copy_from_slice(data);
        let new_len = buf.len() as u64;
        *content = Bytes::from(buf);
        entry.mtime_ns = now;
        entry.ctime_ns = now;
        Ok(new_len)
    }

    async fn create(&self, path: &Path, mode: u16) -> Result<FileAttr> {
        check_relative(path)?;
        let mut inner = self.inner.lock().await;
        let now = self.clock.now_ns();
        let (parent_ino, name) = inner.split_parent(path)?;
        let name = name.to_string();
        {
            let parent_entry = inner.inodes.get(&parent_ino).ok_or(HostError::NotFound)?;
            let Node::Dir { children, .. } = &parent_entry.node else {
                return Err(HostError::NotADirectory);
            };
            if children.contains_key(&name) {
                return Err(HostError::AlreadyExists);
            }
        }
        let ino = inner.alloc_ino();
        inner.inodes.insert(
            ino,
            InodeEntry {
                parent: Some(parent_ino),
                name: name.clone(),
                node: Node::File {
                    content: Bytes::new(),
                    mode,
                },
                atime_ns: now,
                mtime_ns: now,
                ctime_ns: now,
            },
        );
        let parent_entry = inner.inodes.get_mut(&parent_ino).unwrap();
        if let Node::Dir { children, .. } = &mut parent_entry.node {
            children.insert(name, ino);
            parent_entry.mtime_ns = now;
            parent_entry.ctime_ns = now;
        }
        inner.attr(ino)
    }

    async fn mkdir(&self, path: &Path, mode: u16) -> Result<FileAttr> {
        check_relative(path)?;
        let mut inner = self.inner.lock().await;
        let now = self.clock.now_ns();
        let (parent_ino, name) = inner.split_parent(path)?;
        let name = name.to_string();
        {
            let parent_entry = inner.inodes.get(&parent_ino).ok_or(HostError::NotFound)?;
            let Node::Dir { children, .. } = &parent_entry.node else {
                return Err(HostError::NotADirectory);
            };
            if children.contains_key(&name) {
                return Err(HostError::AlreadyExists);
            }
        }
        let ino = inner.alloc_ino();
        inner.inodes.insert(
            ino,
            InodeEntry {
                parent: Some(parent_ino),
                name: name.clone(),
                node: Node::Dir {
                    children: BTreeMap::new(),
                    mode,
                },
                atime_ns: now,
                mtime_ns: now,
                ctime_ns: now,
            },
        );
        let parent_entry = inner.inodes.get_mut(&parent_ino).unwrap();
        if let Node::Dir { children, .. } = &mut parent_entry.node {
            children.insert(name, ino);
            parent_entry.mtime_ns = now;
            parent_entry.ctime_ns = now;
        }
        inner.attr(ino)
    }

    async fn unlink(&self, path: &Path) -> Result<()> {
        check_relative(path)?;
        let mut inner = self.inner.lock().await;
        let now = self.clock.now_ns();
        let (parent_ino, name) = inner.split_parent(path)?;
        let name = name.to_string();
        let target_ino = {
            let parent_entry = inner.inodes.get(&parent_ino).ok_or(HostError::NotFound)?;
            let Node::Dir { children, .. } = &parent_entry.node else {
                return Err(HostError::NotADirectory);
            };
            *children.get(&name).ok_or(HostError::NotFound)?
        };
        let target = inner.inodes.get(&target_ino).ok_or(HostError::NotFound)?;
        if matches!(target.node, Node::Dir { .. }) {
            return Err(HostError::IsADirectory);
        }
        inner.inodes.remove(&target_ino);
        let parent_entry = inner.inodes.get_mut(&parent_ino).unwrap();
        if let Node::Dir { children, .. } = &mut parent_entry.node {
            children.remove(&name);
            parent_entry.mtime_ns = now;
            parent_entry.ctime_ns = now;
        }
        Ok(())
    }

    async fn rmdir(&self, path: &Path) -> Result<()> {
        check_relative(path)?;
        let mut inner = self.inner.lock().await;
        let now = self.clock.now_ns();
        let (parent_ino, name) = inner.split_parent(path)?;
        let name = name.to_string();
        let target_ino = {
            let parent_entry = inner.inodes.get(&parent_ino).ok_or(HostError::NotFound)?;
            let Node::Dir { children, .. } = &parent_entry.node else {
                return Err(HostError::NotADirectory);
            };
            *children.get(&name).ok_or(HostError::NotFound)?
        };
        let target = inner.inodes.get(&target_ino).ok_or(HostError::NotFound)?;
        match &target.node {
            Node::File { .. } => return Err(HostError::NotADirectory),
            Node::Dir { children, .. } => {
                if !children.is_empty() {
                    return Err(HostError::NotEmpty);
                }
            }
        }
        inner.inodes.remove(&target_ino);
        let parent_entry = inner.inodes.get_mut(&parent_ino).unwrap();
        if let Node::Dir { children, .. } = &mut parent_entry.node {
            children.remove(&name);
            parent_entry.mtime_ns = now;
            parent_entry.ctime_ns = now;
        }
        Ok(())
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        check_relative(from)?;
        check_relative(to)?;
        let mut inner = self.inner.lock().await;
        let now = self.clock.now_ns();
        let (from_parent, from_name) = inner.split_parent(from)?;
        let from_name = from_name.to_string();
        let (to_parent, to_name) = inner.split_parent(to)?;
        let to_name = to_name.to_string();

        let target_ino = {
            let parent_entry = inner.inodes.get(&from_parent).ok_or(HostError::NotFound)?;
            let Node::Dir { children, .. } = &parent_entry.node else {
                return Err(HostError::NotADirectory);
            };
            *children.get(&from_name).ok_or(HostError::NotFound)?
        };

        // Atomic-rename: if dest exists, replace it.
        let displaced = {
            let to_parent_entry = inner.inodes.get(&to_parent).ok_or(HostError::NotFound)?;
            let Node::Dir { children, .. } = &to_parent_entry.node else {
                return Err(HostError::NotADirectory);
            };
            children.get(&to_name).copied()
        };

        if let Some(d) = displaced {
            // Don't allow renaming a file over a non-empty directory etc.
            // For simplicity, only allow same-kind replacement.
            let src_kind = inner.inodes.get(&target_ino).unwrap().kind();
            let dst_entry = inner.inodes.get(&d).unwrap();
            let dst_kind = dst_entry.kind();
            match (src_kind, dst_kind) {
                (FileKind::Regular, FileKind::Regular) => {}
                (FileKind::Directory, FileKind::Directory) => {
                    if let Node::Dir { children, .. } = &dst_entry.node
                        && !children.is_empty()
                    {
                        return Err(HostError::NotEmpty);
                    }
                }
                _ => return Err(HostError::AlreadyExists),
            }
            inner.inodes.remove(&d);
        }

        // Detach from old parent.
        if let Some(Node::Dir { children, .. }) =
            inner.inodes.get_mut(&from_parent).map(|e| &mut e.node)
        {
            children.remove(&from_name);
        }
        if let Some(p) = inner.inodes.get_mut(&from_parent) {
            p.mtime_ns = now;
            p.ctime_ns = now;
        }
        // Attach to new parent.
        if let Some(p) = inner.inodes.get_mut(&to_parent)
            && let Node::Dir { children, .. } = &mut p.node
        {
            children.insert(to_name.clone(), target_ino);
            p.mtime_ns = now;
            p.ctime_ns = now;
        }
        if let Some(t) = inner.inodes.get_mut(&target_ino) {
            t.parent = Some(to_parent);
            t.name = to_name;
            t.ctime_ns = now;
        }
        Ok(())
    }

    async fn truncate(&self, path: &Path, size: u64) -> Result<()> {
        check_relative(path)?;
        let mut inner = self.inner.lock().await;
        let ino = inner.resolve(path)?;
        let now = self.clock.now_ns();
        let entry = inner.inodes.get_mut(&ino).ok_or(HostError::NotFound)?;
        let Node::File { content, .. } = &mut entry.node else {
            return Err(HostError::IsADirectory);
        };
        let new_size = size as usize;
        if new_size == content.len() {
            return Ok(());
        }
        let mut v = content.to_vec();
        v.resize(new_size, 0);
        *content = Bytes::from(v);
        entry.mtime_ns = now;
        entry.ctime_ns = now;
        Ok(())
    }

    async fn chmod(&self, path: &Path, new_mode: u16) -> Result<()> {
        check_relative(path)?;
        let mut inner = self.inner.lock().await;
        let ino = inner.resolve(path)?;
        let now = self.clock.now_ns();
        let entry = inner.inodes.get_mut(&ino).ok_or(HostError::NotFound)?;
        match &mut entry.node {
            Node::File { mode, .. } | Node::Dir { mode, .. } => {
                *mode = new_mode & 0o7777;
            }
        }
        entry.ctime_ns = now;
        Ok(())
    }

    async fn fsync(&self, path: &Path) -> Result<()> {
        check_relative(path)?;
        let inner = self.inner.lock().await;
        inner.resolve(path)?;
        Ok(())
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<DirChild>> {
        check_relative(path)?;
        let inner = self.inner.lock().await;
        let ino = inner.resolve(path)?;
        let entry = inner.inodes.get(&ino).ok_or(HostError::NotFound)?;
        let Node::Dir { children, .. } = &entry.node else {
            return Err(HostError::NotADirectory);
        };
        Ok(children
            .iter()
            .map(|(name, child_ino)| {
                let kind = inner
                    .inodes
                    .get(child_ino)
                    .map(|e| e.kind())
                    .unwrap_or(FileKind::Regular);
                DirChild {
                    name: name.clone(),
                    kind,
                }
            })
            .collect())
    }

    async fn snapshot(&self) -> Vec<(PathBuf, FileKind, Option<Bytes>)> {
        let inner = self.inner.lock().await;
        let mut out = Vec::new();
        snapshot_walk(&inner, ROOT_INO, PathBuf::new(), &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

fn snapshot_walk(
    inner: &Inner,
    ino: Ino,
    path: PathBuf,
    out: &mut Vec<(PathBuf, FileKind, Option<Bytes>)>,
) {
    let Some(entry) = inner.inodes.get(&ino) else {
        return;
    };
    match &entry.node {
        Node::File { content, .. } => {
            out.push((path, FileKind::Regular, Some(content.clone())));
        }
        Node::Dir { children, .. } => {
            if !path.as_os_str().is_empty() {
                out.push((path.clone(), FileKind::Directory, None));
            }
            for (name, child_ino) in children {
                snapshot_walk(inner, *child_ino, path.join(name), out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> MemFs {
        let clock = FakeClock::new(1_000_000_000);
        MemFs::with_clock(clock)
    }

    #[tokio::test]
    async fn create_read_write() {
        let fs = fs();
        fs.create(Path::new("a.txt"), 0o644).await.unwrap();
        let n = fs.write(Path::new("a.txt"), 0, b"hello").await.unwrap();
        assert_eq!(n, 5);
        let bytes = fs.read(Path::new("a.txt"), 0, 100).await.unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[tokio::test]
    async fn read_past_eof_returns_empty() {
        let fs = fs();
        fs.create(Path::new("a"), 0o644).await.unwrap();
        fs.write(Path::new("a"), 0, b"abc").await.unwrap();
        let bytes = fs.read(Path::new("a"), 100, 10).await.unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn write_with_gap_zero_fills() {
        let fs = fs();
        fs.create(Path::new("a"), 0o644).await.unwrap();
        fs.write(Path::new("a"), 5, b"x").await.unwrap();
        let bytes = fs.read(Path::new("a"), 0, 100).await.unwrap();
        assert_eq!(&bytes[..], b"\0\0\0\0\0x");
    }

    #[tokio::test]
    async fn mkdir_nested() {
        let fs = fs();
        fs.mkdir(Path::new("a"), 0o755).await.unwrap();
        fs.mkdir(Path::new("a/b"), 0o755).await.unwrap();
        fs.create(Path::new("a/b/c"), 0o644).await.unwrap();
        fs.write(Path::new("a/b/c"), 0, b"deep").await.unwrap();
        let bytes = fs.read(Path::new("a/b/c"), 0, 10).await.unwrap();
        assert_eq!(&bytes[..], b"deep");
    }

    #[tokio::test]
    async fn rename_atomic_replaces_dest() {
        let fs = fs();
        fs.create(Path::new("a"), 0o644).await.unwrap();
        fs.write(Path::new("a"), 0, b"new").await.unwrap();
        fs.create(Path::new("b"), 0o644).await.unwrap();
        fs.write(Path::new("b"), 0, b"old").await.unwrap();
        fs.rename(Path::new("a"), Path::new("b")).await.unwrap();
        assert_eq!(
            fs.read(Path::new("b"), 0, 10).await.unwrap(),
            Bytes::from_static(b"new")
        );
        assert_eq!(
            fs.stat(Path::new("a")).await.unwrap_err(),
            HostError::NotFound
        );
    }

    #[tokio::test]
    async fn rmdir_rejects_nonempty() {
        let fs = fs();
        fs.mkdir(Path::new("a"), 0o755).await.unwrap();
        fs.create(Path::new("a/x"), 0o644).await.unwrap();
        assert_eq!(
            fs.rmdir(Path::new("a")).await.unwrap_err(),
            HostError::NotEmpty
        );
    }

    #[tokio::test]
    async fn truncate_grows_with_zeros() {
        let fs = fs();
        fs.create(Path::new("a"), 0o644).await.unwrap();
        fs.write(Path::new("a"), 0, b"abc").await.unwrap();
        fs.truncate(Path::new("a"), 6).await.unwrap();
        let bytes = fs.read(Path::new("a"), 0, 10).await.unwrap();
        assert_eq!(&bytes[..], b"abc\0\0\0");
    }

    #[tokio::test]
    async fn list_dir_returns_children() {
        let fs = fs();
        fs.create(Path::new("a"), 0o644).await.unwrap();
        fs.mkdir(Path::new("d"), 0o755).await.unwrap();
        let mut children = fs.list_dir(Path::new("")).await.unwrap();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].name, "a");
        assert_eq!(children[0].kind, FileKind::Regular);
        assert_eq!(children[1].name, "d");
        assert_eq!(children[1].kind, FileKind::Directory);
    }

    #[tokio::test]
    async fn snapshot_is_deterministic_and_sorted() {
        let fs = fs();
        fs.mkdir(Path::new("a"), 0o755).await.unwrap();
        fs.create(Path::new("a/y"), 0o644).await.unwrap();
        fs.create(Path::new("a/x"), 0o644).await.unwrap();
        fs.write(Path::new("a/y"), 0, b"Y").await.unwrap();
        fs.write(Path::new("a/x"), 0, b"X").await.unwrap();
        let snap = fs.snapshot().await;
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].0, PathBuf::from("a"));
        assert_eq!(snap[1].0, PathBuf::from("a/x"));
        assert_eq!(snap[2].0, PathBuf::from("a/y"));
    }

    #[tokio::test]
    async fn path_escape_rejected() {
        let fs = fs();
        assert_eq!(
            fs.stat(Path::new("../etc")).await.unwrap_err(),
            HostError::PathEscape
        );
        assert_eq!(
            fs.create(Path::new("/etc/passwd"), 0o644)
                .await
                .unwrap_err(),
            HostError::PathEscape
        );
    }

    #[tokio::test]
    async fn mtime_updates_on_write() {
        let clock = FakeClock::new(1_000);
        let fs = MemFs::with_clock(clock.clone());
        fs.create(Path::new("a"), 0o644).await.unwrap();
        let attr1 = fs.stat(Path::new("a")).await.unwrap();
        clock.advance_ns(500);
        fs.write(Path::new("a"), 0, b"x").await.unwrap();
        let attr2 = fs.stat(Path::new("a")).await.unwrap();
        assert!(attr2.mtime_ns > attr1.mtime_ns);
        assert_eq!(attr2.mtime_ns, 1_500);
    }
}
