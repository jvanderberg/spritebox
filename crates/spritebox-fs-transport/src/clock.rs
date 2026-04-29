//! Transport clock abstraction.
//!
//! The forwarder's `sleep` is the only timing source the transport touches.
//! In production we use [`RealClock`] (delegates to `tokio::time::sleep`).
//! In tests we use [`FakeClock`], which makes timing deterministic by
//! integrating with `tokio::time::pause` / `tokio::time::advance`.

use std::time::Duration;

use async_trait::async_trait;

#[async_trait]
pub trait TransportClock: Send + Sync + 'static {
    async fn sleep(&self, dur: Duration);
}

#[derive(Default, Clone, Copy)]
pub struct RealClock;

#[async_trait]
impl TransportClock for RealClock {
    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

/// A clock that uses `tokio::time::pause` semantics. Functionally
/// equivalent to [`RealClock`] from the transport's perspective; the
/// difference is that the test driver controls the elapsed time via
/// `tokio::time::advance`.
#[derive(Default, Clone, Copy)]
pub struct FakeClock;

#[async_trait]
impl TransportClock for FakeClock {
    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}
