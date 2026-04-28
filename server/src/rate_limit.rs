//! Per-peer token-bucket rate limits for the WebSocket protocol.
//!
//! Each `PeerRateLimits` value lives on the stack of one peer's read task,
//! so there's no locking and no cross-peer interference. Repeated
//! over-limit hits within a 10 s window trip a disconnect.

use std::time::{Duration, Instant};

const OVERAGE_WINDOW: Duration = Duration::from_secs(10);
const OVERAGE_DISCONNECT_THRESHOLD: u32 = 20;

pub struct TokenBucket {
    capacity: f32,
    tokens: f32,
    refill_per_sec: f32,
    last: Instant,
}

impl TokenBucket {
    pub fn new(rate_per_sec: f32, burst: f32) -> Self {
        Self {
            capacity: burst,
            tokens: burst,
            refill_per_sec: rate_per_sec,
            last: Instant::now(),
        }
    }

    pub fn try_take(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f32();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub struct PeerRateLimits {
    pub join_room: TokenBucket,
    pub validate_api_key: TokenBucket,
    pub update_position: TokenBucket,
    pub audio: TokenBucket,
    pub identify_group: TokenBucket,
    overage_window_start: Instant,
    overage_count: u32,
}

impl PeerRateLimits {
    pub fn new() -> Self {
        Self {
            // Players chain waypoints across maps quickly — each waypoint
            // is a LeaveRoom + JoinRoom pair. A burst of 8 covers a tight
            // 4-jump sequence; sustained 2/s covers slower chained travel.
            join_room: TokenBucket::new(2.0, 8.0),
            validate_api_key: TokenBucket::new(1.0, 2.0),
            update_position: TokenBucket::new(30.0, 60.0),
            audio: TokenBucket::new(60.0, 120.0),
            // RTAPI-driven squad reports. Coalesced client-side via
            // debounce; a burst of 5 covers rapid join/leave flurries
            // when the squad is forming.
            identify_group: TokenBucket::new(0.5, 5.0),
            overage_window_start: Instant::now(),
            overage_count: 0,
        }
    }

    /// Record an over-limit event. Returns true if the peer has tripped
    /// enough times in the rolling window to be disconnected.
    pub fn record_overage(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.overage_window_start) > OVERAGE_WINDOW {
            self.overage_window_start = now;
            self.overage_count = 0;
        }
        self.overage_count = self.overage_count.saturating_add(1);
        self.overage_count >= OVERAGE_DISCONNECT_THRESHOLD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_starts_full_and_drains() {
        let mut b = TokenBucket::new(10.0, 3.0);
        assert!(b.try_take());
        assert!(b.try_take());
        assert!(b.try_take());
        assert!(!b.try_take(), "fourth take should fail with burst=3");
    }

    #[test]
    fn overage_threshold_trips_disconnect() {
        let mut r = PeerRateLimits::new();
        for _ in 0..(OVERAGE_DISCONNECT_THRESHOLD - 1) {
            assert!(!r.record_overage());
        }
        assert!(r.record_overage(), "Nth overage should trip disconnect");
    }
}
