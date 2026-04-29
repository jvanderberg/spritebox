//! Wire protocol for the spritebox virtual filesystem.
//!
//! Pure data types only — no I/O, no async, no platform deps. Both the
//! sprite-side (`spritebox-fs-remote`) and host-side (`spritebox-fs-host`)
//! crates depend on this. Test harnesses and the FUSE adapter share the
//! same types so wire bugs surface at the boundary, not after the fact.
//!
//! Design notes:
//! - Inode numbers (`Ino`) are assigned by the host and stable for the
//!   lifetime of a file. The host is the source of truth.
//! - Times are encoded as nanoseconds since the Unix epoch in `i64` for
//!   FUSE compatibility (matches `SystemTime` semantics for negative
//!   epochs).
//! - Byte payloads use `bytes::Bytes` for cheap clones across the
//!   request/response path.
//! - Errors are POSIX errno values so the FUSE adapter can return them
//!   directly without translation.

#![forbid(unsafe_code)]

use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub type Ino = u64;
pub type RequestId = u64;

/// Errno passed all the way back to the kernel via FUSE.
pub type Errno = i32;

pub const ROOT_INO: Ino = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAttr {
    pub ino: Ino,
    pub size: u64,
    pub blocks: u64,
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub kind: FileKind,
    /// POSIX mode bits (permissions only — type bits live in `kind`).
    pub mode: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub ino: Ino,
    pub name: String,
    pub kind: FileKind,
}

/// Open-flag subset we honor. Mapped from FUSE/POSIX open flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
}

/// Client → server request payload.
///
/// `ReadDir.offset` is the opaque cookie returned with the previous page;
/// `0` requests the first page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Lookup {
        parent: Ino,
        name: String,
    },
    GetAttr {
        ino: Ino,
    },
    ReadDir {
        ino: Ino,
        offset: u64,
    },
    Open {
        ino: Ino,
        flags: OpenFlags,
    },
    Release {
        ino: Ino,
        handle: u64,
    },
    Read {
        ino: Ino,
        handle: u64,
        offset: u64,
        size: u32,
    },
    Write {
        ino: Ino,
        handle: u64,
        offset: u64,
        data: Bytes,
    },
    Create {
        parent: Ino,
        name: String,
        mode: u16,
        flags: OpenFlags,
    },
    Mkdir {
        parent: Ino,
        name: String,
        mode: u16,
    },
    Unlink {
        parent: Ino,
        name: String,
    },
    Rmdir {
        parent: Ino,
        name: String,
    },
    Rename {
        old_parent: Ino,
        old_name: String,
        new_parent: Ino,
        new_name: String,
    },
    Truncate {
        ino: Ino,
        size: u64,
    },
    Fsync {
        ino: Ino,
        handle: u64,
        data_only: bool,
    },
    StatFs {
        ino: Ino,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatFs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
}

/// SHA-256 digest of a payload, used for hash-on-receive integrity checks.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sha256(pub [u8; 32]);

impl std::fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sha256:")?;
        for b in &self.0[..4] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…")
    }
}

impl Sha256 {
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Sha256(out)
    }

    pub fn verify(&self, bytes: &[u8]) -> bool {
        Sha256::of(bytes) == *self
    }
}

/// Server → client response payload, paired with a request by `RequestId`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Attr(FileAttr),
    Entry {
        attr: FileAttr,
    },
    DirPage {
        entries: Vec<DirEntry>,
        next_offset: Option<u64>,
    },
    OpenOk {
        handle: u64,
    },
    /// File-content payload with a SHA-256 of the bytes for end-to-end
    /// integrity checking. The receiver MUST verify before serving the
    /// data to a caller; mismatches surface as `EIO` (the transport may
    /// have corrupted the frame).
    Bytes {
        data: Bytes,
        hash: Sha256,
    },
    Written {
        bytes: u32,
    },
    StatFs(StatFs),
    Ok,
    Error {
        errno: Errno,
    },
}

impl Response {
    /// Build a `Bytes` response and compute the hash from `data`.
    pub fn bytes(data: Bytes) -> Self {
        let hash = Sha256::of(&data);
        Response::Bytes { data, hash }
    }
}

/// Out-of-band server-initiated message. Used by the host watcher to keep
/// the sprite's caches coherent with on-disk state. Pushes are advisory:
/// the sprite must tolerate spurious pushes (e.g. dedup) but must not
/// require them for correctness — a request always reflects current state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Push {
    /// Inode metadata changed; drop cached attrs.
    InvalidateAttr { ino: Ino },
    /// Directory entry changed; drop cached lookup result. `ino` is `None`
    /// if the entry was added (no prior cached inode to invalidate).
    InvalidateEntry {
        parent: Ino,
        name: String,
        ino: Option<Ino>,
    },
    /// File contents changed in the given byte range; drop matching cache
    /// chunks. `len == 0` means "invalidate everything for this inode."
    InvalidateData { ino: Ino, offset: u64, len: u64 },
    /// Manifest snapshot epoch — bumped when the host's watcher detects a
    /// change it cannot describe more precisely (e.g. on resync after
    /// reconnect). Sprites should drop their entire metadata cache.
    Resync { epoch: u64 },
}

/// Frame on the wire: either a paired request, a paired response, or an
/// unpaired push. The transport layer is responsible only for delivering
/// frames; ordering across `RequestId`s is not guaranteed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    Request { id: RequestId, body: Request },
    Response { id: RequestId, body: Response },
    Push(Push),
}

/// Standard POSIX errno values we use across the protocol. These match the
/// numeric values on Linux (which is what FUSE expects) — macOS callers
/// should treat them as opaque ints.
pub mod errno {
    use super::Errno;
    pub const EPERM: Errno = 1;
    pub const ENOENT: Errno = 2;
    pub const EIO: Errno = 5;
    pub const EBADF: Errno = 9;
    pub const EAGAIN: Errno = 11;
    pub const EACCES: Errno = 13;
    pub const EBUSY: Errno = 16;
    pub const EEXIST: Errno = 17;
    pub const ENOTDIR: Errno = 20;
    pub const EISDIR: Errno = 21;
    pub const EINVAL: Errno = 22;
    pub const ENFILE: Errno = 23;
    pub const ENOSPC: Errno = 28;
    pub const EROFS: Errno = 30;
    pub const ENAMETOOLONG: Errno = 36;
    pub const ENOSYS: Errno = 38;
    pub const ENOTEMPTY: Errno = 39;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_attr() -> FileAttr {
        FileAttr {
            ino: 42,
            size: 1024,
            blocks: 2,
            atime_ns: 1_700_000_000_000_000_000,
            mtime_ns: 1_700_000_001_000_000_000,
            ctime_ns: 1_700_000_002_000_000_000,
            kind: FileKind::Regular,
            mode: 0o644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
        }
    }

    #[test]
    fn root_ino_is_one() {
        assert_eq!(ROOT_INO, 1);
    }

    #[test]
    fn file_attr_round_trip_via_bincode_compatible_serde() {
        let a = sample_attr();
        let json = serde_json::to_string(&a).unwrap();
        let b: FileAttr = serde_json::from_str(&json).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn frame_request_round_trips() {
        let frame = Frame::Request {
            id: 7,
            body: Request::Read {
                ino: 42,
                handle: 1,
                offset: 4096,
                size: 8192,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: Frame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, back);
    }

    #[test]
    fn frame_response_with_bytes_round_trips() {
        let payload = Bytes::from_static(&[0u8, 1, 2, 3, 4, 0xff, 0xfe]);
        let frame = Frame::Response {
            id: 7,
            body: Response::bytes(payload.clone()),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: Frame = serde_json::from_str(&json).unwrap();
        match back {
            Frame::Response {
                body: Response::Bytes { data, hash },
                ..
            } => {
                assert_eq!(data, payload);
                assert!(hash.verify(&data));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn sha256_detects_corruption() {
        let data = Bytes::from_static(b"hello world");
        let hash = Sha256::of(&data);
        assert!(hash.verify(&data));
        let corrupted = Bytes::from_static(b"hello worle");
        assert!(!hash.verify(&corrupted));
    }

    #[test]
    fn push_round_trips() {
        let p = Push::InvalidateData {
            ino: 9,
            offset: 0,
            len: 0,
        };
        let json = serde_json::to_string(&Frame::Push(p.clone())).unwrap();
        let Frame::Push(back) = serde_json::from_str::<Frame>(&json).unwrap() else {
            panic!("not a push");
        };
        assert_eq!(p, back);
    }

    #[test]
    fn errno_constants_match_linux() {
        assert_eq!(errno::ENOENT, 2);
        assert_eq!(errno::EIO, 5);
        assert_eq!(errno::EROFS, 30);
        assert_eq!(errno::ENOSPC, 28);
    }
}
