//! Stdio-framed transport adapter.
//!
//! Wraps any [`AsyncRead`] + [`AsyncWrite`] pair as a [`FrameSink`] /
//! [`FrameStream`]. Framing is `[u32 length][Frame as postcard]`.
//! Postcard's varint encoding leaves binary `Bytes` payloads on the
//! wire as raw bytes (5-byte length prefix + payload), eliminating
//! the ~3× tax JSON imposed on `Read` responses.
//!
//! This is the production transport on the sprite side: the daemon
//! reads frames from stdin and writes to stdout. Carried byte-for-byte
//! by the Sprites exec WebSocket (verified binary-clean by the
//! `ws_binary_roundtrip` probe).

use async_trait::async_trait;
use spritebox_fs_protocol::Frame;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::{FrameSink, FrameStream, TransportError};

const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024; // 64 MiB hard cap

pub struct StdioSink<W: AsyncWrite + Send + Unpin + 'static> {
    inner: Mutex<W>,
    closed: std::sync::atomic::AtomicBool,
}

impl<W: AsyncWrite + Send + Unpin + 'static> StdioSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: Mutex::new(writer),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl<W: AsyncWrite + Send + Unpin + 'static> FrameSink for StdioSink<W> {
    async fn send(&mut self, frame: Frame) -> Result<(), TransportError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(TransportError::Closed);
        }
        let body =
            postcard::to_allocvec(&frame).map_err(|_| TransportError::Closed)?;
        if body.len() as u64 > MAX_FRAME_BYTES as u64 {
            return Err(TransportError::Closed);
        }
        let len = body.len() as u32;
        let mut g = self.inner.lock().await;
        g.write_all(&len.to_be_bytes()).await.map_err(io_to_err)?;
        g.write_all(&body).await.map_err(io_to_err)?;
        g.flush().await.map_err(io_to_err)
    }

    async fn close(&mut self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

pub struct StdioStream<R: AsyncRead + Send + Unpin + 'static> {
    inner: R,
}

impl<R: AsyncRead + Send + Unpin + 'static> StdioStream<R> {
    pub fn new(reader: R) -> Self {
        Self { inner: reader }
    }
}

#[async_trait]
impl<R: AsyncRead + Send + Unpin + 'static> FrameStream for StdioStream<R> {
    async fn recv(&mut self) -> Option<Frame> {
        let mut len_buf = [0u8; 4];
        if self.inner.read_exact(&mut len_buf).await.is_err() {
            return None;
        }
        let len = u32::from_be_bytes(len_buf);
        if len > MAX_FRAME_BYTES {
            return None;
        }
        let mut body = vec![0u8; len as usize];
        if self.inner.read_exact(&mut body).await.is_err() {
            return None;
        }
        postcard::from_bytes(&body).ok()
    }
}

fn io_to_err(_err: std::io::Error) -> TransportError {
    TransportError::Closed
}

#[cfg(test)]
mod tests {
    use super::*;
    use spritebox_fs_protocol::{Frame, Push, Request, Response};
    use tokio::io::duplex;

    /// Build a one-direction stdio pipe. `a` is the write side, `b` is
    /// the read side. The unused halves are dropped to ensure EOF
    /// propagates if the writer closes.
    fn pipe() -> (StdioSink<tokio::io::WriteHalf<tokio::io::DuplexStream>>, StdioStream<tokio::io::ReadHalf<tokio::io::DuplexStream>>) {
        let (a, b) = duplex(64 * 1024);
        let (a_read, a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);
        // Drop unused halves explicitly via `drop()` — `let _x = ...` would
        // keep the binding alive to end of scope, which is the opposite of
        // what we want.
        drop(a_read);
        drop(b_write);
        (StdioSink::new(a_write), StdioStream::new(b_read))
    }

    #[tokio::test]
    async fn round_trip_simple_frame() {
        let (mut sink, mut stream) = pipe();
        let frame = Frame::Request {
            id: 42,
            body: Request::GetAttr { ino: 7 },
        };
        sink.send(frame.clone()).await.unwrap();
        let received = stream.recv().await.unwrap();
        assert_eq!(received, frame);
    }

    #[tokio::test]
    async fn round_trip_bytes_response() {
        let (mut sink, mut stream) = pipe();
        let frame = Frame::Response {
            id: 1,
            body: Response::bytes(bytes::Bytes::from_static(&[0u8, 1, 2, 3, 4, 0xff]), 0),
        };
        sink.send(frame.clone()).await.unwrap();
        let back = stream.recv().await.unwrap();
        assert_eq!(back, frame);
    }

    #[tokio::test]
    async fn multiple_frames_in_a_row() {
        let (mut sink, mut stream) = pipe();
        for i in 0..10u64 {
            sink.send(Frame::Push(Push::InvalidateAttr { ino: i }))
                .await
                .unwrap();
        }
        for i in 0..10u64 {
            let f = stream.recv().await.unwrap();
            assert_eq!(f, Frame::Push(Push::InvalidateAttr { ino: i }));
        }
    }

    #[tokio::test]
    async fn closed_stream_returns_none() {
        let (sink, mut stream) = pipe();
        // Drop the sink's writer. The reader should see EOF.
        drop(sink);
        let f = stream.recv().await;
        assert!(f.is_none());
    }
}
