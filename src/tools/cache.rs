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
        if self.entries.contains_key(&key)
            && let Some(pos) = self.lru.iter().position(|k| *k == key)
        {
            self.lru.remove(pos);
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
    ///
    /// Parses the JSON portion of each cache key and checks the "path" field
    /// for exact match (not substring), avoiding false positives like
    /// `src/main.rs` matching `src/main.rs.bak`.
    pub fn invalidate_path(&mut self, path: &Path) {
        let path_str = path.to_string_lossy();
        let normalized = normalize_path_for_key(&path_str);
        let keys_to_remove: Vec<String> = self
            .entries
            .keys()
            .filter(|key| {
                // Key format: "{tool_name}:{json}" — split on first ':'
                let json_str = match key.find(':') {
                    Some(pos) => &key[pos + 1..],
                    None => return false,
                };
                let args: Value = match serde_json::from_str(json_str) {
                    Ok(v) => v,
                    Err(_) => return false,
                };
                // Check "path" field (used by read, write, edit, grep, glob)
                if let Some(p) = args.get("path").and_then(|v| v.as_str())
                    && normalize_path_for_key(p) == normalized
                {
                    return true;
                }
                // Check "include" field for glob-style tools
                if let Some(p) = args.get("include").and_then(|v| v.as_str())
                    && normalize_path_for_key(p) == normalized
                {
                    return true;
                }
                false
            })
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

/// Normalize tool arguments for cache key construction.
///
/// Only used for cacheable tools (`read`, `glob`, `grep`). Calling with
/// other tool names returns args unchanged (harmless but unnecessary).
///
/// For the `read` tool, normalizes path only (`./src/main.rs` → `src/main.rs`).
/// Offset/limit are NOT stripped to their defaults, because `parse_read_range`
/// treats bare reads differently from explicit-default reads for small files
/// (bare read returns full file content; explicit offset=1, limit=500 returns
/// at most 500 lines). Keeping them distinct prevents incorrect cache hits.
///
/// For `glob` and `grep`, normalizes the `path` field only.
/// For other tools, returns args unchanged.
pub fn normalize_for_cache(tool_name: &str, args: &Value) -> Value {
    match tool_name {
        "read" => {
            // Only normalize the path; preserve offset/limit/context as-is
            // to avoid semantic collisions between bare reads and explicit reads.
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                let normalized_path = normalize_path_for_key(p);
                if normalized_path == p {
                    return args.clone();
                }
                let mut out = args.clone();
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("path".into(), Value::String(normalized_path));
                }
                out
            } else {
                args.clone()
            }
        }
        "glob" | "grep" => {
            // Normalize path if present
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                let mut out = args.clone();
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("path".into(), Value::String(normalize_path_for_key(p)));
                }
                out
            } else {
                args.clone()
            }
        }
        _ => args.clone(),
    }
}

/// Normalize a file path for cache key use.
/// Strips leading `./` to avoid `./src/main.rs` != `src/main.rs` misses.
fn normalize_path_for_key(path: &str) -> String {
    let trimmed = path.trim();
    // Remove leading "./"
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);

    // Split path into components, filtering out empty components
    let mut components = Vec::new();
    for component in trimmed.split('/').filter(|c| !c.is_empty()) {
        match component {
            "." => continue,
            ".." => {
                // Pop previous component if exists and not at root
                if !components.is_empty() {
                    components.pop();
                } else {
                    // No previous component, keep ".."
                    components.push("..");
                }
            }
            _ => components.push(component),
        }
    }

    // Rejoin components
    if components.is_empty() {
        ".".to_string() // Return current directory for empty path
    } else {
        components.join("/")
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

    #[test]
    fn test_normalize_for_cache_read_defaults_differ() {
        // Bare read and explicit-default read should NOT match —
        // parse_read_range treats them differently for small files.
        let a = normalize_for_cache("read", &json!({"path": "src/main.rs"}));
        let b = normalize_for_cache(
            "read",
            &json!({"path": "src/main.rs", "offset": 1, "limit": 500}),
        );
        assert_ne!(normalize_json(&a), normalize_json(&b));
    }

    #[test]
    fn test_normalize_for_cache_read_dot_slash() {
        // ./src/main.rs should match src/main.rs
        let a = normalize_for_cache("read", &json!({"path": "./src/main.rs"}));
        let b = normalize_for_cache("read", &json!({"path": "src/main.rs"}));
        assert_eq!(normalize_json(&a), normalize_json(&b));
    }

    #[test]
    fn test_normalize_for_cache_read_non_default() {
        // Non-default offset/limit should be preserved
        let a = normalize_for_cache(
            "read",
            &json!({"path": "src/main.rs", "offset": 100, "limit": 200}),
        );
        let b = normalize_for_cache("read", &json!({"path": "src/main.rs"}));
        assert_ne!(normalize_json(&a), normalize_json(&b));
    }

    #[test]
    fn test_normalize_for_cache_read_context_mode() {
        // context mode should differ from offset/limit mode
        let a = normalize_for_cache(
            "read",
            &json!({"path": "src/main.rs", "offset": 50, "context": 10}),
        );
        let b = normalize_for_cache("read", &json!({"path": "src/main.rs"}));
        assert_ne!(normalize_json(&a), normalize_json(&b));
    }

    #[test]
    fn test_normalize_for_cache_context_vs_offset_only() {
        // context mode and offset-only mode must not collide
        let a = normalize_for_cache(
            "read",
            &json!({"path": "src/main.rs", "offset": 50, "context": 10}),
        );
        let b = normalize_for_cache("read", &json!({"path": "src/main.rs", "offset": 50}));
        assert_ne!(normalize_json(&a), normalize_json(&b));
    }

    #[test]
    fn test_normalize_for_cache_end_to_end() {
        // Full end-to-end: normalized key should hit the cache.
        // Both calls use consistent args (bare read with ./ prefix).
        let mut cache = ToolCache::new();
        let args_a = json!({"path": "./src/main.rs"});
        let args_b = json!({"path": "src/main.rs"});
        let normalized_insert = normalize_for_cache("read", &args_a);
        cache.insert("read", &normalized_insert, "file content".into());
        let normalized_lookup = normalize_for_cache("read", &args_b);
        assert_eq!(cache.get("read", &normalized_lookup), Some("file content"));
    }

    #[test]
    fn test_normalize_path_for_key_dot_dot() {
        // src/../src/main.rs should normalize to src/main.rs
        assert_eq!(normalize_path_for_key("src/../src/main.rs"), "src/main.rs");
        // ../src/main.rs should stay as ../src/main.rs (can't go above root)
        assert_eq!(normalize_path_for_key("../src/main.rs"), "../src/main.rs");
        // ./src/../main.rs should normalize to main.rs
        assert_eq!(normalize_path_for_key("./src/../main.rs"), "main.rs");
        // a/b/../c should normalize to a/c
        assert_eq!(normalize_path_for_key("a/b/../c"), "a/c");
        // a/./b should normalize to a/b
        assert_eq!(normalize_path_for_key("a/./b"), "a/b");
        // Empty path should become "."
        assert_eq!(normalize_path_for_key(""), ".");
    }
}
