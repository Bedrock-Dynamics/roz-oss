use std::time::{Duration, Instant};

/// Rate-limits telemetry messages to a target frequency.
///
/// Call `should_send()` for each incoming message. Returns `true` if enough
/// time has elapsed since the last sent message.
pub struct Decimator {
    target_hz: f64,
    min_interval: Duration,
    last_sent: Instant,
}

impl Decimator {
    pub fn new(target_hz: f64) -> Self {
        let min_interval = if target_hz > 0.0 {
            Duration::from_secs_f64(1.0 / target_hz)
        } else {
            Duration::MAX
        };
        Self {
            target_hz,
            min_interval,
            // Use checked_sub to safely subtract; fall back to epoch-like instant
            // when min_interval is Duration::MAX (target_hz == 0).
            last_sent: Instant::now().checked_sub(min_interval).unwrap_or_else(Instant::now),
        }
    }

    pub const fn target_hz(&self) -> f64 {
        self.target_hz
    }

    pub fn should_send(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.last_sent) >= self.min_interval {
            self.last_sent = now;
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
    fn first_message_always_sent() {
        let mut dec = Decimator::new(10.0);
        assert!(dec.should_send());
    }

    #[test]
    fn respects_rate_limit() {
        let mut dec = Decimator::new(10.0);
        assert!(dec.should_send()); // first
        assert!(!dec.should_send()); // too soon (< 100ms)
    }

    #[test]
    fn allows_after_interval() {
        let mut dec = Decimator::new(10.0);
        dec.should_send(); // first
        dec.last_sent = Instant::now().checked_sub(Duration::from_millis(200)).unwrap();
        assert!(dec.should_send()); // enough time passed
    }

    #[test]
    fn zero_hz_blocks_all() {
        let mut dec = Decimator::new(0.0);
        assert!(!dec.should_send());
    }

    #[test]
    fn target_hz_accessor() {
        let dec = Decimator::new(30.0);
        assert!((dec.target_hz() - 30.0).abs() < f64::EPSILON);
    }
}
