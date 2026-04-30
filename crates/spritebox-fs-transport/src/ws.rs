//! WebSocket transport adapter that speaks the Sprites exec channel
//! framing.
//!
//! The Sprites exec WebSocket carries stdio multiplexed by a 1-byte
//! channel prefix per binary frame:
//!
//! - host → sprite (stdin):  `0x00 + body`, terminated by `0x04` (EOF)
//! - sprite → host (stdout): `0x01 + body`
//! - sprite → host (stderr): `0x02 + body` (we ignore this channel)
//! - sprite → host (exit):   `0x03 + code` (we ignore for ongoing ops)
//!
//! Inside the stdio body the sprite-side daemon and host both use the
//! same length-prefixed postcard framing as `crate::stdio` — so reading
//! from the WS strips the 0x01 prefix to recover the same byte stream
//! the daemon's stdin/stdout sees.

use std::collections::VecDeque;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt, stream::SplitSink, stream::SplitStream};
use spritebox_fs_protocol::Frame;
use tokio_tungstenite::{
    WebSocketStream as TwsStream,
    tungstenite::{Message, protocol::CloseFrame},
};

use crate::{FrameSink, FrameStream, TransportError};

const STDIN_PREFIX: u8 = 0x00;
const STDOUT_PREFIX: u8 = 0x01;
const STDIN_EOF_PREFIX: u8 = 0x04;

const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// `FrameSink` over the host side of a Sprites exec WebSocket.
///
/// Every Frame is serialized via postcard, length-prefixed, then wrapped
/// in a `0x00`-prefixed binary WS message (stdin to the daemon).
pub struct WsFrameSink<S> {
    sink: SplitSink<TwsStream<S>, Message>,
}

impl<S> WsFrameSink<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    pub fn new(sink: SplitSink<TwsStream<S>, Message>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl<S> FrameSink for WsFrameSink<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, frame: Frame) -> Result<(), TransportError> {
        let body =
            postcard::to_allocvec(&frame).map_err(|_| TransportError::Closed)?;
        if body.len() as u64 > MAX_FRAME_BYTES as u64 {
            return Err(TransportError::Closed);
        }
        let len = (body.len() as u32).to_be_bytes();
        let mut payload = Vec::with_capacity(1 + 4 + body.len());
        payload.push(STDIN_PREFIX);
        payload.extend_from_slice(&len);
        payload.extend_from_slice(&body);
        self.sink
            .send(Message::Binary(Bytes::from(payload).into()))
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn close(&mut self) {
        // Send EOF marker then close the WebSocket.
        let _ = self
            .sink
            .send(Message::Binary(
                Bytes::from(vec![STDIN_EOF_PREFIX]).into(),
            ))
            .await;
        let _ = self
            .sink
            .send(Message::Close(Some(CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                reason: "shutdown".into(),
            })))
            .await;
    }
}

/// `FrameStream` over the host side of a Sprites exec WebSocket.
///
/// Reads `0x01`-prefixed binary WS messages, accumulates the body bytes,
/// and parses out length-prefixed postcard Frames as they become available.
///
/// Optionally forwards `0x02`-prefixed stderr to a caller-supplied
/// channel so daemon error messages don't get silently dropped.
pub struct WsFrameStream<S> {
    stream: SplitStream<TwsStream<S>>,
    buffer: VecDeque<u8>,
    closed: bool,
    stderr_tx: Option<tokio::sync::mpsc::UnboundedSender<Bytes>>,
}

impl<S> WsFrameStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    pub fn new(stream: SplitStream<TwsStream<S>>) -> Self {
        Self {
            stream,
            buffer: VecDeque::new(),
            closed: false,
            stderr_tx: None,
        }
    }

    /// Forward `0x02` stderr bytes to the given channel instead of
    /// dropping them. Useful for surfacing daemon error output to the
    /// user's terminal.
    pub fn with_stderr(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
    ) -> Self {
        self.stderr_tx = Some(tx);
        self
    }

    /// Pull the next chunk of bytes out of the WS into our buffer.
    /// Returns true if any bytes were added; false on EOF.
    async fn pump(&mut self) -> bool {
        while let Some(msg) = self.stream.next().await {
            match msg {
                Ok(Message::Binary(data)) if !data.is_empty() => {
                    match data[0] {
                        STDOUT_PREFIX => {
                            self.buffer.extend(&data[1..]);
                            return true;
                        }
                        0x02 => {
                            if let Some(tx) = &self.stderr_tx {
                                let _ = tx.send(Bytes::copy_from_slice(&data[1..]));
                            }
                            continue;
                        }
                        // exit-code / unknown — ignore.
                        _ => continue,
                    }
                }
                Ok(Message::Close(_)) | Err(_) => {
                    self.closed = true;
                    return false;
                }
                _ => continue,
            }
        }
        self.closed = true;
        false
    }
}

#[async_trait]
impl<S> FrameStream for WsFrameStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn recv(&mut self) -> Option<Frame> {
        loop {
            // Need 4 bytes for length prefix.
            while self.buffer.len() < 4 {
                if self.closed {
                    return None;
                }
                if !self.pump().await {
                    return None;
                }
            }
            let len_bytes = [
                self.buffer[0],
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
            ];
            let len = u32::from_be_bytes(len_bytes) as usize;
            if len > MAX_FRAME_BYTES as usize {
                return None;
            }
            // Read body.
            while self.buffer.len() < 4 + len {
                if self.closed {
                    return None;
                }
                if !self.pump().await {
                    return None;
                }
            }
            for _ in 0..4 {
                self.buffer.pop_front();
            }
            let body: Vec<u8> = self.buffer.drain(..len).collect();
            return postcard::from_bytes(&body).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spritebox_fs_protocol::{Frame, Push, Request, Response};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::protocol::Role;

    /// Build an in-memory pair of WebSocket streams (client end, server end)
    /// connected via tokio::io::duplex. Each side wraps its half of the
    /// duplex with tokio-tungstenite directly (no handshake) using
    /// `WebSocketStream::from_raw_socket`.
    async fn ws_pair() -> (
        TwsStream<tokio::io::DuplexStream>,
        TwsStream<tokio::io::DuplexStream>,
    ) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let a_ws =
            TwsStream::from_raw_socket(a, Role::Client, None).await;
        let b_ws =
            TwsStream::from_raw_socket(b, Role::Server, None).await;
        (a_ws, b_ws)
    }

    /// Read the next binary message from a WS stream and assert it's
    /// stdin-channel (0x00). Returns the body.
    async fn next_stdin(ws: &mut TwsStream<tokio::io::DuplexStream>) -> Vec<u8> {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(data) => {
                    if data.is_empty() {
                        continue;
                    }
                    assert_eq!(data[0], STDIN_PREFIX);
                    return data[1..].to_vec();
                }
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn send_emits_stdin_prefixed_frame() {
        let (client_ws, mut server_ws) = ws_pair().await;
        let (sink, _stream) = client_ws.split();
        let mut sink = WsFrameSink::new(sink);
        let frame = Frame::Request {
            id: 1,
            body: Request::GetAttr { ino: 42 },
        };
        sink.send(frame.clone()).await.unwrap();

        let body = next_stdin(&mut server_ws).await;
        assert_eq!(body.len(), 4 + postcard::to_allocvec(&frame).unwrap().len());
        // Verify length prefix matches the JSON body length.
        let len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
        assert_eq!(len, body.len() - 4);
        let parsed: Frame = postcard::from_bytes(&body[4..]).unwrap();
        assert_eq!(parsed, frame);
    }

    #[tokio::test]
    async fn recv_decodes_stdout_prefixed_frame() {
        let (mut client_ws, server_ws) = ws_pair().await;
        let (_sink, stream) = server_ws.split();
        let mut stream_wrap = WsFrameStream::new(stream);

        // Client writes a 0x01-prefixed message containing length-prefixed JSON.
        let frame = Frame::Push(Push::InvalidateAttr { ino: 7 });
        let body = postcard::to_allocvec(&frame).unwrap();
        let len = (body.len() as u32).to_be_bytes();
        let mut payload = Vec::with_capacity(1 + 4 + body.len());
        payload.push(STDOUT_PREFIX);
        payload.extend_from_slice(&len);
        payload.extend_from_slice(&body);
        client_ws
            .send(Message::Binary(Bytes::from(payload).into()))
            .await
            .unwrap();

        let received = stream_wrap.recv().await.unwrap();
        assert_eq!(received, frame);
    }

    #[tokio::test]
    async fn recv_handles_split_messages() {
        let (mut client_ws, server_ws) = ws_pair().await;
        let (_sink, stream) = server_ws.split();
        let mut stream_wrap = WsFrameStream::new(stream);

        let frame = Frame::Response {
            id: 9,
            body: Response::bytes(Bytes::from_static(&[0u8, 1, 2, 3, 4, 5, 6, 7])),
        };
        let body = postcard::to_allocvec(&frame).unwrap();
        let len = (body.len() as u32).to_be_bytes();

        // Split the wire bytes into two messages mid-body to exercise
        // the buffering/pumping path.
        let mut wire = Vec::new();
        wire.extend_from_slice(&len);
        wire.extend_from_slice(&body);

        let mid = wire.len() / 2;
        let part1: Vec<u8> = std::iter::once(STDOUT_PREFIX)
            .chain(wire[..mid].iter().copied())
            .collect();
        let part2: Vec<u8> = std::iter::once(STDOUT_PREFIX)
            .chain(wire[mid..].iter().copied())
            .collect();

        client_ws
            .send(Message::Binary(Bytes::from(part1).into()))
            .await
            .unwrap();
        client_ws
            .send(Message::Binary(Bytes::from(part2).into()))
            .await
            .unwrap();

        let received = stream_wrap.recv().await.unwrap();
        assert_eq!(received, frame);
    }

    #[tokio::test]
    async fn recv_ignores_stderr_and_exit() {
        let (mut client_ws, server_ws) = ws_pair().await;
        let (_sink, stream) = server_ws.split();
        let mut stream_wrap = WsFrameStream::new(stream);

        // Stderr garbage that should be ignored.
        client_ws
            .send(Message::Binary(
                Bytes::from(vec![0x02, b'g', b'a', b'r', b'b']).into(),
            ))
            .await
            .unwrap();
        // Exit code that should also be ignored.
        client_ws
            .send(Message::Binary(Bytes::from(vec![0x03, 0]).into()))
            .await
            .unwrap();
        // Now a real stdout frame.
        let frame = Frame::Push(Push::Resync { epoch: 1 });
        let body = postcard::to_allocvec(&frame).unwrap();
        let len = (body.len() as u32).to_be_bytes();
        let mut payload = Vec::with_capacity(1 + 4 + body.len());
        payload.push(STDOUT_PREFIX);
        payload.extend_from_slice(&len);
        payload.extend_from_slice(&body);
        client_ws
            .send(Message::Binary(Bytes::from(payload).into()))
            .await
            .unwrap();

        let received = stream_wrap.recv().await.unwrap();
        assert_eq!(received, frame);
    }

    #[tokio::test]
    async fn recv_returns_none_on_close() {
        let (mut client_ws, server_ws) = ws_pair().await;
        let (_sink, stream) = server_ws.split();
        let mut stream_wrap = WsFrameStream::new(stream);
        client_ws.close(None).await.unwrap();
        let r = stream_wrap.recv().await;
        assert!(r.is_none());
    }

    /// Force the duplex out of scope by ensuring it's referenced — this
    /// is just an anchor for tokio::io traits used at top of file.
    #[tokio::test]
    async fn duplex_io_traits_in_scope() {
        let (mut a, mut b) = tokio::io::duplex(16);
        a.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [b'x']);
    }
}
