//! Host-side request dispatcher.
//!
//! Translates [`Frame::Request`]s into [`HostFs`] calls and produces the
//! corresponding [`Frame::Response`]. Maintains the [`InodeTable`] so the
//! client can refer to files by `Ino` while `HostFs` operates on paths.
//!
//! Open file handles are tracked as a counter — there's no per-handle
//! state today since `HostFs` is stateless. Future work (write buffering,
//! `O_APPEND` cursor) lives here.

use std::path::PathBuf;
use std::sync::Arc;

use spritebox_fs_protocol::{
    DirEntry, FileAttr, FileKind, Frame, Ino, OpenFlags, Push, Request, RequestId, Response,
    StatFs,
    errno::{self as e},
};
use std::collections::HashMap;
use tokio::sync::Mutex;

use crate::{HostFs, InodeTable};

/// Push pipe shared with the watcher / external invalidator.
pub type PushSender = tokio::sync::mpsc::Sender<Push>;

/// Per-open-handle state. Tracks the inode and flags so the dispatcher
/// can honor `O_APPEND` semantics on write and validate read/write
/// permission against the open mode.
#[derive(Debug, Clone, Copy)]
struct HandleState {
    ino: Ino,
    flags: OpenFlags,
}

#[derive(Default)]
struct OpenHandles {
    next_handle: u64,
    open: HashMap<u64, HandleState>,
}

impl OpenHandles {
    fn alloc(&mut self, ino: Ino, flags: OpenFlags) -> u64 {
        self.next_handle += 1;
        let h = self.next_handle;
        self.open.insert(h, HandleState { ino, flags });
        h
    }

    fn release(&mut self, handle: u64) {
        self.open.remove(&handle);
    }

    fn get(&self, handle: u64) -> Option<HandleState> {
        self.open.get(&handle).copied()
    }
}

pub struct Dispatcher<F: HostFs> {
    fs: Arc<F>,
    inodes: Arc<Mutex<InodeTable>>,
    handles: Arc<Mutex<OpenHandles>>,
}

impl<F: HostFs> Clone for Dispatcher<F> {
    fn clone(&self) -> Self {
        Self {
            fs: self.fs.clone(),
            inodes: self.inodes.clone(),
            handles: self.handles.clone(),
        }
    }
}

impl<F: HostFs> Dispatcher<F> {
    pub fn new(fs: F) -> Self {
        Self {
            fs: Arc::new(fs),
            inodes: Arc::new(Mutex::new(InodeTable::new())),
            handles: Arc::new(Mutex::new(OpenHandles::default())),
        }
    }

    pub fn fs(&self) -> &F {
        &self.fs
    }

    pub fn inodes(&self) -> Arc<Mutex<InodeTable>> {
        self.inodes.clone()
    }

    /// Handle a single request and produce a response.
    pub async fn handle(&self, id: RequestId, req: Request) -> Frame {
        let body = self.dispatch(req).await;
        Frame::Response { id, body }
    }

    async fn dispatch(&self, req: Request) -> Response {
        match req {
            Request::Lookup { parent, name } => self.lookup(parent, &name).await,
            Request::GetAttr { ino } => self.getattr(ino).await,
            Request::ReadDir { ino, offset } => self.readdir(ino, offset).await,
            Request::Open { ino, flags } => self.open(ino, flags).await,
            Request::Release { handle, .. } => self.release(handle).await,
            Request::Read {
                ino,
                handle,
                offset,
                size,
            } => self.read(ino, handle, offset, size).await,
            Request::Write {
                ino,
                handle,
                offset,
                data,
            } => self.write(ino, handle, offset, &data).await,
            Request::Create {
                parent,
                name,
                mode,
                ..
            } => self.create(parent, &name, mode).await,
            Request::Mkdir {
                parent,
                name,
                mode,
            } => self.mkdir(parent, &name, mode).await,
            Request::Unlink { parent, name } => self.unlink(parent, &name).await,
            Request::Rmdir { parent, name } => self.rmdir(parent, &name).await,
            Request::Rename {
                old_parent,
                old_name,
                new_parent,
                new_name,
            } => {
                self.rename(old_parent, &old_name, new_parent, &new_name)
                    .await
            }
            Request::Truncate { ino, size } => self.truncate(ino, size).await,
            Request::Chmod { ino, mode } => self.chmod(ino, mode).await,
            Request::Fsync { ino, .. } => self.fsync(ino).await,
            Request::StatFs { .. } => Response::StatFs(default_statfs()),
        }
    }

    async fn resolve_ino(&self, ino: spritebox_fs_protocol::Ino) -> Result<PathBuf, Response> {
        let inodes = self.inodes.lock().await;
        match inodes.path(ino) {
            Some(p) => Ok(p.to_path_buf()),
            None => Err(Response::Error { errno: e::ENOENT }),
        }
    }

    async fn lookup(&self, parent: spritebox_fs_protocol::Ino, name: &str) -> Response {
        let parent_path = match self.resolve_ino(parent).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        let path = parent_path.join(name);
        match self.fs.stat(&path).await {
            Ok(mut attr) => {
                let mut inodes = self.inodes.lock().await;
                attr.ino = inodes.intern(&path);
                Response::Entry { attr }
            }
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn getattr(&self, ino: spritebox_fs_protocol::Ino) -> Response {
        let path = match self.resolve_ino(ino).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        match self.fs.stat(&path).await {
            Ok(mut attr) => {
                attr.ino = ino;
                Response::Attr(attr)
            }
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn readdir(&self, ino: spritebox_fs_protocol::Ino, offset: u64) -> Response {
        let path = match self.resolve_ino(ino).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        let children = match self.fs.list_dir(&path).await {
            Ok(c) => c,
            Err(err) => return Response::Error { errno: err.errno() },
        };
        let start = offset as usize;
        if start >= children.len() {
            return Response::DirPage {
                entries: Vec::new(),
                next_offset: None,
            };
        }
        let mut inodes = self.inodes.lock().await;
        let entries: Vec<DirEntry> = children[start..]
            .iter()
            .map(|child| {
                let child_path = path.join(&child.name);
                let child_ino = inodes.intern(&child_path);
                DirEntry {
                    ino: child_ino,
                    name: child.name.clone(),
                    kind: child.kind,
                }
            })
            .collect();
        Response::DirPage {
            entries,
            next_offset: None,
        }
    }

    async fn open(&self, ino: Ino, flags: OpenFlags) -> Response {
        let path = match self.resolve_ino(ino).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        match self.fs.stat(&path).await {
            Ok(_) => {
                // Honor O_TRUNC at open time so existing content disappears
                // immediately, matching POSIX semantics. Skip if the open
                // is read-only (POSIX would EINVAL but we silently accept).
                if flags.truncate && flags.write
                    && let Err(err) = self.fs.truncate(&path, 0).await
                {
                    return Response::Error { errno: err.errno() };
                }
                let mut h = self.handles.lock().await;
                Response::OpenOk {
                    handle: h.alloc(ino, flags),
                }
            }
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn release(&self, handle: u64) -> Response {
        let mut h = self.handles.lock().await;
        h.release(handle);
        Response::Ok
    }

    async fn read(&self, ino: Ino, handle: u64, offset: u64, size: u32) -> Response {
        // Validate the handle was opened for reading; reject EBADF if not.
        if handle != 0 {
            let h = self.handles.lock().await;
            if let Some(state) = h.get(handle)
                && !state.flags.read
            {
                return Response::Error { errno: e::EBADF };
            }
        }
        let path = match self.resolve_ino(ino).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        match self.fs.read(&path, offset, size).await {
            Ok(b) => Response::bytes(b),
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn write(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        data: &[u8],
    ) -> Response {
        // Look up the handle's flags. handle == 0 is a sentinel from
        // callers that aren't tracking handles (e.g. tests); allow.
        let mut effective_offset = offset;
        if handle != 0 {
            let h = self.handles.lock().await;
            if let Some(state) = h.get(handle) {
                if !state.flags.write {
                    return Response::Error { errno: e::EBADF };
                }
                if state.flags.append {
                    let path = match self.resolve_ino(ino).await {
                        Ok(p) => p,
                        Err(r) => return r,
                    };
                    drop(h);
                    // Stat fresh so the append always lands at end-of-file
                    // even if the file grew via another handle.
                    match self.fs.stat(&path).await {
                        Ok(attr) => effective_offset = attr.size,
                        Err(err) => return Response::Error { errno: err.errno() },
                    }
                }
            }
        }
        let path = match self.resolve_ino(ino).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        match self.fs.write(&path, effective_offset, data).await {
            Ok(_total) => Response::Written {
                bytes: data.len() as u32,
            },
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn create(
        &self,
        parent: spritebox_fs_protocol::Ino,
        name: &str,
        mode: u16,
    ) -> Response {
        let parent_path = match self.resolve_ino(parent).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        let path = parent_path.join(name);
        match self.fs.create(&path, mode).await {
            Ok(mut attr) => {
                let mut inodes = self.inodes.lock().await;
                attr.ino = inodes.intern(&path);
                Response::Entry { attr }
            }
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn mkdir(
        &self,
        parent: spritebox_fs_protocol::Ino,
        name: &str,
        mode: u16,
    ) -> Response {
        let parent_path = match self.resolve_ino(parent).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        let path = parent_path.join(name);
        match self.fs.mkdir(&path, mode).await {
            Ok(mut attr) => {
                let mut inodes = self.inodes.lock().await;
                attr.ino = inodes.intern(&path);
                Response::Entry { attr }
            }
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn unlink(&self, parent: spritebox_fs_protocol::Ino, name: &str) -> Response {
        let parent_path = match self.resolve_ino(parent).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        let path = parent_path.join(name);
        match self.fs.unlink(&path).await {
            Ok(()) => {
                let mut inodes = self.inodes.lock().await;
                inodes.forget_path(&path);
                Response::Ok
            }
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn rmdir(&self, parent: spritebox_fs_protocol::Ino, name: &str) -> Response {
        let parent_path = match self.resolve_ino(parent).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        let path = parent_path.join(name);
        match self.fs.rmdir(&path).await {
            Ok(()) => {
                let mut inodes = self.inodes.lock().await;
                inodes.forget_path(&path);
                Response::Ok
            }
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn rename(
        &self,
        old_parent: spritebox_fs_protocol::Ino,
        old_name: &str,
        new_parent: spritebox_fs_protocol::Ino,
        new_name: &str,
    ) -> Response {
        let old_parent_path = match self.resolve_ino(old_parent).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        let new_parent_path = match self.resolve_ino(new_parent).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        let from = old_parent_path.join(old_name);
        let to = new_parent_path.join(new_name);
        match self.fs.rename(&from, &to).await {
            Ok(()) => {
                let mut inodes = self.inodes.lock().await;
                inodes.rename(&from, &to);
                Response::Ok
            }
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn truncate(&self, ino: spritebox_fs_protocol::Ino, size: u64) -> Response {
        let path = match self.resolve_ino(ino).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        match self.fs.truncate(&path, size).await {
            Ok(()) => Response::Ok,
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn chmod(&self, ino: spritebox_fs_protocol::Ino, mode: u16) -> Response {
        let path = match self.resolve_ino(ino).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        match self.fs.chmod(&path, mode).await {
            Ok(()) => Response::Ok,
            Err(err) => Response::Error { errno: err.errno() },
        }
    }

    async fn fsync(&self, ino: spritebox_fs_protocol::Ino) -> Response {
        let path = match self.resolve_ino(ino).await {
            Ok(p) => p,
            Err(r) => return r,
        };
        match self.fs.fsync(&path).await {
            Ok(()) => Response::Ok,
            Err(err) => Response::Error { errno: err.errno() },
        }
    }
}

fn default_statfs() -> StatFs {
    StatFs {
        blocks: 0,
        bfree: 0,
        bavail: 0,
        files: 0,
        ffree: 0,
        bsize: 4096,
        namelen: 255,
    }
}

// Suppress unused-import warnings on items that exist for future use.
#[allow(dead_code)]
fn _force_use(_a: FileAttr, _k: FileKind, _ps: PushSender) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemFs;
    use crate::mem::FakeClock;
    use bytes::Bytes;
    use spritebox_fs_protocol::{OpenFlags, ROOT_INO};

    fn dispatcher() -> Dispatcher<MemFs> {
        let clock = FakeClock::new(1_000_000);
        Dispatcher::new(MemFs::with_clock(clock))
    }

    fn open_flags() -> OpenFlags {
        OpenFlags {
            read: true,
            write: true,
            append: false,
            truncate: false,
        }
    }

    async fn handle(d: &Dispatcher<MemFs>, req: Request) -> Response {
        let Frame::Response { body, .. } = d.handle(1, req).await else {
            panic!("expected response frame");
        };
        body
    }

    #[tokio::test]
    async fn create_lookup_returns_same_ino() {
        let d = dispatcher();
        let resp = handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "a".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await;
        let Response::Entry { attr } = resp else {
            panic!("expected Entry, got {resp:?}");
        };
        let created_ino = attr.ino;

        let resp = handle(
            &d,
            Request::Lookup {
                parent: ROOT_INO,
                name: "a".into(),
            },
        )
        .await;
        let Response::Entry { attr } = resp else {
            panic!("expected Entry");
        };
        assert_eq!(attr.ino, created_ino);
    }

    #[tokio::test]
    async fn read_write_round_trip() {
        let d = dispatcher();
        let Response::Entry { attr } = handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "f".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };
        let ino = attr.ino;

        let Response::OpenOk { handle: h } = handle(
            &d,
            Request::Open {
                ino,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };

        let Response::Written { bytes } = handle(
            &d,
            Request::Write {
                ino,
                handle: h,
                offset: 0,
                data: Bytes::from_static(b"hello"),
            },
        )
        .await
        else {
            panic!()
        };
        assert_eq!(bytes, 5);

        let Response::Bytes { data, hash } = handle(
            &d,
            Request::Read {
                ino,
                handle: h,
                offset: 0,
                size: 16,
            },
        )
        .await
        else {
            panic!()
        };
        assert_eq!(&data[..], b"hello");
        assert!(hash.verify(&data));

        let r = handle(
            &d,
            Request::Release {
                ino,
                handle: h,
            },
        )
        .await;
        assert_eq!(r, Response::Ok);
    }

    #[tokio::test]
    async fn rename_keeps_ino_stable() {
        let d = dispatcher();
        let Response::Entry { attr } = handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "a".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };
        let original_ino = attr.ino;
        handle(
            &d,
            Request::Rename {
                old_parent: ROOT_INO,
                old_name: "a".into(),
                new_parent: ROOT_INO,
                new_name: "b".into(),
            },
        )
        .await;
        let Response::Entry { attr } = handle(
            &d,
            Request::Lookup {
                parent: ROOT_INO,
                name: "b".into(),
            },
        )
        .await
        else {
            panic!()
        };
        assert_eq!(attr.ino, original_ino);
    }

    #[tokio::test]
    async fn unlink_then_lookup_returns_enoent() {
        let d = dispatcher();
        handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "a".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await;
        handle(
            &d,
            Request::Unlink {
                parent: ROOT_INO,
                name: "a".into(),
            },
        )
        .await;
        let r = handle(
            &d,
            Request::Lookup {
                parent: ROOT_INO,
                name: "a".into(),
            },
        )
        .await;
        assert_eq!(r, Response::Error { errno: e::ENOENT });
    }

    #[tokio::test]
    async fn readdir_assigns_inos_to_children() {
        let d = dispatcher();
        for n in &["x", "y", "z"] {
            handle(
                &d,
                Request::Create {
                    parent: ROOT_INO,
                    name: (*n).into(),
                    mode: 0o644,
                    flags: open_flags(),
                },
            )
            .await;
        }
        let Response::DirPage { entries, .. } = handle(
            &d,
            Request::ReadDir {
                ino: ROOT_INO,
                offset: 0,
            },
        )
        .await
        else {
            panic!()
        };
        assert_eq!(entries.len(), 3);
        let inos: std::collections::HashSet<_> = entries.iter().map(|e| e.ino).collect();
        assert_eq!(inos.len(), 3);
        for e in &entries {
            assert!(e.ino > ROOT_INO);
        }
    }

    #[tokio::test]
    async fn getattr_unknown_ino_returns_enoent() {
        let d = dispatcher();
        let r = handle(&d, Request::GetAttr { ino: 999 }).await;
        assert_eq!(r, Response::Error { errno: e::ENOENT });
    }

    #[tokio::test]
    async fn fsync_on_existing_file_is_ok() {
        let d = dispatcher();
        let Response::Entry { attr } = handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "a".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };
        let r = handle(
            &d,
            Request::Fsync {
                ino: attr.ino,
                handle: 0,
                data_only: false,
            },
        )
        .await;
        assert_eq!(r, Response::Ok);
    }

    #[tokio::test]
    async fn truncate_grows_file() {
        let d = dispatcher();
        let Response::Entry { attr } = handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "a".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };
        handle(
            &d,
            Request::Truncate {
                ino: attr.ino,
                size: 10,
            },
        )
        .await;
        let Response::Attr(a2) = handle(&d, Request::GetAttr { ino: attr.ino }).await else {
            panic!()
        };
        assert_eq!(a2.size, 10);
    }

    #[tokio::test]
    async fn append_handle_writes_at_eof() {
        let d = dispatcher();
        let Response::Entry { attr } = handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "a".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };
        // Pre-fill the file via a regular handle.
        let Response::OpenOk { handle: rw } = handle(
            &d,
            Request::Open {
                ino: attr.ino,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };
        handle(
            &d,
            Request::Write {
                ino: attr.ino,
                handle: rw,
                offset: 0,
                data: Bytes::from_static(b"hello"),
            },
        )
        .await;
        handle(&d, Request::Release { ino: attr.ino, handle: rw }).await;

        // Open with O_APPEND; the offset we pass is irrelevant — the
        // dispatcher must redirect to EOF.
        let append_flags = OpenFlags {
            read: false,
            write: true,
            append: true,
            truncate: false,
        };
        let Response::OpenOk { handle: ah } = handle(
            &d,
            Request::Open {
                ino: attr.ino,
                flags: append_flags,
            },
        )
        .await
        else {
            panic!()
        };
        handle(
            &d,
            Request::Write {
                ino: attr.ino,
                handle: ah,
                offset: 0, // would normally clobber, but APPEND redirects
                data: Bytes::from_static(b"-world"),
            },
        )
        .await;
        let Response::Bytes { data, .. } = handle(
            &d,
            Request::Read {
                ino: attr.ino,
                handle: 0,
                offset: 0,
                size: 64,
            },
        )
        .await
        else {
            panic!()
        };
        assert_eq!(&data[..], b"hello-world");
    }

    #[tokio::test]
    async fn truncate_flag_clears_on_open() {
        let d = dispatcher();
        let Response::Entry { attr } = handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "t".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };
        let h = match handle(
            &d,
            Request::Open {
                ino: attr.ino,
                flags: open_flags(),
            },
        )
        .await
        {
            Response::OpenOk { handle } => handle,
            other => panic!("{other:?}"),
        };
        handle(
            &d,
            Request::Write {
                ino: attr.ino,
                handle: h,
                offset: 0,
                data: Bytes::from_static(b"will be cleared"),
            },
        )
        .await;
        handle(&d, Request::Release { ino: attr.ino, handle: h }).await;

        // Re-open with O_TRUNC (read|write|truncate). Size must drop to 0.
        let trunc_flags = OpenFlags {
            read: true,
            write: true,
            append: false,
            truncate: true,
        };
        handle(
            &d,
            Request::Open {
                ino: attr.ino,
                flags: trunc_flags,
            },
        )
        .await;
        let Response::Attr(a) = handle(&d, Request::GetAttr { ino: attr.ino }).await else {
            panic!()
        };
        assert_eq!(a.size, 0);
    }

    #[tokio::test]
    async fn read_on_writeonly_handle_returns_ebadf() {
        let d = dispatcher();
        let Response::Entry { attr } = handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "w".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };
        let writeonly = OpenFlags {
            read: false,
            write: true,
            append: false,
            truncate: false,
        };
        let Response::OpenOk { handle: h } = handle(
            &d,
            Request::Open {
                ino: attr.ino,
                flags: writeonly,
            },
        )
        .await
        else {
            panic!()
        };
        let r = handle(
            &d,
            Request::Read {
                ino: attr.ino,
                handle: h,
                offset: 0,
                size: 16,
            },
        )
        .await;
        assert_eq!(r, Response::Error { errno: e::EBADF });
    }

    #[tokio::test]
    async fn chmod_changes_mode_bits() {
        let d = dispatcher();
        let Response::Entry { attr } = handle(
            &d,
            Request::Create {
                parent: ROOT_INO,
                name: "x".into(),
                mode: 0o644,
                flags: open_flags(),
            },
        )
        .await
        else {
            panic!()
        };
        handle(
            &d,
            Request::Chmod {
                ino: attr.ino,
                mode: 0o755,
            },
        )
        .await;
        let Response::Attr(a) = handle(&d, Request::GetAttr { ino: attr.ino }).await else {
            panic!()
        };
        assert_eq!(a.mode, 0o755);
    }
}
