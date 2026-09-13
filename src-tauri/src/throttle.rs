//! Aggregate bandwidth limiter.
//!
//! A token bucket built on `tokio::sync::Semaphore`, where one permit == one
//! byte. A background task refills permits every 100 ms; segment tasks acquire
//! permits before consuming each chunk, so the sum across every concurrent
//! download stays near the configured cap. A single `Throttle` is shared by all
//! transfers, so the cap is a global ceiling, not per-download.

use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::Semaphore;

const REFILL_MS: u64 = 100;
const SLICE: usize = 16 * 1024;
const MAX_SLICE: usize = 256 * 1024;

struct Inner {
    sem: Semaphore,
    /// Bytes added each 100 ms tick = cap_bytes_per_sec / 10.
    per_tick: usize,
    /// Never let more than this bank up, or an idle period would hand the next
    /// download a burst that ignores the cap until it drains.
    cap: usize,
    /// Acquire in slices this big. Normally 16 KB, but never larger than a
    /// single tick's budget, or a low cap could never accumulate one slice.
    slice: usize,
}

/// Cloneable handle. `unlimited()` disables limiting with zero overhead.
#[derive(Clone)]
pub struct Throttle {
    inner: Option<Arc<Inner>>,
}

impl Throttle {
    pub fn unlimited() -> Self {
        Throttle { inner: None }
    }

    /// `kb_per_sec == 0` is treated as unlimited.
    pub fn new(kb_per_sec: u64) -> Self {
        if kb_per_sec == 0 {
            return Throttle::unlimited();
        }

        let per_tick = (kb_per_sec.saturating_mul(1024) / (1000 / REFILL_MS))
            .min((usize::MAX / 4) as u64) as usize;
        let per_tick = per_tick.max(1);
        // Scale the acquire slice with the cap so a high limit doesn't force
        // dozens of tiny semaphore acquisitions per network chunk, while never
        // exceeding a single tick's budget (which would deadlock a low cap).
        let slice = (per_tick / 4).clamp(SLICE, MAX_SLICE).min(per_tick).max(1);
        let cap = (per_tick * 2).max(slice);

        let inner = Arc::new(Inner {
            sem: Semaphore::new(0),
            per_tick,
            cap,
            slice,
        });

        // The refiller holds only a Weak ref, so once every Throttle handle and
        // every in-flight transfer has dropped its clone, the task exits on its
        // own — no explicit shutdown needed when settings change.
        let weak = Arc::downgrade(&inner);
        tokio::spawn(refill(weak));

        Throttle { inner: Some(inner) }
    }

    /// Block until `n` bytes of budget are available, consuming them. A no-op
    /// under `unlimited()`.
    pub async fn take(&self, n: usize) {
        let Some(inner) = &self.inner else { return };
        let mut remaining = n;
        while remaining > 0 {
            let want = remaining.min(inner.slice);
            // Semaphore is never closed, so acquire only errors on close.
            if let Ok(permit) = inner.sem.acquire_many(want as u32).await {
                permit.forget();
            }
            remaining -= want;
        }
    }
}

async fn refill(weak: Weak<Inner>) {
    let mut interval = tokio::time::interval(Duration::from_millis(REFILL_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let Some(inner) = weak.upgrade() else { break };
        let available = inner.sem.available_permits();
        if available < inner.cap {
            let room = inner.cap - available;
            inner.sem.add_permits(inner.per_tick.min(room));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn unlimited_take_returns_immediately() {
        let t = Throttle::unlimited();
        let start = Instant::now();
        t.take(10 * 1024 * 1024).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn limits_throughput_to_roughly_the_cap() {
        // 100 KB/s cap, pull 30 KB: first tick grants ~10 KB, so the rest must
        // wait for at least the next two ticks (~200 ms).
        let t = Throttle::new(100);
        let start = Instant::now();
        t.take(30 * 1024).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150),
            "throttle released too fast: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn bank_is_capped_so_idle_does_not_grant_a_burst() {
        let t = Throttle::new(100); // per_tick ~10 KB, cap ~20 KB
        // Let it sit idle far longer than two ticks.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let start = Instant::now();
        // The banked budget is capped at ~20 KB; taking 60 KB must still wait
        // for fresh ticks rather than draining a huge idle-accumulated bank.
        t.take(60 * 1024).await;
        assert!(
            start.elapsed() >= Duration::from_millis(250),
            "idle period granted an uncapped burst"
        );
    }

    #[tokio::test]
    async fn tiny_cap_still_makes_progress() {
        // 4 KB/s: per_tick ~409 B < 16 KB slice. Must not deadlock.
        let t = Throttle::new(4);
        tokio::time::timeout(Duration::from_secs(3), t.take(2048))
            .await
            .expect("tiny cap deadlocked");
    }
}
