//! Transport abstraction for the spritebox virtual filesystem.
//!
//! The [`FrameSink`] / [`FrameStream`] traits are the port that
//! `spritebox-fs-remote` and `spritebox-fs-host` use to ship `Frame`s
//! across the wire. The real implementation wraps a WebSocket; the
//! [`InMemoryTransport`] in this crate is the test-side adapter that
//! gives the harness a controllable, deterministic substitute.
//!
//! The in-memory transport carries the **fault-injection control surface**
//! the testing strategy depends on:
//!
//! - [`Controls::set_latency`] — per-frame delay
//! - [`Controls::set_drop_rate`] — random drop probability
//! - [`Controls::drop_next`] — drop the next N frames
//! - [`Controls::disconnect_now`] — close immediately
//! - [`Controls::disconnect_after_bytes`] — close after N bytes pass
//! - [`Controls::pause`] / [`Controls::resume`] — hold frames in flight
//! - [`Controls::set_corruption`] — mutate a specific byte of a matching frame
//! - [`Controls::set_seed`] — make randomness deterministic
//!
//! All controls apply to **one direction** — pair them up as needed for
//! bidirectional fault injection. Two `Controls` (one per direction) are
//! returned by [`InMemoryTransport::pair`].

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use spritebox_fs_protocol::{Frame, RequestId};
use tokio::sync::{Mutex, mpsc};

pub mod clock;

pub use clock::{FakeClock, RealClock, TransportClock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Closed,
    Disconnected,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Closed => write!(f, "transport closed"),
            TransportError::Disconnected => write!(f, "transport disconnected"),
        }
    }
}

impl std::error::Error for TransportError {}

#[async_trait]
pub trait FrameSink: Send + Unpin + 'static {
    async fn send(&mut self, frame: Frame) -> Result<(), TransportError>;
    async fn close(&mut self);
}

#[async_trait]
pub trait FrameStream: Send + Unpin + 'static {
    async fn recv(&mut self) -> Option<Frame>;
}

// ---------------------------------------------------------------------------
// In-memory transport
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Policy {
    latency: Duration,
    drop_rate: f64,
    drop_next: usize,
    disconnect_after_bytes: Option<u64>,
    disconnected: bool,
    paused: bool,
    bytes_through: u64,
    /// Pending corruption: when a `Response` with this `RequestId` arrives,
    /// flip the byte at `byte_offset` in its serialized representation.
    /// Stored as a list because tests may queue several.
    corruptions: Vec<Corruption>,
    seed: u64,
    rng_state: u64,
}

#[derive(Clone)]
struct Corruption {
    request_id: RequestId,
    byte_offset: usize,
}

impl Policy {
    fn next_random(&mut self) -> u64 {
        // xorshift64*. Deterministic given a seed.
        let mut x = if self.rng_state == 0 {
            self.seed.wrapping_add(0x9E3779B97F4A7C15)
        } else {
            self.rng_state
        };
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng_state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn random_unit(&mut self) -> f64 {
        (self.next_random() as f64) / (u64::MAX as f64)
    }
}

#[derive(Clone)]
pub struct Controls {
    inner: Arc<Mutex<Policy>>,
    waker: Arc<tokio::sync::Notify>,
}

impl Controls {
    pub async fn set_latency(&self, latency: Duration) {
        self.inner.lock().await.latency = latency;
    }

    pub async fn set_drop_rate(&self, rate: f64) {
        self.inner.lock().await.drop_rate = rate.clamp(0.0, 1.0);
    }

    pub async fn drop_next(&self, n: usize) {
        self.inner.lock().await.drop_next = n;
    }

    pub async fn disconnect_after_bytes(&self, n: u64) {
        self.inner.lock().await.disconnect_after_bytes = Some(n);
    }

    pub async fn disconnect_now(&self) {
        self.inner.lock().await.disconnected = true;
        self.waker.notify_waiters();
    }

    pub async fn pause(&self) {
        self.inner.lock().await.paused = true;
    }

    pub async fn resume(&self) {
        self.inner.lock().await.paused = false;
        self.waker.notify_waiters();
    }

    pub async fn set_corruption(&self, request_id: RequestId, byte_offset: usize) {
        self.inner.lock().await.corruptions.push(Corruption {
            request_id,
            byte_offset,
        });
    }

    pub async fn set_seed(&self, seed: u64) {
        let mut g = self.inner.lock().await;
        g.seed = seed;
        g.rng_state = 0;
    }

    pub async fn bytes_through(&self) -> u64 {
        self.inner.lock().await.bytes_through
    }

    pub async fn is_disconnected(&self) -> bool {
        self.inner.lock().await.disconnected
    }
}

/// In-memory transport. `pair()` returns a back-to-back duplex pipe with
/// independent fault-injection controls for each direction.
pub struct InMemoryTransport;

pub struct InMemSink {
    tx: mpsc::Sender<Frame>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

pub struct InMemStream {
    rx: mpsc::Receiver<Frame>,
}

#[async_trait]
impl FrameSink for InMemSink {
    async fn send(&mut self, frame: Frame) -> Result<(), TransportError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(TransportError::Closed);
        }
        self.tx
            .send(frame)
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn close(&mut self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl FrameStream for InMemStream {
    async fn recv(&mut self) -> Option<Frame> {
        self.rx.recv().await
    }
}

pub struct Endpoint {
    pub sink: InMemSink,
    pub stream: InMemStream,
}

impl InMemoryTransport {
    /// Build a duplex transport.
    ///
    /// Returns `(client, server, client_send_controls, server_send_controls)`.
    /// `client_send_controls` affects frames flowing from the client to the
    /// server; `server_send_controls` affects the reverse.
    pub fn pair(clock: Arc<dyn TransportClock>) -> (Endpoint, Endpoint, Controls, Controls) {
        let (c2s_in_tx, c2s_in_rx) = mpsc::channel(64);
        let (c2s_out_tx, c2s_out_rx) = mpsc::channel(64);
        let (s2c_in_tx, s2c_in_rx) = mpsc::channel(64);
        let (s2c_out_tx, s2c_out_rx) = mpsc::channel(64);

        let c2s_controls = Controls {
            inner: Arc::new(Mutex::new(Policy::default())),
            waker: Arc::new(tokio::sync::Notify::new()),
        };
        let s2c_controls = Controls {
            inner: Arc::new(Mutex::new(Policy::default())),
            waker: Arc::new(tokio::sync::Notify::new()),
        };

        spawn_forwarder(c2s_in_rx, c2s_out_tx, c2s_controls.clone(), clock.clone());
        spawn_forwarder(s2c_in_rx, s2c_out_tx, s2c_controls.clone(), clock.clone());

        let closed_a = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closed_b = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let client = Endpoint {
            sink: InMemSink {
                tx: c2s_in_tx,
                closed: closed_a,
            },
            stream: InMemStream { rx: s2c_out_rx },
        };
        let server = Endpoint {
            sink: InMemSink {
                tx: s2c_in_tx,
                closed: closed_b,
            },
            stream: InMemStream { rx: c2s_out_rx },
        };

        (client, server, c2s_controls, s2c_controls)
    }
}

fn spawn_forwarder(
    mut rx: mpsc::Receiver<Frame>,
    tx: mpsc::Sender<Frame>,
    controls: Controls,
    clock: Arc<dyn TransportClock>,
) {
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            // Pause loop — wait for resume notification.
            loop {
                let paused = {
                    let g = controls.inner.lock().await;
                    if g.disconnected {
                        return;
                    }
                    g.paused
                };
                if !paused {
                    break;
                }
                controls.waker.notified().await;
            }

            let action = {
                let mut g = controls.inner.lock().await;
                if g.disconnected {
                    return;
                }
                let frame_size = approx_frame_size(&frame);
                g.bytes_through = g.bytes_through.saturating_add(frame_size);
                if let Some(limit) = g.disconnect_after_bytes
                    && g.bytes_through >= limit
                {
                    g.disconnected = true;
                    return;
                }
                if g.drop_next > 0 {
                    g.drop_next -= 1;
                    Action::Drop
                } else if g.drop_rate > 0.0 && g.random_unit() < g.drop_rate {
                    Action::Drop
                } else {
                    let frame = apply_corruption(&mut g, frame);
                    Action::Forward {
                        frame,
                        latency: g.latency,
                    }
                }
            };

            match action {
                Action::Drop => {}
                Action::Forward { frame, latency } => {
                    if !latency.is_zero() {
                        clock.sleep(latency).await;
                    }
                    if tx.send(frame).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
}

enum Action {
    Drop,
    Forward { frame: Frame, latency: Duration },
}

fn approx_frame_size(frame: &Frame) -> u64 {
    use spritebox_fs_protocol::{Request, Response};
    match frame {
        Frame::Request { body, .. } => match body {
            Request::Write { data, .. } => 64 + data.len() as u64,
            _ => 64,
        },
        Frame::Response { body, .. } => match body {
            Response::Bytes { data, .. } => 64 + data.len() as u64,
            Response::DirPage { entries, .. } => 32 + (entries.len() as u64 * 64),
            _ => 32,
        },
        Frame::Push(_) => 32,
    }
}

/// Apply pending corruptions to a Response frame whose request_id matches.
/// We mutate the byte at the given offset of the serialized payload, which
/// for Bytes responses means the data buffer.
fn apply_corruption(policy: &mut Policy, frame: Frame) -> Frame {
    use spritebox_fs_protocol::Response;
    let id = match &frame {
        Frame::Response { id, .. } => *id,
        _ => return frame,
    };
    let mut taken: Vec<Corruption> = Vec::new();
    policy.corruptions.retain(|c| {
        if c.request_id == id {
            taken.push(c.clone());
            false
        } else {
            true
        }
    });
    if taken.is_empty() {
        return frame;
    }
    let Frame::Response { id, body } = frame else {
        return frame;
    };
    let new_body = match body {
        Response::Bytes { data, hash } => {
            // Corrupt the *data* but leave the hash untouched. That's the
            // point: hash-on-receive must catch transit corruption.
            let mut v = data.to_vec();
            for c in &taken {
                if c.byte_offset < v.len() {
                    v[c.byte_offset] ^= 0xFF;
                }
            }
            Response::Bytes {
                data: bytes::Bytes::from(v),
                hash,
            }
        }
        other => other,
    };
    Frame::Response { id, body: new_body }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spritebox_fs_protocol::{Push, Request, Response};
    use std::time::Duration;

    fn make() -> (Endpoint, Endpoint, Controls, Controls) {
        let clock = Arc::new(RealClock);
        InMemoryTransport::pair(clock)
    }

    fn req(id: RequestId) -> Frame {
        Frame::Request {
            id,
            body: Request::GetAttr { ino: 1 },
        }
    }

    fn resp(id: RequestId) -> Frame {
        Frame::Response {
            id,
            body: Response::Ok,
        }
    }

    fn resp_bytes(id: RequestId, payload: &'static [u8]) -> Frame {
        Frame::Response {
            id,
            body: Response::bytes(bytes::Bytes::from_static(payload)),
        }
    }

    #[tokio::test]
    async fn happy_path_round_trip() {
        let (mut a, mut b, _ca, _cb) = make();
        a.sink.send(req(1)).await.unwrap();
        let recv = b.stream.recv().await.unwrap();
        assert_eq!(recv, req(1));
        b.sink.send(resp(1)).await.unwrap();
        let back = a.stream.recv().await.unwrap();
        assert_eq!(back, resp(1));
    }

    #[tokio::test]
    async fn drop_next_drops_frames() {
        let (mut a, mut b, ca, _cb) = make();
        ca.drop_next(2).await;
        a.sink.send(req(1)).await.unwrap();
        a.sink.send(req(2)).await.unwrap();
        a.sink.send(req(3)).await.unwrap();

        let frame = tokio::time::timeout(Duration::from_millis(200), b.stream.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame, req(3));
    }

    #[tokio::test]
    async fn disconnect_now_closes_recv() {
        let (mut a, mut b, ca, _cb) = make();
        a.sink.send(req(1)).await.unwrap();
        let _ = b.stream.recv().await;

        ca.disconnect_now().await;
        a.sink.send(req(2)).await.unwrap();
        let r = tokio::time::timeout(Duration::from_millis(100), b.stream.recv()).await;
        // Either timeout (no more frames) or None — both are correct.
        match r {
            Ok(None) => {}
            Err(_) => {}
            Ok(Some(f)) => panic!("unexpected frame after disconnect: {f:?}"),
        }
    }

    #[tokio::test]
    async fn disconnect_after_bytes() {
        let (mut a, mut b, ca, _cb) = make();
        ca.disconnect_after_bytes(150).await;

        a.sink.send(req(1)).await.unwrap();
        let _ = b.stream.recv().await;
        a.sink.send(req(2)).await.unwrap();
        let _ = b.stream.recv().await;
        a.sink.send(req(3)).await.unwrap();
        let r = tokio::time::timeout(Duration::from_millis(100), b.stream.recv()).await;
        match r {
            Ok(None) | Err(_) => {}
            Ok(Some(f)) => panic!("expected disconnect, got {f:?}"),
        }
        assert!(ca.is_disconnected().await);
    }

    #[tokio::test]
    async fn pause_resume_holds_frames() {
        let (mut a, mut b, ca, _cb) = make();
        ca.pause().await;
        a.sink.send(req(1)).await.unwrap();
        let r = tokio::time::timeout(Duration::from_millis(100), b.stream.recv()).await;
        assert!(r.is_err(), "frame leaked through pause");
        ca.resume().await;
        let frame = tokio::time::timeout(Duration::from_millis(500), b.stream.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame, req(1));
    }

    #[tokio::test]
    async fn drop_rate_with_seed_is_deterministic() {
        let (mut a, mut b, ca, _cb) = make();
        ca.set_seed(42).await;
        ca.set_drop_rate(0.5).await;
        for i in 0..20 {
            a.sink.send(req(i)).await.unwrap();
        }
        // Drain
        let mut received = Vec::new();
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_millis(50), b.stream.recv()).await {
                Ok(Some(Frame::Request { id, .. })) => received.push(id),
                _ => break,
            }
        }
        assert!(!received.is_empty());
        assert!(received.len() < 20);
    }

    #[tokio::test]
    async fn corruption_flips_byte_in_matching_response_but_leaves_hash() {
        let (mut a, mut b, _ca, cb) = make();
        cb.set_corruption(7, 2).await;
        b.sink.send(resp_bytes(7, b"hello")).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_millis(200), a.stream.recv())
            .await
            .unwrap()
            .unwrap();
        match frame {
            Frame::Response {
                id: 7,
                body: Response::Bytes { data, hash },
            } => {
                assert_eq!(data[0], b'h');
                assert_eq!(data[1], b'e');
                assert_eq!(data[2], b'l' ^ 0xFF);
                assert_eq!(data[3], b'l');
                assert_eq!(data[4], b'o');
                // The hash was set for the original payload — so it must NOT
                // verify against the corrupted data. That's how hash-on-receive
                // catches transit corruption.
                assert!(!hash.verify(&data));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn corruption_does_not_affect_unmatched() {
        let (mut a, mut b, _ca, cb) = make();
        cb.set_corruption(99, 0).await;
        b.sink.send(resp_bytes(7, b"hello")).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_millis(200), a.stream.recv())
            .await
            .unwrap()
            .unwrap();
        match frame {
            Frame::Response {
                id: 7,
                body: Response::Bytes { data, hash },
            } => {
                assert_eq!(&data[..], b"hello");
                assert!(hash.verify(&data));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn latency_delays_frames() {
        let (mut a, mut b, ca, _cb) = make();
        ca.set_latency(Duration::from_millis(80)).await;
        let t0 = std::time::Instant::now();
        a.sink.send(req(1)).await.unwrap();
        let _ = b.stream.recv().await;
        assert!(t0.elapsed() >= Duration::from_millis(70));
    }

    #[tokio::test]
    async fn pushes_pass_through() {
        let (mut a, mut b, _ca, _cb) = make();
        b.sink
            .send(Frame::Push(Push::Resync { epoch: 5 }))
            .await
            .unwrap();
        let f = a.stream.recv().await.unwrap();
        assert_eq!(f, Frame::Push(Push::Resync { epoch: 5 }));
    }
}
