//! Rate limiter for tool execution — sliding window approach.
//!
//! Prevents runaway agents from overwhelming the system with rapid
//! tool calls (e.g. fork-bomb, infinite loop with bash commands).
//!
//! # Design
//!
//! Uses a sliding window with a configurable max calls and window duration.
//! Thread-safe via `Arc<Mutex<>>`. When the limit is exceeded, the caller
//! receives a `ToolError::Timeout` with a human-readable message.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Default maximum number of bash commands per window.
const DEFAULT_MAX_BASH_CALLS: usize = 10;
/// Default window duration in seconds.
const DEFAULT_WINDOW_SECS: f64 = 30.0;

/// Shared rate limiter (thread-safe wrapper).
pub type SharedRateLimiter = Arc<Mutex<RateLimiter>>;

/// Create a new shared rate limiter with default settings.
pub fn new_shared() -> SharedRateLimiter {
    Arc::new(Mutex::new(RateLimiter::new(
        DEFAULT_MAX_BASH_CALLS,
        DEFAULT_WINDOW_SECS,
    )))
}

/// Sliding-window rate limiter.
#[derive(Debug)]
pub struct RateLimiter {
    max_calls: usize,
    window_secs: f64,
    /// Timestamps of recent calls, in chronological order.
    timestamps: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(max_calls: usize, window_secs: f64) -> Self {
        Self {
            max_calls,
            window_secs,
            timestamps: VecDeque::new(),
        }
    }

    /// Check if a call is allowed. If so, record it and return `Ok(())`.
    /// If the limit is exceeded, return an error message.
    pub fn check_and_record(&mut self) -> Result<(), String> {
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_secs_f64(self.window_secs);

        // Prune expired timestamps from the front
        while let Some(&ts) = self.timestamps.front() {
            if ts < cutoff {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }

        if self.timestamps.len() >= self.max_calls {
            // Calculate when the oldest timestamp expires (approx wait time)
            let oldest = self.timestamps.front().copied().unwrap_or(now);
            let wait = cutoff.checked_duration_since(oldest).unwrap_or_default();
            let wait_secs = wait.as_secs_f64().max(0.1);
            return Err(format!(
                "Rate limit exceeded: {} calls in {:.0}s. Try again in {:.1}s.",
                self.max_calls, self.window_secs, wait_secs
            ));
        }

        self.timestamps.push_back(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_allows_within_limit() {
        let mut rl = RateLimiter::new(3, 10.0);
        assert!(rl.check_and_record().is_ok());
        assert!(rl.check_and_record().is_ok());
        assert!(rl.check_and_record().is_ok());
    }

    #[test]
    fn test_rate_limiter_rejects_excess() {
        let mut rl = RateLimiter::new(2, 10.0);
        assert!(rl.check_and_record().is_ok());
        assert!(rl.check_and_record().is_ok());
        assert!(rl.check_and_record().is_err());
    }

    #[test]
    fn test_rate_limiter_recovers_after_window() {
        let mut rl = RateLimiter::new(2, 0.01); // 10ms window
        assert!(rl.check_and_record().is_ok());
        assert!(rl.check_and_record().is_ok());
        assert!(rl.check_and_record().is_err());
        std::thread::sleep(std::time::Duration::from_millis(20));
        // After the window, old entries are pruned
        assert!(rl.check_and_record().is_ok());
    }

    #[test]
    fn test_rate_limiter_prunes_expired() {
        let mut rl = RateLimiter::new(5, 0.01);
        for _ in 0..5 {
            assert!(rl.check_and_record().is_ok());
        }
        // All 5 within window — should be blocked
        assert!(rl.check_and_record().is_err());
        std::thread::sleep(std::time::Duration::from_millis(20));
        // After sleep, previous entries expired, so we can call again
        assert!(rl.check_and_record().is_ok());
    }
}
