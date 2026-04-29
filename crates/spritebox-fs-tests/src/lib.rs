//! Integration test harness for the spritebox virtual filesystem.
//!
//! Wires the four production crates together with the in-memory
//! transport so the full request/response/push protocol can be exercised
//! in `cargo test` on macOS — no FUSE, no real sprite, no privileges.
//!
//! See `tests/scenarios.rs` for the baseline test matrix from
//! `docs/virtual-fs.md` testing strategy.

#![forbid(unsafe_code)]

use std::sync::Arc;

use spritebox_fs_host::mem::FakeClock;
use spritebox_fs_host::{Dispatcher, MemFs};
use spritebox_fs_protocol::{Frame, Push};
use spritebox_fs_remote::{CmdClient, PassthroughRemote};
use spritebox_fs_transport::{Controls, FrameSink, FrameStream, InMemoryTransport, RealClock};
use tokio::sync::mpsc;

/// A fully-wired in-memory test rig: client (sprite side) ↔ transport ↔
/// server (host side) backed by [`MemFs`]. The harness exposes the
/// transport [`Controls`] in both directions so tests can inject
/// latency, drops, disconnects, corruption, etc.
pub struct Harness {
    pub remote: PassthroughRemote<spritebox_fs_transport::InMemSink>,
    pub host_fs: MemFs,
    pub host_clock: Arc<FakeClock>,
    /// Controls applied to client → host frames.
    pub c2s_controls: Controls,
    /// Controls applied to host → client frames.
    pub s2c_controls: Controls,
    /// Receiver for any pushes the client emits.
    pub push_rx: mpsc::Receiver<Push>,
    /// Handle to the host loop. Aborting it simulates a host crash.
    pub host_loop: tokio::task::JoinHandle<()>,
}

impl Harness {
    pub async fn new() -> Self {
        let clock = Arc::new(RealClock);
        let (client_end, server_end, c2s_controls, s2c_controls) =
            InMemoryTransport::pair(clock);

        let host_clock = FakeClock::new(1_000_000);
        let host_fs = MemFs::with_clock(host_clock.clone());
        let dispatcher = Dispatcher::new(host_fs.clone());

        let host_loop = spawn_host_loop(server_end, dispatcher);

        let (push_tx, push_rx) = mpsc::channel(64);
        let client = CmdClient::new(client_end.sink, client_end.stream, push_tx);
        let remote = PassthroughRemote::new(client);

        Harness {
            remote,
            host_fs,
            host_clock,
            c2s_controls,
            s2c_controls,
            push_rx,
            host_loop,
        }
    }
}

fn spawn_host_loop(
    mut server_end: spritebox_fs_transport::Endpoint,
    dispatcher: Dispatcher<MemFs>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(frame) = server_end.stream.recv().await {
            if let Frame::Request { id, body } = frame {
                let resp = dispatcher.handle(id, body).await;
                if server_end.sink.send(resp).await.is_err() {
                    break;
                }
            }
        }
    })
}
