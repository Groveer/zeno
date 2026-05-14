//! Unified retry infrastructure for both main and auxiliary API calls.
//!
//! Provides:
//! - `RetryConfig`: configurable retry parameters (base delay, max delay)
//! - `get_retry_delay()`: exponential backoff + Retry-After header + jitter
//! - `is_retryable_status_default()`: shared retryable-status classification
//!
//! The main query engine uses `RetryConfig` + `get_retry_delay()` directly
//! inside its stream retry loop. The auxiliary client uses
//! `is_retryable_status_default()` for status classification.

use rand::Rng;

/// Retry configuration shared by main API and auxiliary API calls.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Base delay in seconds for exponential backoff.
    pub base_delay: f64,
    /// Maximum delay in seconds (caps the exponential growth).
    pub max_delay: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            base_delay: 1.0,
            max_delay: 30.0,
        }
    }
}

/// Check if an HTTP status code is retryable using the default status set.
///
/// Default retryable statuses: 429, 500, 502, 503, 529.
pub fn is_retryable_status_default(status: u16) -> bool {
    static DEFAULT_STATUSES: &[u16] = &[429, 500, 502, 503, 529];
    DEFAULT_STATUSES.contains(&status)
}

/// Calculate the delay before the next retry attempt.
///
/// Priority:
/// 1. If the error is a 429 with a `Retry-After` header, use that value.
/// 2. Otherwise, exponential backoff: `base_delay * 2^(attempt-1)`, capped at `max_delay`.
/// 3. Add random jitter (0 ~ 25% of the computed delay).
pub fn get_retry_delay(
    attempt: u32,
    config: &RetryConfig,
    status: Option<u16>,
    retry_after: Option<f64>,
) -> f64 {
    // 1. Honor Retry-After header on 429 responses
    if status == Some(429)
        && let Some(secs) = retry_after
        && secs > 0.0
    {
        return secs.min(config.max_delay);
    }

    // 2. Exponential backoff + jitter
    let base = config.base_delay * 2.0f64.powi(attempt as i32 - 1);
    let delay = base.min(config.max_delay);

    // Add jitter: 0 ~ delay * 0.25
    let jitter = rand::thread_rng().gen_range(0.0..(delay * 0.25));
    delay + jitter
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.base_delay, 1.0);
        assert_eq!(config.max_delay, 30.0);
    }

    #[test]
    fn test_is_retryable_status_default() {
        assert!(is_retryable_status_default(429));
        assert!(is_retryable_status_default(500));
        assert!(!is_retryable_status_default(200));
        assert!(!is_retryable_status_default(401));
    }

    #[test]
    fn test_get_retry_delay_exponential() {
        let config = RetryConfig::default();
        // Attempt 1: base * 2^0 = 1.0, plus jitter
        let d1 = get_retry_delay(1, &config, None, None);
        assert!(d1 >= 1.0 && d1 < 1.3);

        // Attempt 2: base * 2^1 = 2.0, plus jitter
        let d2 = get_retry_delay(2, &config, None, None);
        assert!(d2 >= 2.0 && d2 < 2.6);

        // Attempt 5: base * 2^4 = 16.0, plus jitter
        let d5 = get_retry_delay(5, &config, None, None);
        assert!(d5 >= 16.0 && d5 < 20.5);
    }

    #[test]
    fn test_get_retry_delay_capped() {
        let config = RetryConfig {
            max_delay: 5.0,
            ..Default::default()
        };
        let d = get_retry_delay(10, &config, None, None);
        assert!(d >= 5.0 && d < 6.5);
    }

    #[test]
    fn test_get_retry_delay_retry_after() {
        let config = RetryConfig::default();
        let d = get_retry_delay(1, &config, Some(429), Some(10.0));
        assert_eq!(d, 10.0);
    }

    #[test]
    fn test_get_retry_delay_retry_after_capped() {
        let config = RetryConfig {
            max_delay: 5.0,
            ..Default::default()
        };
        let d = get_retry_delay(1, &config, Some(429), Some(100.0));
        assert_eq!(d, 5.0);
    }
}
