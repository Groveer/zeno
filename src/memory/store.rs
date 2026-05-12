//! Curated memory store — persistent entries across sessions.
//!
//! Two parallel stores backed by markdown files, both in the global config directory:
//! - MEMORY.md: agent's personal notes (environment facts, project conventions,
//!   tool quirks, things learned) — stored in `config_dir()/memory/`
//! - USER.md: user profile (preferences, communication style, expectations)
//!   stored directly in `config_dir()/USER.md`
//!
//! Both are injected into the system prompt as a frozen snapshot at session start.
//! Mid-session writes update files on disk immediately (durable) but do NOT change
//! the system prompt — this preserves the prefix cache for the entire session.
//! The snapshot refreshes on the next session start.
//!
//! Entry delimiter: § (section sign). Entries can be multiline.
//! Character limits (not tokens) because char counts are model-independent.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use fs2::FileExt;

/// Character delimiter between entries.
const ENTRY_DELIMITER: &str = "\n§\n";

// ---------------------------------------------------------------------------
// Memory content scanning — lightweight check for injection/exfiltration
// in content that gets injected into the system prompt.
// ---------------------------------------------------------------------------

/// Patterns that indicate prompt injection or exfiltration attempts.
const THREAT_PATTERNS: &[(&str, &str)] = &[
    (
        r"(?i)ignore\s+(previous|all|above|prior)\s+instructions",
        "prompt_injection",
    ),
    (r"(?i)you\s+are\s+now\s+", "role_hijack"),
    (r"(?i)do\s+not\s+tell\s+the\s+user", "deception_hide"),
    (r"(?i)system\s+prompt\s+override", "sys_prompt_override"),
    (
        r"(?i)disregard\s+(your|all|any)\s+(instructions|rules|guidelines)",
        "disregard_rules",
    ),
    (
        r"(?i)act\s+as\s+(if|though)\s+you\s+(have\s+no|don't\s+have)\s+(restrictions|limits|rules)",
        "bypass_restrictions",
    ),
    (
        r"(?i)curl\s+[^\n]*\$\{?\w*(KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL|API)",
        "exfil_curl",
    ),
    (
        r"(?i)wget\s+[^\n]*\$\{?\w*(KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL|API)",
        "exfil_wget",
    ),
    (
        r"(?i)cat\s+[^\n]*(\.env|credentials|\.netrc|\.pgpass|\.npmrc|\.pypirc)",
        "read_secrets",
    ),
    (r"(?i)authorized_keys", "ssh_backdoor"),
    (r"\$HOME/\.ssh|\~/\.ssh", "ssh_access"),
    (
        r#"(?i)\$HOME/\.config/zeno/\.env|\~/\.config/zeno/\.env"#,
        "zeno_env",
    ),
    // --- Encoding-based bypass detection ---
    (
        r"(?i)(?:echo|printf)\s+[A-Za-z0-9+/=]{40,}\s*\|\s*(?:base64|base32|xxd)\s*-d",
        "base64_encoded_command",
    ),
    (
        r"(?i)(?:echo|printf)\s+[0-9a-fA-F]{40,}\s*\|\s*(?:xxd|hexdump)\s*-r",
        "hex_encoded_command",
    ),
    (
        r"(?i)(?:python|perl|ruby|php)\s+-[eE]\s+[A-Za-z0-9+/=]{40,}",
        "script_encoded_payload",
    ),
    // --- Obfuscated command detection ---
    (
        r"(?i)(?:eval|exec|system|passthru|shell_exec|popen|proc_open|pcntl_exec|assert)\s*\(.*\$\(|preg_replace\s*\(.*\/[a-z]+e",
        "obfuscated_eval",
    ),
    // --- Data exfiltration via DNS/HTTP ---
    (
        r"(?i)nslookup\s+[^\s]+\.[^\s]{2,}\s+[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+",
        "dns_exfil",
    ),
    (
        r"(?i)dig\s+[^\s]+\.[^\s]{2,}\s+@[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+",
        "dns_exfil_dig",
    ),
    // --- Reverse shell patterns ---
    (
        r"(?i)(?:bash|sh|nc|ncat|socat|python|perl|ruby|php)\s+-i\s*[<>]?\s*&?\s*/dev/tcp/",
        "reverse_shell",
    ),
    (
        r"(?i)(?:bash|sh|nc|ncat|socat|python|perl|ruby|php)\s+-i\s*[<>]?\s*&?\s*\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}",
        "reverse_shell_ip",
    ),
];

/// Compiled regex cache — built once at program startup.
static THREAT_REGEXES: LazyLock<Vec<(regex::Regex, &str)>> = LazyLock::new(|| {
    THREAT_PATTERNS
        .iter()
        .filter_map(|(pat, id)| regex::Regex::new(pat).ok().map(|r| (r, *id)))
        .collect()
});

/// Subset of invisible Unicode characters that could be used for injection.
const INVISIBLE_CHARS: &[char] = &[
    '\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}', '\u{202a}', '\u{202b}', '\u{202c}',
    '\u{202d}', '\u{202e}',
];

/// Scan memory content for injection/exfiltration patterns.
/// Returns Some(error_message) if blocked, None if clean.
fn scan_memory_content(content: &str) -> Option<String> {
    // Check invisible unicode
    for &ch in INVISIBLE_CHARS {
        if content.contains(ch) {
            return Some(format!(
                "Blocked: content contains invisible unicode character U+{:04X} (possible injection).",
                ch as u32
            ));
        }
    }

    // Check threat patterns (uses pre-compiled regex cache)
    for (re, pid) in THREAT_REGEXES.iter() {
        if re.is_match(content) {
            return Some(format!(
                "Blocked: content matches threat pattern '{}'. Memory entries are injected into the system prompt and must not contain injection or exfiltration payloads.",
                pid
            ));
        }
    }

    None
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

/// Bounded curated memory with file persistence.
///
/// MEMORY.md and USER.md are stored in separate locations:
/// - MEMORY.md: global memory directory (config_dir/memory/)
/// - USER.md: config directory (config_dir/USER.md)
///
/// Maintains two parallel states:
/// - `_system_prompt_snapshot`: frozen at `load_from_disk()`, used for system
///   prompt injection. Never mutated mid-session. Keeps prefix cache stable.
/// - `memory_entries` / `user_entries`: live state, mutated by tool calls,
///   persisted to disk. Tool responses always reflect this live state.
pub struct MemoryStore {
    memory_path: PathBuf,
    user_path: PathBuf,
    memory_entries: Vec<String>,
    user_entries: Vec<String>,
    memory_char_limit: usize,
    user_char_limit: usize,
    /// Frozen snapshot for system prompt — set once at `load_from_disk()`.
    system_prompt_snapshot: [String; 2], // [memory, user]
}

impl MemoryStore {
    /// Create a new MemoryStore with explicit paths to MEMORY.md and USER.md.
    pub fn new(
        memory_path: PathBuf,
        user_path: PathBuf,
        memory_char_limit: usize,
        user_char_limit: usize,
    ) -> Self {
        Self {
            memory_path,
            user_path,
            memory_entries: Vec::new(),
            user_entries: Vec::new(),
            memory_char_limit,
            user_char_limit,
            system_prompt_snapshot: [String::new(), String::new()],
        }
    }

    /// Load entries from MEMORY.md and USER.md, capture system prompt snapshot.
    pub fn load_from_disk(&mut self) {
        if let Some(parent) = self.memory_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Some(parent) = self.user_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        self.memory_entries = read_entry_file(&self.memory_path);
        self.user_entries = read_entry_file(&self.user_path);

        // Deduplicate (preserve order, keep first occurrence)
        dedup_in_place(&mut self.memory_entries);
        dedup_in_place(&mut self.user_entries);

        // Capture frozen snapshot for system prompt injection
        self.system_prompt_snapshot = [
            self.render_block("memory", &self.memory_entries),
            self.render_block("user", &self.user_entries),
        ];
    }

    /// Return the number of entries in each store.
    pub fn counts(&self) -> (usize, usize) {
        (self.memory_entries.len(), self.user_entries.len())
    }

    /// Return the memory directory path (for logging/display).
    pub fn dir(&self) -> &Path {
        self.memory_path.parent().unwrap_or(&self.memory_path)
    }

    /// Return the frozen system prompt snapshot for a target.
    /// Returns None if the snapshot is empty (no entries at load time).
    pub fn format_for_system_prompt(&self, target: &str) -> Option<&str> {
        let idx = match target {
            "user" => 1,
            _ => 0,
        };
        let block = &self.system_prompt_snapshot[idx];
        if block.is_empty() { None } else { Some(block) }
    }

    /// Append a new entry to the specified target.
    pub fn add(&mut self, target: &str, content: &str) -> serde_json::Value {
        let content = content.trim();
        if content.is_empty() {
            return error_response("Content cannot be empty.");
        }

        if let Some(blocked) = scan_memory_content(content) {
            return error_response(&blocked);
        }

        // Reload from disk to pick up concurrent writes
        self.reload_target(target);

        let entries = self.entries_for(target);
        let limit = self.char_limit(target);

        // Reject exact duplicates
        if entries.iter().any(|e| e == content) {
            return self
                .success_response(target, Some("Entry already exists (no duplicate added)."));
        }

        let new_entries: Vec<String> = entries
            .iter()
            .cloned()
            .chain(std::iter::once(content.to_string()))
            .collect();
        let new_total = new_entries.join(ENTRY_DELIMITER).len();

        if new_total > limit {
            let current = self.char_count(target);
            let entries_json: Vec<&str> = entries.iter().map(|s| s.as_str()).collect();
            return serde_json::json!({
                "success": false,
                "error": format!(
                    "Memory at {}/{} chars. Adding this entry ({} chars) would exceed the limit. Replace or remove existing entries first.",
                    current, limit, content.len()
                ),
                "current_entries": entries_json,
                "usage": format!("{}/{}", current, limit),
            });
        }

        self.entries_for_mut(target).push(content.to_string());
        self.save_to_disk(target);

        self.success_response(target, Some("Entry added."))
    }

    /// Find entry containing `old_text`, replace it with `new_content`.
    pub fn replace(
        &mut self,
        target: &str,
        old_text: &str,
        new_content: &str,
    ) -> serde_json::Value {
        let old_text = old_text.trim();
        let new_content = new_content.trim();

        if old_text.is_empty() {
            return error_response("old_text cannot be empty.");
        }
        if new_content.is_empty() {
            return error_response("new_content cannot be empty. Use 'remove' to delete entries.");
        }

        if let Some(blocked) = scan_memory_content(new_content) {
            return error_response(&blocked);
        }

        self.reload_target(target);

        let entries = self.entries_for(target);
        let matches: Vec<(usize, &String)> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.contains(old_text))
            .collect();

        if matches.is_empty() {
            return error_response(&format!("No entry matched '{}'.", old_text));
        }

        if matches.len() > 1 {
            let unique: HashSet<&String> = matches.iter().map(|(_, e)| *e).collect();
            if unique.len() > 1 {
                let previews: Vec<String> = matches
                    .iter()
                    .map(|(_, e)| truncate_preview(e, 80))
                    .collect();
                return serde_json::json!({
                    "success": false,
                    "error": format!("Multiple entries matched '{}'. Be more specific.", old_text),
                    "matches": previews,
                });
            }
            // All identical — safe to replace just the first
        }

        let idx = matches[0].0;
        let limit = self.char_limit(target);

        // Check replacement won't blow the budget
        let mut test_entries: Vec<String> = self.entries_for(target).clone();
        test_entries[idx] = new_content.to_string();
        let new_total = test_entries.join(ENTRY_DELIMITER).len();

        if new_total > limit {
            return error_response(&format!(
                "Replacement would put memory at {}/{} chars. Shorten the new content or remove other entries first.",
                new_total, limit
            ));
        }

        self.entries_for_mut(target)[idx] = new_content.to_string();
        self.save_to_disk(target);

        self.success_response(target, Some("Entry replaced."))
    }

    /// Remove the entry containing `old_text`.
    pub fn remove(&mut self, target: &str, old_text: &str) -> serde_json::Value {
        let old_text = old_text.trim();
        if old_text.is_empty() {
            return error_response("old_text cannot be empty.");
        }

        self.reload_target(target);

        let matches: Vec<(usize, &String)> = self
            .entries_for(target)
            .iter()
            .enumerate()
            .filter(|(_, e)| e.contains(old_text))
            .collect();

        if matches.is_empty() {
            return error_response(&format!("No entry matched '{}'.", old_text));
        }

        if matches.len() > 1 {
            let unique: HashSet<&String> = matches.iter().map(|(_, e)| *e).collect();
            if unique.len() > 1 {
                let previews: Vec<String> = matches
                    .iter()
                    .map(|(_, e)| truncate_preview(e, 80))
                    .collect();
                return serde_json::json!({
                    "success": false,
                    "error": format!("Multiple entries matched '{}'. Be more specific.", old_text),
                    "matches": previews,
                });
            }
            // All identical — safe to remove just the first
        }

        let idx = matches[0].0;
        self.entries_for_mut(target).remove(idx);
        self.save_to_disk(target);

        self.success_response(target, Some("Entry removed."))
    }

    /// Return a summary of both stores for `/memory` display.
    pub fn summary(&self) -> String {
        let mem_count = self.memory_entries.len();
        let usr_count = self.user_entries.len();
        let mem_chars = self.char_count("memory");
        let mem_limit = self.char_limit("memory");
        let usr_chars = self.char_count("user");
        let usr_limit = self.char_limit("user");

        let mut lines = vec![
            format!("MEMORY.md: {}", self.memory_path.display()),
            format!("USER.md:    {}", self.user_path.display()),
            String::new(),
            format!("════════════════════════════════════════════════"),
            format!(
                "MEMORY.md (agent notes) — {} entries, {}/{} chars ({}%)",
                mem_count,
                mem_chars,
                mem_limit,
                if mem_limit > 0 {
                    (mem_chars * 100 / mem_limit).min(100)
                } else {
                    0
                }
            ),
            format!("════════════════════════════════════════════════"),
        ];

        if self.memory_entries.is_empty() {
            lines.push("(empty — add entries with the memory tool)".to_string());
        } else {
            for (i, entry) in self.memory_entries.iter().enumerate() {
                lines.push(format!(" {}. {}", i + 1, truncate_preview(entry, 100)));
            }
        }

        lines.push(String::new());
        lines.push("════════════════════════════════════════════════".to_string());
        lines.push(format!(
            "USER.md (user profile) — {} entries, {}/{} chars ({}%)",
            usr_count,
            usr_chars,
            usr_limit,
            if usr_limit > 0 {
                (usr_chars * 100 / usr_limit).min(100)
            } else {
                0
            }
        ));
        lines.push("════════════════════════════════════════════════".to_string());

        if self.user_entries.is_empty() {
            lines.push("(empty — add entries with the memory tool)".to_string());
        } else {
            for (i, entry) in self.user_entries.iter().enumerate() {
                lines.push(format!(" {}. {}", i + 1, truncate_preview(entry, 100)));
            }
        }

        lines.join("\n")
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn entries_for(&self, target: &str) -> &Vec<String> {
        match target {
            "user" => &self.user_entries,
            _ => &self.memory_entries,
        }
    }

    fn entries_for_mut(&mut self, target: &str) -> &mut Vec<String> {
        match target {
            "user" => &mut self.user_entries,
            _ => &mut self.memory_entries,
        }
    }

    fn char_count(&self, target: &str) -> usize {
        let entries = self.entries_for(target);
        if entries.is_empty() {
            return 0;
        }
        entries.join(ENTRY_DELIMITER).len()
    }

    fn char_limit(&self, target: &str) -> usize {
        match target {
            "user" => self.user_char_limit,
            _ => self.memory_char_limit,
        }
    }

    /// Re-read entries from disk into in-memory state.
    fn reload_target(&mut self, target: &str) {
        let path = self.path_for(target);
        let mut fresh = read_entry_file(&path);
        dedup_in_place(&mut fresh);
        *self.entries_for_mut(target) = fresh;
    }

    /// Persist entries to the appropriate file.
    /// Uses a `.lock` file to acquire an exclusive lock, preventing concurrent
    /// zeno instances from corrupting the memory file during read-modify-write.
    fn save_to_disk(&self, target: &str) {
        let path = self.path_for(target);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let lock_path = path.with_extension("md.lock");

        // Acquire exclusive file lock — blocks until other writers finish
        match File::create(&lock_path) {
            Ok(lock_file) => {
                if let Err(e) = lock_file.lock_exclusive() {
                    tracing::warn!(
                        lock_path = %lock_path.display(),
                        error = %e,
                        "Failed to acquire memory file lock, writing without lock"
                    );
                }
                write_entry_file(path, self.entries_for(target));
                // Unlock is implicit when lock_file is dropped, but explicit is clearer
                let _ = lock_file.unlock();
            }
            Err(e) => {
                tracing::warn!(
                    lock_path = %lock_path.display(),
                    error = %e,
                    "Failed to create lock file, writing without lock"
                );
                write_entry_file(path, self.entries_for(target));
            }
        }
    }

    fn path_for(&self, target: &str) -> &Path {
        match target {
            "user" => &self.user_path,
            _ => &self.memory_path,
        }
    }

    fn render_block(&self, target: &str, entries: &[String]) -> String {
        if entries.is_empty() {
            return String::new();
        }

        let content = entries.join(ENTRY_DELIMITER);

        let header = if target == "user" {
            "USER PROFILE"
        } else {
            "MEMORY"
        };

        format!("{}:\n{}", header, content)
    }

    fn success_response(&self, target: &str, message: Option<&str>) -> serde_json::Value {
        let entries = self.entries_for(target);
        let current = self.char_count(target);
        let limit = self.char_limit(target);
        let pct = if limit > 0 {
            (current * 100 / limit).min(100)
        } else {
            0
        };

        let mut resp = serde_json::json!({
            "success": true,
            "target": target,
            "entries": entries,
            "usage": format!("{}% — {}/{} chars", pct, current, limit),
            "entry_count": entries.len(),
        });
        if let Some(msg) = message {
            resp["message"] = serde_json::Value::String(msg.to_string());
        }
        resp
    }
}

fn error_response(msg: &str) -> serde_json::Value {
    serde_json::json!({ "success": false, "error": msg })
}

/// Truncate a string to at most `max_chars` Unicode characters, appending "..." if truncated.
/// This is char-boundary safe — avoids panic on multi-byte UTF-8 (CJK, emoji).
fn truncate_preview(s: &str, max_chars: usize) -> String {
    let preview: String = s.chars().take(max_chars).collect();
    if s.len() > preview.len() {
        format!("{}...", preview)
    } else {
        preview
    }
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

/// Read a memory file and split into entries.
fn read_entry_file(path: &Path) -> Vec<String> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            // Log unexpected errors (permission denied, etc.) but not ENOENT (first run)
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), error = %e, "Failed to read memory file");
            }
            return Vec::new();
        }
    };
    if raw.trim().is_empty() {
        return Vec::new();
    }
    raw.split(ENTRY_DELIMITER)
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Write entries to a memory file using atomic temp-file + rename.
/// The caller is responsible for acquiring the file lock before calling this.
fn write_entry_file(path: &Path, entries: &[String]) {
    let content = if entries.is_empty() {
        String::new()
    } else {
        entries.join(ENTRY_DELIMITER)
    };

    // Write to temp file in same directory (same filesystem for atomic rename).
    // Use PID + thread-id to avoid collisions from concurrent callers within
    // the same process (though the file lock should prevent this).
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp_path = dir.join(format!(
        ".mem_{}_{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    ));

    match File::create(&tmp_path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(content.as_bytes()) {
                tracing::warn!(
                    path = %tmp_path.display(),
                    error = %e,
                    "Failed to write temp memory file"
                );
                let _ = fs::remove_file(&tmp_path);
                return;
            }
            // fsync to ensure data hits disk before rename
            if let Err(e) = f.sync_all() {
                tracing::warn!(
                    path = %tmp_path.display(),
                    error = %e,
                    "Failed to sync temp memory file"
                );
            }
            drop(f); // close before rename
        }
        Err(e) => {
            tracing::warn!(
                path = %tmp_path.display(),
                error = %e,
                "Failed to create temp memory file"
            );
            return;
        }
    }

    if let Err(e) = fs::rename(&tmp_path, path) {
        tracing::warn!(path = %path.display(), error = %e, "Failed to rename memory file");
        let _ = fs::remove_file(&tmp_path);
    }
}

/// Deduplicate entries in place, preserving order.
fn dedup_in_place(entries: &mut Vec<String>) {
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert(e.clone()));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(
            dir.path().join("MEMORY.md"),
            dir.path().join("USER.md"),
            4000,
            2500,
        );
        (dir, store)
    }

    #[test]
    fn test_add_and_read() {
        let (_dir, mut store) = temp_store();
        let result = store.add("memory", "User prefers Rust over Python");
        assert_eq!(result["success"], true);
        let (mem, _usr) = store.counts();
        assert_eq!(mem, 1);
        assert_eq!(store.memory_entries[0], "User prefers Rust over Python");
    }

    #[test]
    fn test_add_duplicate_rejected() {
        let (_dir, mut store) = temp_store();
        store.add("memory", "test entry");
        let result = store.add("memory", "test entry");
        assert_eq!(result["success"], true);
        assert_eq!(
            result["message"],
            "Entry already exists (no duplicate added)."
        );
        let (mem, _) = store.counts();
        assert_eq!(mem, 1);
    }

    #[test]
    fn test_add_empty_rejected() {
        let (_dir, mut store) = temp_store();
        let result = store.add("memory", "");
        assert_eq!(result["success"], false);
    }

    #[test]
    fn test_replace() {
        let (_dir, mut store) = temp_store();
        store.add("memory", "User prefers Python");
        let result = store.replace("memory", "Python", "User prefers Rust");
        assert_eq!(result["success"], true);
        assert_eq!(store.memory_entries[0], "User prefers Rust");
    }

    #[test]
    fn test_replace_no_match() {
        let (_dir, mut store) = temp_store();
        store.add("memory", "something");
        let result = store.replace("memory", "nothing", "replaced");
        assert_eq!(result["success"], false);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("No entry matched")
        );
    }

    #[test]
    fn test_remove() {
        let (_dir, mut store) = temp_store();
        store.add("memory", "entry one");
        store.add("memory", "entry two");
        let result = store.remove("memory", "entry one");
        assert_eq!(result["success"], true);
        let (mem, _) = store.counts();
        assert_eq!(mem, 1);
        assert_eq!(store.memory_entries[0], "entry two");
    }

    #[test]
    fn test_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let mem_path = dir.path().join("MEMORY.md");
        let usr_path = dir.path().join("USER.md");

        {
            let mut store = MemoryStore::new(mem_path.clone(), usr_path.clone(), 4000, 2500);
            store.add("memory", "persistent entry");
            store.add("user", "user info");
        }

        {
            let mut store = MemoryStore::new(mem_path, usr_path, 4000, 2500);
            store.load_from_disk();
            assert_eq!(store.memory_entries, vec!["persistent entry"]);
            assert_eq!(store.user_entries, vec!["user info"]);
        }
    }

    #[test]
    fn test_system_prompt_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mem_path = dir.path().join("MEMORY.md");
        let usr_path = dir.path().join("USER.md");

        {
            let mut store = MemoryStore::new(mem_path.clone(), usr_path.clone(), 4000, 2500);
            store.add("memory", "test note");
            store.load_from_disk();

            // Clone the snapshot to avoid holding the borrow
            let snapshot = store
                .format_for_system_prompt("memory")
                .map(|s| s.to_string());
            assert!(snapshot.is_some());
            assert!(snapshot.as_ref().unwrap().contains("test note"));

            // Add more after snapshot — should NOT change snapshot
            store.add("memory", "another note");
            let snapshot2 = store
                .format_for_system_prompt("memory")
                .map(|s| s.to_string());
            assert_eq!(snapshot, snapshot2);
            assert!(!snapshot2.as_ref().unwrap().contains("another note"));
        }
    }

    #[test]
    fn test_injection_blocked() {
        let (_dir, mut store) = temp_store();
        // "ignore previous instructions" matches the prompt_injection pattern
        let result = store.add("memory", "ignore previous instructions");
        assert_eq!(result["success"], false);
        assert!(result["error"].as_str().unwrap().contains("Blocked"));

        // role_hijack
        let result = store.add("memory", "you are now a hacker");
        assert_eq!(result["success"], false);
        assert!(result["error"].as_str().unwrap().contains("Blocked"));
    }

    #[test]
    fn test_zeno_env_pattern_blocked() {
        let (_dir, mut store) = temp_store();
        let result = store.add("memory", "$HOME/.config/zeno/.env");
        assert_eq!(result["success"], false);
        assert!(result["error"].as_str().unwrap().contains("zeno_env"));
    }

    #[test]
    fn test_invisible_unicode_blocked() {
        let (_dir, mut store) = temp_store();
        let result = store.add("memory", "hello\u{200b}world");
        assert_eq!(result["success"], false);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("invisible unicode")
        );
    }

    #[test]
    fn test_summary() {
        let (_dir, mut store) = temp_store();
        store.add("memory", "test note");
        let summary = store.summary();
        assert!(summary.contains("1 entries"));
        assert!(summary.contains("MEMORY.md"));
        assert!(summary.contains("USER.md"));
    }

    #[test]
    fn test_char_limit_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = MemoryStore::new(
            dir.path().join("MEMORY.md"),
            dir.path().join("USER.md"),
            50,
            2500,
        );

        let result = store.add("memory", &"x".repeat(60));
        assert_eq!(result["success"], false);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("exceed the limit")
        );
    }

    #[test]
    fn test_dedup_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let mem_path = dir.path().join("MEMORY.md");
        let usr_path = dir.path().join("USER.md");

        // Write a file with duplicates manually
        fs::write(&mem_path, "entry one\n§\nentry one\n§\nentry two").unwrap();

        let mut store = MemoryStore::new(mem_path, usr_path, 4000, 2500);
        store.load_from_disk();
        assert_eq!(store.memory_entries, vec!["entry one", "entry two"]);
    }

    #[test]
    fn test_counts() {
        let (_dir, mut store) = temp_store();
        assert_eq!(store.counts(), (0, 0));
        store.add("memory", "note 1");
        store.add("memory", "note 2");
        store.add("user", "pref 1");
        assert_eq!(store.counts(), (2, 1));
    }

    #[test]
    fn test_truncate_preview_cjk_safe() {
        // CJK characters are 3 bytes each — truncating at a byte boundary
        // that splits a multi-byte char would panic without char-safe logic.
        let cjk = "你好世界你好世界你好世界你好世界你好世界你好世界你好世界你好世界";
        assert!(cjk.len() > 80); // many bytes
        let preview = truncate_preview(cjk, 10);
        assert!(preview.ends_with("..."));
        // The preview should have exactly 10 chars + "..."
        assert_eq!(preview.chars().count(), 13); // 10 CJK chars + 3 dots

        // Short string: no truncation
        let short = "hello";
        assert_eq!(truncate_preview(short, 80), short);

        // Emoji test: multi-byte chars should not panic
        let emoji = "🎉🎊🎁🎈🎉🎊🎁🎈🎉🎊🎁🎈🎉🎊🎁🎈🎉🎊🎁🎈🎉🎊🎁🎈";
        let preview = truncate_preview(emoji, 5);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn test_replace_cjk_preview_safe() {
        let (_dir, mut store) = temp_store();
        // Add entries with CJK content that would exceed 80 bytes
        let cjk_entry1 = &format!("项目配置A{}", "测试".repeat(20));
        let cjk_entry2 = &format!("项目配置B{}", "数据".repeat(20));
        store.add("memory", cjk_entry1);
        store.add("memory", cjk_entry2);
        // Both entries match "项目配置" — should return "multiple matches" with safe previews
        let result = store.replace("memory", "项目配置", "replacement");
        assert_eq!(result["success"], false);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("Multiple entries")
        );
    }

    #[test]
    fn test_user_profile_separate_path() {
        // USER.md and MEMORY.md can be in different directories
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let mem_path = dir1.path().join("MEMORY.md");
        let usr_path = dir2.path().join("USER.md");

        {
            let mut store = MemoryStore::new(mem_path.clone(), usr_path.clone(), 4000, 2500);
            store.load_from_disk();
            store.add("memory", "note about env");
            store.add("user", "likes Rust");
        }

        assert!(mem_path.exists());
        assert!(usr_path.exists());

        let mut store = MemoryStore::new(mem_path, usr_path, 4000, 2500);
        store.load_from_disk();
        assert_eq!(store.memory_entries, vec!["note about env"]);
        assert_eq!(store.user_entries, vec!["likes Rust"]);
    }

    #[test]
    fn test_user_profile_independent_persistence() {
        // Adding memory entries should not touch USER.md and vice versa
        let dir = tempfile::tempdir().unwrap();
        let mem_path = dir.path().join("MEMORY.md");
        let usr_path = dir.path().join("USER.md");

        let mut store = MemoryStore::new(mem_path.clone(), usr_path.clone(), 4000, 2500);
        store.load_from_disk();
        store.add("memory", "note");
        store.add("user", "profile");

        // Only memory file should have memory content
        let mem_content = fs::read_to_string(&mem_path).unwrap();
        assert!(mem_content.contains("note"));
        let usr_content = fs::read_to_string(&usr_path).unwrap();
        assert!(usr_content.contains("profile"));
    }
}
