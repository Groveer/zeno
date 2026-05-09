//! Unified retry infrastructure for both main and auxiliary API calls.
//!
//! Provides:
//! - `RetryConfig`: configurable retry parameters (max retries, delays, retryable statuses)
//! - `get_retry_delay()`: exponential backoff + Retry-After header + jitter
//! - `is_retryable_status()`: shared retryable-status classification
//! - `retry_with_backoff()`: generic async retry wrapper for non-streaming calls
//!
//! The main query engine uses `RetryConfig` + `get_retry_delay()` directly
//! inside its stream retry loop. The auxiliary client uses `retry_with_backoff()`
//! for connection/server-error retries, and `is_retryable_status()` for
//! status classification (replacing the duplicated logic in router.rs).

use rand::Rng;
use std::future::Future;

/// Retry configuration shared by main API and auxiliary API calls.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (not counting the initial call).
    pub max_retries: u32,
    /// Base delay in seconds for exponential backoff.
    pub base_delay: f64,
    /// Maximum delay in seconds (caps the exponential growth).
    pub max_delay: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: 1.0,
            max_delay: 30.0,
        }
    }
}

impl RetryConfig {
    /// Create a config tailored for auxiliary (non-streaming) API calls.
    /// Auxiliary calls are cheap and short-lived, so we use fewer retries
    /// and shorter delays than the main query loop.
    pub fn for_auxiliary() -> Self {
        Self {
            max_retries: 2,
            base_delay: 0.5,
            max_delay: 10.0,
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

/// Generic retry wrapper for async operations with exponential backoff.
///
/// Calls `f` repeatedly until it returns `Ok(T)` or retries are exhausted.
/// On each `Err(E)`, calls `is_retryable` to decide whether to retry.
/// Logs each retry attempt with the delay.
///
/// # Arguments
/// * `config` — Retry parameters (max retries, delays)
/// * `label` — A human-readable label for log messages (e.g. "auxiliary compression")
/// * `is_retryable` — Callback that returns `true` if the error is worth retrying
/// * `f` — The async operation to attempt
///
/// # Returns
/// * `Ok(T)` on success
/// * `Err(E)` after exhausting retries or hitting a non-retryable error
pub async fn retry_with_backoff<T, E, F, Fut, P>(
    config: &RetryConfig,
    label: &str,
    is_retryable: P,
    mut f: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Debug,
    P: Fn(&E) -> bool,
{
    let mut attempts: u32 = 0;
    loop {
        match f().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                attempts += 1;
                if attempts > config.max_retries || !is_retryable(&e) {
                    return Err(e);
                }
                let delay = get_retry_delay(attempts, config, None, None);
                tracing::warn!(
                    label = %label,
                    attempt = attempts,
                    delay_secs = delay,
                    error = ?e,
                    "{} failed, retrying", label
                );
                tokio::time::sleep(tokio::time::Duration::from_secs_f64(delay)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.base_delay, 1.0);
        assert_eq!(config.max_delay, 30.0);
    }

    #[test]
    fn test_retry_config_auxiliary() {
        let config = RetryConfig::for_auxiliary();
        assert_eq!(config.max_retries, 2);
        assert_eq!(config.base_delay, 0.5);
        assert_eq!(config.max_delay, 10.0);
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
        assert!(d1 >= 1.0 && d1 < 1.3); // 1.0 + jitter(0..0.25)

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
        // Even at high attempts, delay should not exceed max_delay + jitter
        let d = get_retry_delay(10, &config, None, None);
        assert!(d >= 5.0 && d < 6.5); // capped at 5.0 + jitter(0..1.25)
    }

    #[test]
    fn test_get_retry_delay_retry_after() {
        let config = RetryConfig::default();
        let d = get_retry_delay(1, &config, Some(429), Some(10.0));
        assert_eq!(d, 10.0); // Retry-After honored, capped at max_delay(30)
    }

    #[test]
    fn test_get_retry_delay_retry_after_capped() {
        let config = RetryConfig {
            max_delay: 5.0,
            ..Default::default()
        };
        let d = get_retry_delay(1, &config, Some(429), Some(100.0));
        assert_eq!(d, 5.0); // Retry-After of 100 capped at max_delay=5
    }

    #[tokio::test]
    async fn test_retry_with_backoff_success_first_try() {
        let config = RetryConfig {
            max_retries: 2,
            base_delay: 0.01,
            max_delay: 0.05,
        };
        let result =
            retry_with_backoff(&config, "test", |_| true, || async { Ok::<_, String>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_succeeds_after_retry() {
        let config = RetryConfig {
            max_retries: 2,
            base_delay: 0.01,
            max_delay: 0.05,
        };
        let mut attempts = 0;
        let result = retry_with_backoff(
            &config,
            "test",
            |_: &String| true,
            || {
                attempts += 1;
                async move {
                    if attempts <= 1 {
                        Err("transient".into())
                    } else {
                        Ok(99)
                    }
                }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 99);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_non_retryable() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay: 0.01,
            max_delay: 0.05,
        };
        let result: Result<i32, &str> = retry_with_backoff(
            &config,
            "test",
            |e| *e != "fatal",
            || async { Err::<i32, &str>("fatal") },
        )
        .await;
        assert_eq!(result.unwrap_err(), "fatal");
    }

    #[tokio::test]
    async fn test_retry_with_backoff_exhausted() {
        let config = RetryConfig {
            max_retries: 1,
            base_delay: 0.01,
            max_delay: 0.05,
        };
        let result: Result<i32, &str> = retry_with_backoff(
            &config,
            "test",
            |_| true,
            || async { Err::<i32, &str>("always fails") },
        )
        .await;
        assert_eq!(result.unwrap_err(), "always fails");
    }
}
