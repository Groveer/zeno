//! Tool result cache — LRU cache for read-only tool results.
//!
//! Caches results from `read`, `glob`, and `grep` tools to avoid redundant
//! execution when the LLM calls the same tool with the same arguments.
//!
//! # Design
//!
//! - Bounded at 50 entries (LRU eviction)
//! - Keyed by `(tool_name, canonical_args_json)` — normalized JSON string
//! - Only caches read-only tools (is_read_only returns true)
//! - Invalidated on write/edit to the same file path
//! - Thread-safe via `Arc<Mutex<>>`
//!
//! # Cache invalidation
//!
//! When a `write` or `edit` tool modifies a file, all cache entries whose
//! arguments reference that file path are evicted. This ensures the LLM
//! always sees fresh content after modifications.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// Maximum number of cached tool results.
const MAX_CACHE_ENTRIES: usize = 200;

/// A single cache entry.
struct CacheEntry {
    /// The cached result string.
    result: String,
}

/// LRU tool result cache.
pub struct ToolCache {
    /// Map from cache key to entry.
    entries: HashMap<String, CacheEntry>,
    /// LRU order: front = most recently used, back = least recently used.
    lru: VecDeque<String>,
}

impl ToolCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    /// Build a normalized cache key from tool name and arguments.
    fn build_key(tool_name: &str, args: &Value) -> String {
        // Normalize args to a stable JSON string (sorted keys)
        let normalized = normalize_json(args);
        format!("{}:{}", tool_name, normalized)
    }

    /// Try to get a cached result. Returns `None` if not cached.
    /// On hit, promotes the entry to the front of the LRU list.
    pub fn get(&mut self, tool_name: &str, args: &Value) -> Option<&str> {
        let key = Self::build_key(tool_name, args);
        if self.entries.contains_key(&key) {
            // Promote to front
            if let Some(pos) = self.lru.iter().position(|k| *k == key) {
                self.lru.remove(pos);
            }
            self.lru.push_front(key.clone());
            self.entries.get(&key).map(|e| e.result.as_str())
        } else {
            None
        }
    }

    /// Insert a result into the cache.
    /// Evicts the least recently used entry if at capacity.
    pub fn insert(&mut self, tool_name: &str, args: &Value, result: String) {
        let key = Self::build_key(tool_name, args);

        // Remove existing entry if present (for LRU reordering)
        if self.entries.contains_key(&key) {
            if let Some(pos) = self.lru.iter().position(|k| *k == key) {
                self.lru.remove(pos);
            }
        }

        // Evict if at capacity
        while self.lru.len() >= MAX_CACHE_ENTRIES {
            if let Some(oldest) = self.lru.pop_back() {
                self.entries.remove(&oldest);
            }
        }

        self.lru.push_front(key.clone());
        self.entries.insert(key, CacheEntry { result });
    }

    /// Invalidate all cache entries whose arguments reference the given file path.
    /// Called after write/edit tools modify a file.
    pub fn invalidate_path(&mut self, path: &Path) {
        let path_str = path.to_string_lossy();
        let keys_to_remove: Vec<String> = self
            .entries
            .keys()
            .filter(|key| key.contains(path_str.as_ref()))
            .cloned()
            .collect();

        for key in &keys_to_remove {
            self.entries.remove(key);
            if let Some(pos) = self.lru.iter().position(|k| k == key) {
                self.lru.remove(pos);
            }
        }

        if !keys_to_remove.is_empty() {
            tracing::debug!(
                path = %path_str,
                evicted = keys_to_remove.len(),
                "Tool cache invalidated for path"
            );
        }
    }

    /// Clear the entire cache.
    #[allow(
        dead_code,
        reason = "cache management API, may be used by future commands"
    )]
    pub fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }

    /// Number of entries in the cache.
    #[allow(
        dead_code,
        reason = "cache management API, may be used by future commands"
    )]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Normalize a JSON value to a stable string representation.
/// Sorts object keys so the same logical arguments produce the same key.
fn normalize_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let pairs: Vec<String> = keys
                .iter()
                .map(|k| format!("\"{}\":{}", k, normalize_json(&map[*k])))
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
        Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(normalize_json).collect();
            format!("[{}]", items.join(","))
        }
        Value::String(s) => format!("\"{}\"", s),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
    }
}

/// Thread-safe shared tool cache.
pub type SharedToolCache = Arc<Mutex<ToolCache>>;

/// Create a new shared tool cache.
pub fn new_shared() -> SharedToolCache {
    Arc::new(Mutex::new(ToolCache::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_cache_hit() {
        let mut cache = ToolCache::new();
        let args = json!({"path": "src/main.rs"});
        assert!(cache.get("read", &args).is_none());
        cache.insert("read", &args, "file content".into());
        assert_eq!(cache.get("read", &args), Some("file content"));
    }

    #[test]
    fn test_cache_miss_different_args() {
        let mut cache = ToolCache::new();
        cache.insert("read", &json!({"path": "a.rs"}), "content a".into());
        assert!(cache.get("read", &json!({"path": "b.rs"})).is_none());
    }

    #[test]
    fn test_cache_miss_different_tool() {
        let mut cache = ToolCache::new();
        cache.insert("read", &json!({"path": "a.rs"}), "content".into());
        assert!(cache.get("glob", &json!({"path": "a.rs"})).is_none());
    }

    #[test]
    fn test_lru_eviction() {
        let mut cache = ToolCache::new();
        // Fill to capacity
        for i in 0..MAX_CACHE_ENTRIES {
            let args = json!({"path": format!("file_{}.rs", i)});
            cache.insert("read", &args, format!("content {}", i));
        }
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);

        // Access the oldest entry to promote it
        let oldest_args = json!({"path": "file_0.rs"});
        assert_eq!(cache.get("read", &oldest_args), Some("content 0"));

        // Insert one more — should evict file_1 (now the LRU)
        cache.insert(
            "read",
            &json!({"path": "new_file.rs"}),
            "new content".into(),
        );
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        // file_0 was promoted, so it should still be there
        assert_eq!(cache.get("read", &oldest_args), Some("content 0"));
    }

    #[test]
    fn test_invalidate_path() {
        let mut cache = ToolCache::new();
        cache.insert("read", &json!({"path": "src/main.rs"}), "main".into());
        cache.insert("read", &json!({"path": "src/lib.rs"}), "lib".into());
        cache.insert("glob", &json!({"pattern": "*.rs"}), "files".into());

        assert_eq!(cache.len(), 3);
        cache.invalidate_path(Path::new("src/main.rs"));
        assert_eq!(cache.len(), 2);
        assert!(cache.get("read", &json!({"path": "src/main.rs"})).is_none());
        assert_eq!(
            cache.get("read", &json!({"path": "src/lib.rs"})),
            Some("lib")
        );
    }

    #[test]
    fn test_clear() {
        let mut cache = ToolCache::new();
        cache.insert("read", &json!({"path": "a.rs"}), "a".into());
        cache.insert("read", &json!({"path": "b.rs"}), "b".into());
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_normalize_json_sorted_keys() {
        let a = json!({"z": 1, "a": 2, "m": 3});
        let b = json!({"a": 2, "m": 3, "z": 1});
        assert_eq!(normalize_json(&a), normalize_json(&b));
    }

    #[test]
    fn test_normalize_json_nested() {
        let a = json!({"path": "src/main.rs", "options": {"limit": 10, "offset": 0}});
        let b = json!({"options": {"offset": 0, "limit": 10}, "path": "src/main.rs"});
        assert_eq!(normalize_json(&a), normalize_json(&b));
    }
}
