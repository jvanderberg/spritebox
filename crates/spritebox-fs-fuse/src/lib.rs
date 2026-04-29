//! FUSE adapter for the spritebox virtual filesystem.
//!
//! Linux-only — on other platforms this crate compiles to nothing so the
//! workspace stays whole. The adapter is the only piece of the stack
//! that depends on `fuser` / `/dev/fuse` / a kernel mount.
//!
//! Design rule: this crate is a **thin translation layer**. All caching,
//! buffering, request correlation, and integrity checking live in
//! `spritebox-fs-remote`. The FUSE adapter only:
//!
//! 1. Receives kernel callbacks via the `fuser::Filesystem` trait.
//! 2. Translates each callback's arguments to a `RemoteFs` call.
//! 3. Maps the resulting `ClientResult` to the appropriate `Reply*::data` /
//!    `Reply*::error`.
//! 4. Wraps each callback in a `tracing::span!` for diagnostics.
//!
//! Per the design doc, every callback follows the same pattern, factored
//! into the `fuse_call!` macro. Adding a new callback is "match the FUSE
//! signature, call the macro, done."

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr as FuseAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
};
use spritebox_fs_protocol::{
    FileAttr as PAttr, FileKind, Ino, OpenFlags, ROOT_INO, errno as e,
};
use spritebox_fs_remote::{ClientError, RemoteFs};
use tokio::runtime::Handle;

/// FUSE-side default TTLs for kernel-level caching. The real cache TTLs
/// live in `spritebox-fs-remote`; these tell the kernel how long to
/// trust a FUSE reply.
const ENTRY_TTL: Duration = Duration::from_secs(1);
const ATTR_TTL: Duration = Duration::from_secs(1);
const GENERATION: u64 = 1;

/// FUSE-mountable adapter. Constructed with a runtime handle (so callbacks
/// can `block_on` the async `RemoteFs` calls) and an `Arc<dyn RemoteFs>`.
pub struct SpriteboxFs {
    runtime: Handle,
    remote: Arc<dyn RemoteFs>,
}

impl SpriteboxFs {
    pub fn new(runtime: Handle, remote: Arc<dyn RemoteFs>) -> Self {
        Self { runtime, remote }
    }
}

fn to_fuse_attr(a: &PAttr) -> FuseAttr {
    let kind = match a.kind {
        FileKind::Regular => FileType::RegularFile,
        FileKind::Directory => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
    };
    let to_st = |ns: i64| -> SystemTime {
        if ns >= 0 {
            SystemTime::UNIX_EPOCH + Duration::from_nanos(ns as u64)
        } else {
            SystemTime::UNIX_EPOCH - Duration::from_nanos((-ns) as u64)
        }
    };
    FuseAttr {
        ino: a.ino,
        size: a.size,
        blocks: a.blocks,
        atime: to_st(a.atime_ns),
        mtime: to_st(a.mtime_ns),
        ctime: to_st(a.ctime_ns),
        crtime: to_st(a.ctime_ns),
        kind,
        perm: a.mode,
        nlink: a.nlink,
        uid: a.uid,
        gid: a.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn errno_for(err: &ClientError) -> i32 {
    err.errno()
}

fn open_flags_from(flags: i32) -> OpenFlags {
    let mode = flags & libc::O_ACCMODE;
    OpenFlags {
        read: mode == libc::O_RDONLY || mode == libc::O_RDWR,
        write: mode == libc::O_WRONLY || mode == libc::O_RDWR,
        append: flags & libc::O_APPEND != 0,
        truncate: flags & libc::O_TRUNC != 0,
    }
}

/// Each callback funnels through this macro: open a tracing span, time
/// the call, run the async `RemoteFs` op, map errors, reply.
///
/// Usage: `fuse_call!(self, span_name, ino_or_path, async_block, on_ok)`
macro_rules! fuse_call {
    ($self:expr, $span:expr, { $($field:tt)* }, $fut:expr, $reply:expr, $ok:expr) => {{
        let span = tracing::info_span!($span, $($field)*);
        let _enter = span.enter();
        let t0 = std::time::Instant::now();
        let result = $self.runtime.block_on($fut);
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        match result {
            Ok(value) => {
                tracing::info!(elapsed_ms, "ok");
                $ok($reply, value);
            }
            Err(err) => {
                let errno = errno_for(&err);
                tracing::warn!(elapsed_ms, errno, "err");
                $reply.error(errno);
            }
        }
    }};
}

impl Filesystem for SpriteboxFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(e::EINVAL);
                return;
            }
        };
        let remote = self.remote.clone();
        let span = tracing::info_span!("fuse.lookup", parent, name = %name_str);
        let _enter = span.enter();
        let t0 = std::time::Instant::now();
        let result = self.runtime.block_on(remote.lookup(parent as Ino, &name_str));
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        match result {
            Ok(attr) => {
                tracing::info!(elapsed_ms, ino = attr.ino, "ok");
                reply.entry(&ENTRY_TTL, &to_fuse_attr(&attr), GENERATION);
            }
            Err(err) => {
                let errno = errno_for(&err);
                tracing::warn!(elapsed_ms, errno, "err");
                reply.error(errno);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let remote = self.remote.clone();
        fuse_call!(
            self,
            "fuse.getattr",
            { ino },
            remote.getattr(ino as Ino),
            reply,
            |reply: ReplyAttr, attr: PAttr| reply.attr(&ATTR_TTL, &to_fuse_attr(&attr))
        );
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let remote = self.remote.clone();
        let of = open_flags_from(flags);
        fuse_call!(
            self,
            "fuse.open",
            { ino },
            remote.open(ino as Ino, of),
            reply,
            |reply: ReplyOpen, handle: u64| reply.opened(handle, 0)
        );
    }

    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let remote = self.remote.clone();
        fuse_call!(
            self,
            "fuse.release",
            { ino, fh },
            remote.release(ino as Ino, fh),
            reply,
            |reply: ReplyEmpty, _: ()| reply.ok()
        );
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let remote = self.remote.clone();
        fuse_call!(
            self,
            "fuse.read",
            { ino, fh, offset, size },
            remote.read(ino as Ino, fh, offset as u64, size),
            reply,
            |reply: ReplyData, bytes: bytes::Bytes| reply.data(&bytes)
        );
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let remote = self.remote.clone();
        let data = data.to_vec();
        let len = data.len();
        let span = tracing::info_span!("fuse.write", ino, fh, offset, len);
        let _enter = span.enter();
        let t0 = std::time::Instant::now();
        let result = self
            .runtime
            .block_on(remote.write(ino as Ino, fh, offset as u64, &data));
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        match result {
            Ok(written) => {
                tracing::info!(elapsed_ms, written, "ok");
                reply.written(written);
            }
            Err(err) => {
                let errno = errno_for(&err);
                tracing::warn!(elapsed_ms, errno, "err");
                reply.error(errno);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let remote = self.remote.clone();
        let span = tracing::info_span!("fuse.readdir", ino, offset);
        let _enter = span.enter();
        let result = self
            .runtime
            .block_on(remote.readdir(ino as Ino, offset as u64));
        match result {
            Ok(page) => {
                let mut idx = offset as u64;
                for entry in page.entries {
                    idx += 1;
                    let kind = match entry.kind {
                        FileKind::Regular => FileType::RegularFile,
                        FileKind::Directory => FileType::Directory,
                        FileKind::Symlink => FileType::Symlink,
                    };
                    if reply.add(entry.ino, idx as i64, kind, &entry.name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(err) => {
                reply.error(errno_for(&err));
            }
        }
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let remote = self.remote.clone();
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(e::EINVAL);
                return;
            }
        };
        let of = open_flags_from(flags);
        let span = tracing::info_span!("fuse.create", parent, name = %name_str);
        let _enter = span.enter();
        let result = self.runtime.block_on(async {
            let attr = remote
                .create(parent as Ino, &name_str, (mode & 0o7777) as u16, of)
                .await?;
            let handle = remote.open(attr.ino, of).await?;
            Ok::<(_, u64), ClientError>((attr, handle))
        });
        match result {
            Ok((attr, handle)) => {
                tracing::info!(ino = attr.ino, handle, "ok");
                reply.created(
                    &ENTRY_TTL,
                    &to_fuse_attr(&attr),
                    GENERATION,
                    handle,
                    0,
                );
            }
            Err(err) => {
                reply.error(errno_for(&err));
            }
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let remote = self.remote.clone();
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(e::EINVAL);
                return;
            }
        };
        fuse_call!(
            self,
            "fuse.mkdir",
            { parent, name = name_str.as_str() },
            remote.mkdir(parent as Ino, &name_str, (mode & 0o7777) as u16),
            reply,
            |reply: ReplyEntry, attr: PAttr| reply.entry(&ENTRY_TTL, &to_fuse_attr(&attr), GENERATION)
        );
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let remote = self.remote.clone();
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(e::EINVAL);
                return;
            }
        };
        fuse_call!(
            self,
            "fuse.unlink",
            { parent, name = name_str.as_str() },
            remote.unlink(parent as Ino, &name_str),
            reply,
            |reply: ReplyEmpty, _: ()| reply.ok()
        );
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let remote = self.remote.clone();
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(e::EINVAL);
                return;
            }
        };
        fuse_call!(
            self,
            "fuse.rmdir",
            { parent, name = name_str.as_str() },
            remote.rmdir(parent as Ino, &name_str),
            reply,
            |reply: ReplyEmpty, _: ()| reply.ok()
        );
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let remote = self.remote.clone();
        let from = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(e::EINVAL);
                return;
            }
        };
        let to = match newname.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(e::EINVAL);
                return;
            }
        };
        fuse_call!(
            self,
            "fuse.rename",
            { parent, newparent, from = from.as_str(), to = to.as_str() },
            remote.rename(parent as Ino, &from, newparent as Ino, &to),
            reply,
            |reply: ReplyEmpty, _: ()| reply.ok()
        );
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let remote = self.remote.clone();
        let span = tracing::info_span!("fuse.setattr", ino);
        let _enter = span.enter();
        let result = self.runtime.block_on(async {
            if let Some(s) = size {
                remote.truncate(ino as Ino, s).await?;
            }
            remote.getattr(ino as Ino).await
        });
        match result {
            Ok(attr) => reply.attr(&ATTR_TTL, &to_fuse_attr(&attr)),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn fsync(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let remote = self.remote.clone();
        fuse_call!(
            self,
            "fuse.fsync",
            { ino, fh, datasync },
            remote.fsync(ino as Ino, fh, datasync),
            reply,
            |reply: ReplyEmpty, _: ()| reply.ok()
        );
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        let remote = self.remote.clone();
        let span = tracing::info_span!("fuse.statfs");
        let _enter = span.enter();
        let result = self.runtime.block_on(remote.statfs(ROOT_INO));
        match result {
            Ok(st) => reply.statfs(
                st.blocks,
                st.bfree,
                st.bavail,
                st.files,
                st.ffree,
                st.bsize,
                st.namelen,
                st.bsize,
            ),
            Err(err) => reply.error(errno_for(&err)),
        }
    }
}
