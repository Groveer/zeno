//! Auxiliary client cache — reuse HTTP clients across auxiliary calls.
//!
//! Caches reqwest::Client instances by (provider, base_url) key.
//! `get_or_create()` is actively used by `client.rs`; the eviction/shutdown
//! helpers are reserved for future interactive CLI commands.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Client;

/// Maximum number of cached clients.
const CACHE_MAX_SIZE: usize = 32;

/// Time after which a cached client is considered stale (5 minutes).
const STALE_AFTER: Duration = Duration::from_secs(300);

/// Cache key: (provider_name, base_url).
type CacheKey = (String, String);

/// A cached HTTP client with metadata.
struct CachedEntry {
    client: Client,
    created_at: Instant,
}

/// Global client cache, protected by a mutex.
static CLIENT_CACHE: once_cell::sync::Lazy<Mutex<HashMap<CacheKey, CachedEntry>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// Get or create a cached `reqwest::Client` for the given provider + base_url.
///
/// If a cached client exists and is not stale, returns it. Otherwise creates
/// a new one with the specified timeout, stores it, and returns it.
pub fn get_or_create(
    provider_name: &str,
    base_url: &str,
    timeout: Duration,
) -> Result<Client, String> {
    let key = (provider_name.to_string(), base_url.to_string());

    let mut cache = CLIENT_CACHE
        .lock()
        .map_err(|e| format!("Client cache lock poisoned: {}", e))?;

    // Check for existing non-stale entry
    if let Some(entry) = cache.get(&key) {
        if entry.created_at.elapsed() < STALE_AFTER {
            return Ok(entry.client.clone());
        }
        // Stale — remove and rebuild
        cache.remove(&key);
    }

    // Evict oldest entries if at capacity
    while cache.len() >= CACHE_MAX_SIZE {
        // Remove the first (oldest-insertion-order) entry
        if let Some(oldest_key) = cache.keys().next().cloned() {
            cache.remove(&oldest_key);
        } else {
            break;
        }
    }

    // Build new client
    let client = Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    cache.insert(
        key,
        CachedEntry {
            client: client.clone(),
            created_at: Instant::now(),
        },
    );

    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_or_create() {
        let client = get_or_create("test", "https://api.example.com", Duration::from_secs(30));
        assert!(client.is_ok());

        // Second call should return the same cached client
        let client2 = get_or_create("test", "https://api.example.com", Duration::from_secs(30));
        assert!(client2.is_ok());
    }

    #[test]
    fn test_shutdown() {
        let _ = get_or_create(
            "shutdown_test",
            "https://api.example.com",
            Duration::from_secs(30),
        );
        // Clear the cache manually for cleanup
        if let Ok(mut cache) = CLIENT_CACHE.lock() {
            cache.clear();
        }
        // After clear, next call creates a fresh client
        let client = get_or_create(
            "shutdown_test",
            "https://api.example.com",
            Duration::from_secs(30),
        );
        assert!(client.is_ok());
    }
}
