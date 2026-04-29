//! Sprite-side filesystem client.
//!
//! This crate sits between the FUSE adapter (above) and the transport
//! (below). It exposes the [`RemoteFs`] trait — the API the FUSE adapter
//! speaks — and provides:
//!
//! - [`CmdClient`]: low-level request/response correlation over a
//!   [`FrameSink`]/[`FrameStream`] pair, plus a routed channel for
//!   [`Push`](spritebox_fs_protocol::Push) frames.
//! - [`PassthroughRemote`]: a minimal `RemoteFs` impl that forwards
//!   every call straight to the host with no caching. Useful for
//!   end-to-end protocol round-trip tests and as the reference against
//!   which cached impls are checked for transparency.
//!
//! A future caching `RemoteFs` impl will layer on top of `CmdClient`
//! to add metadata cache, content cache, write buffering, and prefetch.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use spritebox_fs_protocol::{
    DirEntry, Errno, FileAttr, Frame, Ino, OpenFlags, Push, Request, RequestId, Response, StatFs,
    errno as e,
};
use spritebox_fs_transport::{FrameSink, FrameStream};
use tokio::sync::{Mutex, mpsc, oneshot};

pub mod cache;
pub mod content_cache;
pub use cache::{CacheConfig, CacheStats, CachedRemote};
pub use content_cache::{ContentCache, ContentCacheConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The transport closed before a response arrived.
    Disconnected,
    /// The response arrived but reported an error from the host.
    Errno(Errno),
    /// The response was the wrong shape for the request — protocol bug.
    Protocol,
}

impl ClientError {
    pub fn errno(&self) -> Errno {
        match self {
            ClientError::Disconnected => e::EIO,
            ClientError::Errno(n) => *n,
            ClientError::Protocol => e::EIO,
        }
    }
}

pub type ClientResult<T> = Result<T, ClientError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirPage {
    pub entries: Vec<DirEntry>,
    pub next_offset: Option<u64>,
}

// ---------------------------------------------------------------------------
// CmdClient
// ---------------------------------------------------------------------------

type Pending = std::collections::HashMap<RequestId, oneshot::Sender<ClientResult<Response>>>;

/// Low-level request/response client. Owns the transport's sink for sends
/// and spawns a background task that reads from the stream, dispatching
/// responses to the matching request and pushes to the supplied channel.
pub struct CmdClient<S: FrameSink> {
    sink: Arc<Mutex<S>>,
    pending: Arc<Mutex<Pending>>,
    next_id: AtomicU64,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl<S: FrameSink> CmdClient<S> {
    pub fn new<R: FrameStream>(sink: S, stream: R, push_tx: mpsc::Sender<Push>) -> Arc<Self> {
        let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(Pending::new()));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = Arc::new(Self {
            sink: Arc::new(Mutex::new(sink)),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            closed: closed.clone(),
        });
        spawn_reader(stream, pending, push_tx, closed);
        client
    }

    pub async fn request(&self, body: Request) -> ClientResult<Response> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ClientError::Disconnected);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }
        {
            let mut sink = self.sink.lock().await;
            if sink.send(Frame::Request { id, body }).await.is_err() {
                self.pending.lock().await.remove(&id);
                return Err(ClientError::Disconnected);
            }
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(ClientError::Disconnected),
        }
    }
}

fn spawn_reader<R: FrameStream>(
    mut stream: R,
    pending: Arc<Mutex<Pending>>,
    push_tx: mpsc::Sender<Push>,
    closed: Arc<std::sync::atomic::AtomicBool>,
) {
    tokio::spawn(async move {
        loop {
            match stream.recv().await {
                Some(Frame::Response { id, body }) => {
                    let entry = pending.lock().await.remove(&id);
                    if let Some(tx) = entry {
                        let _ = tx.send(Ok(body));
                    }
                }
                Some(Frame::Push(push)) => {
                    let _ = push_tx.send(push).await;
                }
                Some(Frame::Request { .. }) => {
                    // Server should never send requests on the client channel.
                    // Ignore.
                }
                None => break,
            }
        }
        closed.store(true, Ordering::SeqCst);
        let mut p = pending.lock().await;
        for (_, tx) in p.drain() {
            let _ = tx.send(Err(ClientError::Disconnected));
        }
    });
}

// ---------------------------------------------------------------------------
// RemoteFs trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait RemoteFs: Send + Sync + 'static {
    async fn lookup(&self, parent: Ino, name: &str) -> ClientResult<FileAttr>;
    async fn getattr(&self, ino: Ino) -> ClientResult<FileAttr>;
    async fn readdir(&self, ino: Ino, offset: u64) -> ClientResult<DirPage>;
    async fn open(&self, ino: Ino, flags: OpenFlags) -> ClientResult<u64>;
    async fn release(&self, ino: Ino, handle: u64) -> ClientResult<()>;
    async fn read(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> ClientResult<Bytes>;
    async fn write(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        data: &[u8],
    ) -> ClientResult<u32>;
    async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u16,
        flags: OpenFlags,
    ) -> ClientResult<FileAttr>;
    async fn mkdir(&self, parent: Ino, name: &str, mode: u16) -> ClientResult<FileAttr>;
    async fn unlink(&self, parent: Ino, name: &str) -> ClientResult<()>;
    async fn rmdir(&self, parent: Ino, name: &str) -> ClientResult<()>;
    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
    ) -> ClientResult<()>;
    async fn truncate(&self, ino: Ino, size: u64) -> ClientResult<()>;
    async fn fsync(&self, ino: Ino, handle: u64, data_only: bool) -> ClientResult<()>;
    async fn statfs(&self, ino: Ino) -> ClientResult<StatFs>;
}

// ---------------------------------------------------------------------------
// PassthroughRemote
// ---------------------------------------------------------------------------

/// A minimal `RemoteFs` impl that forwards every call to the host without
/// any caching or buffering. Used as the reference implementation for
/// integration tests and as the cache-transparency oracle.
pub struct PassthroughRemote<S: FrameSink> {
    client: Arc<CmdClient<S>>,
}

impl<S: FrameSink> PassthroughRemote<S> {
    pub fn new(client: Arc<CmdClient<S>>) -> Self {
        Self { client }
    }

    async fn req(&self, body: Request) -> ClientResult<Response> {
        self.client.request(body).await
    }
}

fn unwrap_attr(r: Response) -> ClientResult<FileAttr> {
    match r {
        Response::Attr(a) | Response::Entry { attr: a } => Ok(a),
        Response::Error { errno } => Err(ClientError::Errno(errno)),
        _ => Err(ClientError::Protocol),
    }
}

fn unwrap_ok(r: Response) -> ClientResult<()> {
    match r {
        Response::Ok => Ok(()),
        Response::Error { errno } => Err(ClientError::Errno(errno)),
        _ => Err(ClientError::Protocol),
    }
}

#[async_trait]
impl<S: FrameSink> RemoteFs for PassthroughRemote<S> {
    async fn lookup(&self, parent: Ino, name: &str) -> ClientResult<FileAttr> {
        unwrap_attr(
            self.req(Request::Lookup {
                parent,
                name: name.to_string(),
            })
            .await?,
        )
    }

    async fn getattr(&self, ino: Ino) -> ClientResult<FileAttr> {
        unwrap_attr(self.req(Request::GetAttr { ino }).await?)
    }

    async fn readdir(&self, ino: Ino, offset: u64) -> ClientResult<DirPage> {
        match self.req(Request::ReadDir { ino, offset }).await? {
            Response::DirPage {
                entries,
                next_offset,
            } => Ok(DirPage {
                entries,
                next_offset,
            }),
            Response::Error { errno } => Err(ClientError::Errno(errno)),
            _ => Err(ClientError::Protocol),
        }
    }

    async fn open(&self, ino: Ino, flags: OpenFlags) -> ClientResult<u64> {
        match self.req(Request::Open { ino, flags }).await? {
            Response::OpenOk { handle } => Ok(handle),
            Response::Error { errno } => Err(ClientError::Errno(errno)),
            _ => Err(ClientError::Protocol),
        }
    }

    async fn release(&self, ino: Ino, handle: u64) -> ClientResult<()> {
        unwrap_ok(self.req(Request::Release { ino, handle }).await?)
    }

    async fn read(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> ClientResult<Bytes> {
        match self
            .req(Request::Read {
                ino,
                handle,
                offset,
                size,
            })
            .await?
        {
            Response::Bytes { data, hash } => {
                if !hash.verify(&data) {
                    return Err(ClientError::Errno(e::EIO));
                }
                Ok(data)
            }
            Response::Error { errno } => Err(ClientError::Errno(errno)),
            _ => Err(ClientError::Protocol),
        }
    }

    async fn write(
        &self,
        ino: Ino,
        handle: u64,
        offset: u64,
        data: &[u8],
    ) -> ClientResult<u32> {
        match self
            .req(Request::Write {
                ino,
                handle,
                offset,
                data: Bytes::copy_from_slice(data),
            })
            .await?
        {
            Response::Written { bytes } => Ok(bytes),
            Response::Error { errno } => Err(ClientError::Errno(errno)),
            _ => Err(ClientError::Protocol),
        }
    }

    async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u16,
        flags: OpenFlags,
    ) -> ClientResult<FileAttr> {
        unwrap_attr(
            self.req(Request::Create {
                parent,
                name: name.to_string(),
                mode,
                flags,
            })
            .await?,
        )
    }

    async fn mkdir(&self, parent: Ino, name: &str, mode: u16) -> ClientResult<FileAttr> {
        unwrap_attr(
            self.req(Request::Mkdir {
                parent,
                name: name.to_string(),
                mode,
            })
            .await?,
        )
    }

    async fn unlink(&self, parent: Ino, name: &str) -> ClientResult<()> {
        unwrap_ok(
            self.req(Request::Unlink {
                parent,
                name: name.to_string(),
            })
            .await?,
        )
    }

    async fn rmdir(&self, parent: Ino, name: &str) -> ClientResult<()> {
        unwrap_ok(
            self.req(Request::Rmdir {
                parent,
                name: name.to_string(),
            })
            .await?,
        )
    }

    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
    ) -> ClientResult<()> {
        unwrap_ok(
            self.req(Request::Rename {
                old_parent,
                old_name: old_name.to_string(),
                new_parent,
                new_name: new_name.to_string(),
            })
            .await?,
        )
    }

    async fn truncate(&self, ino: Ino, size: u64) -> ClientResult<()> {
        unwrap_ok(self.req(Request::Truncate { ino, size }).await?)
    }

    async fn fsync(&self, ino: Ino, handle: u64, data_only: bool) -> ClientResult<()> {
        unwrap_ok(
            self.req(Request::Fsync {
                ino,
                handle,
                data_only,
            })
            .await?,
        )
    }

    async fn statfs(&self, ino: Ino) -> ClientResult<StatFs> {
        match self.req(Request::StatFs { ino }).await? {
            Response::StatFs(s) => Ok(s),
            Response::Error { errno } => Err(ClientError::Errno(errno)),
            _ => Err(ClientError::Protocol),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spritebox_fs_host::{Dispatcher, MemFs, mem::FakeClock};
    use spritebox_fs_protocol::ROOT_INO;
    use spritebox_fs_transport::{InMemoryTransport, RealClock};

    fn open_flags() -> OpenFlags {
        OpenFlags {
            read: true,
            write: true,
            append: false,
            truncate: false,
        }
    }

    /// Wire the full sprite ↔ host stack with an in-memory transport.
    /// Returns the client-side `RemoteFs` impl and a guard that keeps
    /// the host loop alive.
    async fn wire() -> (
        PassthroughRemote<spritebox_fs_transport::InMemSink>,
        mpsc::Receiver<Push>,
    ) {
        let clock = Arc::new(RealClock);
        let (client_end, mut server_end, _ctrl_c2s, _ctrl_s2c) = InMemoryTransport::pair(clock);

        // Host-side: dispatcher backed by MemFs.
        let dispatcher = Dispatcher::new(MemFs::with_clock(FakeClock::new(1_000_000)));

        // Spawn host loop: read requests, dispatch, send responses.
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

        let (push_tx, push_rx) = mpsc::channel(16);
        let client = CmdClient::new(client_end.sink, client_end.stream, push_tx);
        (PassthroughRemote::new(client), push_rx)
    }

    #[tokio::test]
    async fn end_to_end_create_lookup() {
        let (rfs, _) = wire().await;
        let attr = rfs
            .create(ROOT_INO, "a", 0o644, open_flags())
            .await
            .unwrap();
        let again = rfs.lookup(ROOT_INO, "a").await.unwrap();
        assert_eq!(attr.ino, again.ino);
    }

    #[tokio::test]
    async fn end_to_end_read_write() {
        let (rfs, _) = wire().await;
        let attr = rfs
            .create(ROOT_INO, "f", 0o644, open_flags())
            .await
            .unwrap();
        let h = rfs.open(attr.ino, open_flags()).await.unwrap();
        let n = rfs.write(attr.ino, h, 0, b"hello").await.unwrap();
        assert_eq!(n, 5);
        let bytes = rfs.read(attr.ino, h, 0, 16).await.unwrap();
        assert_eq!(&bytes[..], b"hello");
        rfs.release(attr.ino, h).await.unwrap();
    }

    #[tokio::test]
    async fn end_to_end_rename_keeps_ino() {
        let (rfs, _) = wire().await;
        let attr = rfs
            .create(ROOT_INO, "a", 0o644, open_flags())
            .await
            .unwrap();
        rfs.rename(ROOT_INO, "a", ROOT_INO, "b").await.unwrap();
        let renamed = rfs.lookup(ROOT_INO, "b").await.unwrap();
        assert_eq!(attr.ino, renamed.ino);
    }

    #[tokio::test]
    async fn lookup_nonexistent_returns_enoent() {
        let (rfs, _) = wire().await;
        let err = rfs.lookup(ROOT_INO, "nope").await.unwrap_err();
        assert_eq!(err, ClientError::Errno(e::ENOENT));
    }

    #[tokio::test]
    async fn readdir_returns_children() {
        let (rfs, _) = wire().await;
        for n in &["x", "y", "z"] {
            rfs.create(ROOT_INO, n, 0o644, open_flags()).await.unwrap();
        }
        let page = rfs.readdir(ROOT_INO, 0).await.unwrap();
        let names: std::collections::HashSet<_> =
            page.entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains("x"));
        assert!(names.contains("y"));
        assert!(names.contains("z"));
    }

    #[tokio::test]
    async fn many_concurrent_requests_correlate_correctly() {
        let (rfs, _) = wire().await;
        rfs.mkdir(ROOT_INO, "d", 0o755).await.unwrap();
        let dir_attr = rfs.lookup(ROOT_INO, "d").await.unwrap();

        // Create 10 files.
        let mut files = Vec::new();
        for i in 0..10 {
            let name = format!("f{i}");
            let attr = rfs
                .create(dir_attr.ino, &name, 0o644, open_flags())
                .await
                .unwrap();
            let h = rfs.open(attr.ino, open_flags()).await.unwrap();
            files.push((attr.ino, h, name, i));
        }

        // Fire 10 writes concurrently.
        let writes = files.iter().map(|(ino, h, _, i)| {
            let payload = format!("payload-{i}").into_bytes();
            let rfs = &rfs;
            let ino = *ino;
            let h = *h;
            async move { rfs.write(ino, h, 0, &payload).await.unwrap() }
        });
        let results = futures_join_all(writes).await;
        for n in results {
            assert!(n > 0);
        }

        // Read them back; verify content is right per file.
        for (ino, h, _name, i) in &files {
            let bytes = rfs.read(*ino, *h, 0, 64).await.unwrap();
            let expected = format!("payload-{i}");
            assert_eq!(&bytes[..], expected.as_bytes());
        }
    }

    /// Tiny join_all to avoid pulling in futures-util.
    async fn futures_join_all<F, T>(iter: impl IntoIterator<Item = F>) -> Vec<T>
    where
        F: std::future::Future<Output = T>,
    {
        let mut out = Vec::new();
        for f in iter {
            out.push(f.await);
        }
        out
    }
}
