//! Caps the rate at which new peer connections are admitted.
//!
//! Without a rate cap the client was observed dialling/accepting ~200 peers
//! per second during an ultra-broad search (e.g. `intro`, which matches
//! something on almost every peer), tripping flood protection on the server
//! (and stressing the local network path) and resetting the server
//! connection. See the seakarr investigation (2026-08-23): 500 connections in
//! 3.5s after an `intro` search vs. <25/s for a normal broad search.
//!
//! The single shared limiter caps the search-result connection storm in both
//! directions: inbound accepts (the listener, before the handshake thread) and
//! outbound server-brokered `P` dials (the client operations loop). User-
//! initiated connections — `F` transfer dials and `GetPeerAddressResponse`
//! dials (downloads, uploads, and peer-connection requests) — are exempt: they never storm, and capping them
//! could only stall a legitimate action. Inbound accepts are gated before the
//! handshake reveals the connection type, so an inbound `F` transfer or
//! `PierceFirewall` dial-back also draws from the budget; this is negligible
//! in practice (the cap is ~2-3x normal search rate, and a refused transfer
//! simply retries).
//!
//! A slot is consumed when a connection is *attempted*, not when it succeeds:
//! a dial that fails or a handshake that never completes still occupies its
//! slot for one second. That keeps the limiter a simple refuse-over-queue
//! guard; the 50/s budget is generous enough that wasted slots do not starve
//! real connections.
//!
//! The cap is a compatibility guard, not a throttle on normal use: typical
//! searches admit ~15-25 new connections per second, and the cap is set well
//! above that while staying far below the observed flood threshold. Excess
//! connections are simply refused (dropped) — they are overwhelmingly
//! redundant search-result deliveries, and we only need one good candidate
//! per album.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Connections per second allowed to be newly admitted. The default sits
/// between normal use (~25/s worst case) and the kick threshold (~200/s).
pub const DEFAULT_MAX_CONNECTIONS_PER_SECOND: usize = 50;

/// Simple per-second admission limiter.
///
/// Thread-safe: shared between the listener accept loop (inbound) and the
/// client operations loop (outbound dials) so the two paths share one budget.
pub struct ConnectionLimiter {
    cap_per_second: usize,
    admissions: Mutex<VecDeque<Instant>>,
}

impl ConnectionLimiter {
    #[must_use]
    pub const fn new(cap_per_second: usize) -> Self {
        Self {
            cap_per_second,
            admissions: Mutex::new(VecDeque::new()),
        }
    }

    /// Reserve one connection slot within the current one-second window.
    /// Returns `false` when the window is already at capacity — the caller
    /// should drop the excess connection rather than queue it.
    #[must_use]
    pub fn try_acquire(&self) -> bool {
        self.try_acquire_at(Instant::now())
    }

    /// Sliding-window admission at an explicit time, so tests can drive the
    /// window boundary deterministically.
    fn try_acquire_at(&self, now: Instant) -> bool {
        let mut admissions = match self.admissions.lock() {
            Ok(a) => a,
            Err(poisoned) => poisoned.into_inner(),
        };
        // A sliding window (not a fixed epoch) so a burst cannot straddle a
        // boundary and admit 2x the cap: only admissions from the last full
        // second still occupy budget.
        while admissions
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1))
        {
            admissions.pop_front();
        }
        if admissions.len() < self.cap_per_second {
            admissions.push_back(now);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_capped_at_the_connections_per_second_limit() {
        let limiter = ConnectionLimiter::new(5);

        let admitted = (0..100).filter(|_| limiter.try_acquire()).count();

        assert!(
            admitted <= 5,
            "a 100-connection burst must not admit more than the 5-per-second \
             cap, admitted {admitted}"
        );
    }

    #[test]
    fn the_limit_is_per_second_and_recovers_after_the_window() {
        let limiter = ConnectionLimiter::new(2);

        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());
        assert!(
            !limiter.try_acquire(),
            "a third connection within the same second must be refused"
        );

        std::thread::sleep(Duration::from_secs(1) + Duration::from_millis(100));

        assert!(
            limiter.try_acquire(),
            "the per-second budget must refresh after the window"
        );
    }

    #[test]
    fn a_zero_cap_refuses_every_connection() {
        let limiter = ConnectionLimiter::new(0);

        assert!(
            !limiter.try_acquire(),
            "a zero-per-second cap must admit nothing"
        );
    }

    #[test]
    fn a_sliding_window_refuses_a_burst_straddling_the_boundary() {
        // A fixed-window limiter resets its epoch after one second and would
        // admit 2x the cap when a burst straddles that boundary. The sliding
        // window must keep refusing until a full second has elapsed since the
        // first admission.
        let limiter = ConnectionLimiter::new(2);
        let t0 = Instant::now();

        // Fill the budget 990ms into the nominal window.
        assert!(limiter.try_acquire_at(t0 + Duration::from_millis(990)));
        assert!(limiter.try_acquire_at(t0 + Duration::from_millis(990)));

        // 20ms later, a fixed-window impl would reset and admit again; the
        // sliding window still sees both admissions as 20ms old.
        assert!(!limiter.try_acquire_at(t0 + Duration::from_millis(1010)));
        assert!(!limiter.try_acquire_at(t0 + Duration::from_millis(1010)));

        // Once a full second has passed since the first admission, budget frees.
        assert!(limiter.try_acquire_at(t0 + Duration::from_millis(1991)));
    }

    #[test]
    fn concurrent_burst_is_bounded_by_the_cap() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const THREADS: usize = 8;
        const ATTEMPTS_PER_THREAD: usize = 50;
        let limiter = Arc::new(ConnectionLimiter::new(20));
        let admitted = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let limiter = Arc::clone(&limiter);
            let admitted = Arc::clone(&admitted);
            handles.push(std::thread::spawn(move || {
                for _ in 0..ATTEMPTS_PER_THREAD {
                    if limiter.try_acquire() {
                        admitted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let total = admitted.load(Ordering::Relaxed);
        assert!(
            total <= 20,
            "a concurrent burst of {} attempts must admit at most the \
             per-second cap of 20, admitted {total}",
            THREADS * ATTEMPTS_PER_THREAD
        );
    }
}
