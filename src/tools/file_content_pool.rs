//! File content pool — in-memory cache of file contents with read-range tracking.
//!
//! Avoids redundant disk I/O when the LLM reads the same file multiple times
//! (e.g. preview → explicit offset, or overlapping pagination ranges).
//!
//! # Design
//!
//! - Caches up to `MAX_FILES` files (LRU eviction)
//! - Tracks total bytes, evicts oldest when `MAX_TOTAL_BYTES` exceeded
//! - Per-file `read_ranges` tracks which 0-indexed line ranges have been returned
//! - On overlapping requests, returns a `ReadOutcome::Hit` with `covered_prefix`
//!   indicating how many leading lines were already seen
//! - Invalidated on write/edit via `remove()`
//! - Thread-safe via `Arc<Mutex<>>`

use std::collections::{HashMap, VecDeque};

/// Maximum files to keep in the pool.
const MAX_FILES: usize = 30;

/// Maximum files to include in the read-files summary injected into the prompt.
/// Keeps the system prompt concise — beyond this, just say "and N more files".
const MAX_SUMMARY_FILES: usize = 8;

/// Maximum total bytes across all cached files (50 MB).
const MAX_TOTAL_BYTES: usize = 50 * 1024 * 1024;

/// A cached file's content and metadata.
struct CachedFile {
    /// File lines (without line-number prefix).
    lines: Vec<String>,
    /// Total byte size of the content.
    byte_size: usize,
    /// Ranges of 0-indexed lines that have been returned to the LLM.
    /// Each entry is `(start, end)` where `end` is exclusive.
    read_ranges: Vec<(usize, usize)>,
}

/// Result of a pool lookup.
pub enum ReadOutcome {
    /// File was not in the pool (or was evicted). Caller should read from disk.
    Miss,
    /// File content was in the pool. Contains the full requested line range
    /// (start..end, 0-indexed, end exclusive) and a `covered_prefix` length:
    /// if > 0, the first `covered_prefix` lines in the range were already
    /// returned to the LLM in a previous call.
    Hit {
        lines: Vec<String>,
        start: usize,
        end: usize,
        covered_prefix: usize,
    },
}

pub struct FileContentPool {
    files: HashMap<String, CachedFile>,
    /// LRU order: front = most recently used, back = least recently used.
    lru: VecDeque<String>,
    /// Current total bytes across all cached files.
    total_bytes: usize,
}

impl std::fmt::Debug for FileContentPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileContentPool")
            .field("files", &self.files.len())
            .field("total_bytes", &self.total_bytes)
            .finish()
    }
}

impl FileContentPool {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            lru: VecDeque::new(),
            total_bytes: 0,
        }
    }

    /// Load a file into the pool (or replace if already present).
    /// Preserves existing `read_ranges` if the content is unchanged,
    /// so concurrent inserts by another task won't reset tracking.
    /// Returns the total line count.
    pub fn insert_preserving_ranges(&mut self, resolved_path: &str, content: &str) -> usize {
        let byte_size = content.len();
        let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
        let total_lines = lines.len();

        // Capture existing read_ranges if content matches.
        // Fast-path: skip expensive line-by-line comparison when byte_size differs.
        let existing_ranges = self.files.get(resolved_path).and_then(|existing| {
            if existing.byte_size != byte_size {
                return None;
            }
            if existing.lines == lines {
                Some(existing.read_ranges.clone())
            } else {
                None
            }
        });

        // Remove existing entry (but we already captured the ranges)
        self.remove_internal(resolved_path);

        // Evict LRU entries until we have room
        while (self.files.len() >= MAX_FILES)
            || (self.total_bytes + byte_size > MAX_TOTAL_BYTES && !self.files.is_empty())
        {
            if let Some(oldest_key) = self.lru.pop_back()
                && let Some(removed) = self.files.remove(&oldest_key)
            {
                self.total_bytes -= removed.byte_size;
            }
        }

        self.total_bytes += byte_size;
        self.files.insert(
            resolved_path.to_string(),
            CachedFile {
                lines,
                byte_size,
                read_ranges: existing_ranges.unwrap_or_default(),
            },
        );
        self.lru.push_front(resolved_path.to_string());

        total_lines
    }

    /// Load a file into the pool (or replace if already present).
    /// Returns the total line count.
    ///
    /// Note: prefer `insert_preserving_ranges` in production code to avoid
    /// resetting read tracking on concurrent inserts.
    #[allow(dead_code)]
    pub fn insert(&mut self, resolved_path: &str, content: &str) -> usize {
        let byte_size = content.len();
        let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
        let total_lines = lines.len();

        // Remove existing entry if present
        self.remove_internal(resolved_path);

        // Evict LRU entries until we have room
        while (self.files.len() >= MAX_FILES)
            || (self.total_bytes + byte_size > MAX_TOTAL_BYTES && !self.files.is_empty())
        {
            if let Some(oldest_key) = self.lru.pop_back()
                && let Some(removed) = self.files.remove(&oldest_key)
            {
                self.total_bytes -= removed.byte_size;
            }
        }

        self.total_bytes += byte_size;
        self.files.insert(
            resolved_path.to_string(),
            CachedFile {
                lines,
                byte_size,
                read_ranges: Vec::new(),
            },
        );
        self.lru.push_front(resolved_path.to_string());

        total_lines
    }

    /// Look up a file and request a range of lines.
    ///
    /// Returns `ReadOutcome::Hit` with the requested lines and overlap info,
    /// or `ReadOutcome::Miss` if the file is not cached.
    pub fn read_range(&mut self, resolved_path: &str, start: usize, end: usize) -> ReadOutcome {
        let file = match self.files.get(resolved_path) {
            Some(f) => f,
            None => return ReadOutcome::Miss,
        };

        let total_lines = file.lines.len();
        let clamped_start = start.min(total_lines);
        let clamped_end = end.min(total_lines);

        if clamped_start >= clamped_end {
            // Empty range — still a hit, just nothing to return
            self.promote_lru(resolved_path);
            return ReadOutcome::Hit {
                lines: Vec::new(),
                start: clamped_start,
                end: clamped_end,
                covered_prefix: 0,
            };
        }

        // Calculate overlap with previously read ranges (immutable access)
        let covered_prefix = self.count_covered_lines(resolved_path, clamped_start, clamped_end);

        // Extract the requested lines (immutable access)
        let lines = file.lines[clamped_start..clamped_end].to_vec();

        // Mark this range as read (mutable access — done after reads)
        self.mark_range_read(resolved_path, clamped_start, clamped_end);

        self.promote_lru(resolved_path);

        ReadOutcome::Hit {
            lines,
            start: clamped_start,
            end: clamped_end,
            covered_prefix,
        }
    }

    /// Remove a file from the pool (called after write/edit).
    pub fn remove(&mut self, resolved_path: &str) {
        self.remove_internal(resolved_path);
    }

    /// Check if a file is cached.
    #[cfg(test)]
    pub fn contains(&self, resolved_path: &str) -> bool {
        self.files.contains_key(resolved_path)
    }

    /// Get the total line count for a cached file (None if not cached).
    pub fn total_lines(&self, resolved_path: &str) -> Option<usize> {
        self.files.get(resolved_path).map(|f| f.lines.len())
    }

    /// Number of files in the pool.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Build a concise summary of all files that have been read in this session.
    ///
    /// Returns `None` when no files have read ranges (pool empty or nothing read).
    /// The output is formatted as a compact list suitable for injecting into the
    /// system prompt, so the LLM knows what it already has in context and can
    /// avoid redundant re-reads.
    ///
    /// Format per entry:
    /// ```
    /// - path/to/file.rs (fully read, Z lines)
    /// - path/to/file.rs (lines 5-30 of Z, P% read)
    /// - path/to/file.rs (lines 5-100 of Z, P% read, 60 of Z unique lines)
    /// ```
    /// When coverage is contiguous: shows exact range + percentage.
    /// When coverage has gaps (multiple segments): shows span + unique lines count.
    /// When only a small portion: shows approximate line count.
    /// Capped at `MAX_SUMMARY_FILES` entries; excess shown as "and N more files".
    pub fn read_files_summary(&self) -> Option<String> {
        if self.files.is_empty() {
            return None;
        }

        // Collect files that have at least one read_range (files inserted but
        // never read via read_range() have empty ranges and are excluded).
        let mut entries: Vec<(
            /*path*/ &str,
            /*total*/ usize,
            /*covered*/ usize,
            /*min_start*/ usize,
            /*max_end*/ usize,
            /*range_count*/ usize,
        )> = self
            .files
            .iter()
            .filter_map(|(path, f)| {
                if f.read_ranges.is_empty() {
                    return None;
                }
                // Ranges are already merged by mark_range_read — sorted,
                // non-overlapping, non-adjacent. Sum for unique line coverage.
                let mut covered = 0usize;
                let mut min_start = usize::MAX;
                let mut max_end = 0usize;
                for &(s, e) in &f.read_ranges {
                    covered += e.saturating_sub(s);
                    min_start = min_start.min(s);
                    max_end = max_end.max(e);
                }
                if covered == 0 {
                    return None;
                }
                Some((
                    path.as_str(),
                    f.lines.len(),
                    covered,
                    min_start,
                    max_end,
                    f.read_ranges.len(),
                ))
            })
            .collect();

        if entries.is_empty() {
            return None;
        }

        // Sort by most-covered first (the most context-dominant files are most relevant)
        entries.sort_by(|a, b| b.2.cmp(&a.2));

        let mut lines: Vec<String> = Vec::new();
        let total = entries.len();
        let display_count = total.min(MAX_SUMMARY_FILES);

        for &(path, total_lines, covered, min_start, max_end, range_count) in
            entries.iter().take(display_count)
        {
            let pct = (covered as f64 / total_lines as f64 * 100.0).round() as u8;
            let line_info = if covered >= total_lines {
                format!("fully read, {} lines", total_lines)
            } else if covered <= 200 {
                // Small range — show approximate line count instead of percentage
                // to avoid misleading percentages (e.g. "10 of 200 = 5%").
                format!("~{} lines of {}", covered, total_lines)
            } else if range_count == 1 {
                // Single contiguous segment — show exact 1-indexed range
                format!(
                    "lines {}-{} of {} ({}%)",
                    min_start + 1,
                    max_end,
                    total_lines,
                    pct
                )
            } else {
                // Multiple segments — show span and unique count so LLM knows there are gaps
                format!(
                    "lines {}-{} ({} of {} unique lines, {}%)",
                    min_start + 1,
                    max_end,
                    covered,
                    total_lines,
                    pct
                )
            };
            lines.push(format!("- {} ({})", path, line_info));
        }

        if total > display_count {
            lines.push(format!("- ... and {} more files", total - display_count));
        }

        Some(lines.join("\n"))
    }

    // --- Internal helpers ---

    /// Remove a key from the LRU deque in a single scan.
    fn remove_lru_key(&mut self, key: &str) {
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(pos);
        }
    }

    fn remove_internal(&mut self, resolved_path: &str) {
        if let Some(removed) = self.files.remove(resolved_path) {
            self.total_bytes -= removed.byte_size;
        }
        self.remove_lru_key(resolved_path);
    }

    fn promote_lru(&mut self, resolved_path: &str) {
        // Fast path: already at front — nothing to do.
        if self.lru.front().map(|k| k.as_str()) == Some(resolved_path) {
            return;
        }
        self.remove_lru_key(resolved_path);
        self.lru.push_front(resolved_path.to_string());
    }

    /// Count how many lines in [start, end) are already covered by previous reads.
    ///
    /// Uses binary search since `read_ranges` is always sorted and coalesced.
    /// Complexity: O(log k + m) where k = number of ranges, m = number of
    /// overlapping ranges (typically 1-2).
    fn count_covered_lines(&self, resolved_path: &str, start: usize, end: usize) -> usize {
        let file = match self.files.get(resolved_path) {
            Some(f) => f,
            None => return 0,
        };

        let ranges = &file.read_ranges;
        if ranges.is_empty() {
            return 0;
        }

        // Binary search: find the first range whose end > start (could overlap)
        let idx = ranges.partition_point(|&(_, re)| re <= start);

        let mut covered = 0usize;
        for &(rs, re) in &ranges[idx..] {
            if rs >= end {
                break; // Past our range — ranges are sorted, no more overlaps
            }
            // Overlap is [max(start, rs), min(end, re))
            let overlap_start = start.max(rs);
            let overlap_end = end.min(re);
            covered += overlap_end - overlap_start;
        }

        covered
    }

    /// Merge a new range [start, end) into the file's read_ranges.
    fn mark_range_read(&mut self, resolved_path: &str, start: usize, end: usize) {
        let file = match self.files.get_mut(resolved_path) {
            Some(f) => f,
            None => return,
        };

        // Simple merge: add the new range and coalesce overlapping/adjacent ranges
        file.read_ranges.push((start, end));
        file.read_ranges.sort_by_key(|&(s, _)| s);

        // Coalesce overlapping/adjacent ranges
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for &(s, e) in &file.read_ranges {
            if let Some(last) = merged.last_mut()
                && s <= last.1
            {
                last.1 = last.1.max(e);
                continue;
            }
            merged.push((s, e));
        }
        file.read_ranges = merged;
    }
}

/// Thread-safe shared file content pool.
pub type SharedFileContentPool = std::sync::Arc<tokio::sync::Mutex<FileContentPool>>;

/// Create a new shared file content pool.
pub fn new_shared() -> SharedFileContentPool {
    std::sync::Arc::new(tokio::sync::Mutex::new(FileContentPool::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_read() {
        let mut pool = FileContentPool::new();
        let content = "line 0\nline 1\nline 2\nline 3\nline 4\n";
        pool.insert("/test.rs", content);

        match pool.read_range("/test.rs", 0, 3) {
            ReadOutcome::Hit {
                lines,
                start,
                end,
                covered_prefix,
            } => {
                assert_eq!(start, 0);
                assert_eq!(end, 3);
                assert_eq!(lines.len(), 3);
                assert_eq!(lines[0], "line 0");
                assert_eq!(covered_prefix, 0); // First read — no overlap
            }
            ReadOutcome::Miss => panic!("expected hit"),
        }
    }

    #[test]
    fn test_overlap_detection() {
        let mut pool = FileContentPool::new();
        let content = "a\nb\nc\nd\ne\n";
        pool.insert("/test.rs", content);

        // First read: lines 0-3 (a, b, c)
        let r1 = pool.read_range("/test.rs", 0, 3);
        match &r1 {
            ReadOutcome::Hit { covered_prefix, .. } => assert_eq!(*covered_prefix, 0),
            _ => panic!("expected hit"),
        }

        // Second read: lines 2-5 (c, d, e) — line 2 (c) was already read
        let r2 = pool.read_range("/test.rs", 2, 5);
        match r2 {
            ReadOutcome::Hit {
                lines,
                covered_prefix,
                ..
            } => {
                assert_eq!(lines.len(), 3);
                assert_eq!(lines[0], "c");
                assert_eq!(covered_prefix, 1); // "c" was already returned
            }
            _ => panic!("expected hit"),
        }
    }

    #[test]
    fn test_full_overlap() {
        let mut pool = FileContentPool::new();
        pool.insert("/test.rs", "a\nb\nc\n");

        // Read all
        pool.read_range("/test.rs", 0, 3);

        // Read same range again — fully covered
        match pool.read_range("/test.rs", 0, 3) {
            ReadOutcome::Hit { covered_prefix, .. } => {
                assert_eq!(covered_prefix, 3); // All 3 lines were already returned
            }
            _ => panic!("expected hit"),
        }
    }

    #[test]
    fn test_miss() {
        let mut pool = FileContentPool::new();
        pool.insert("/test.rs", "a\nb\n");

        match pool.read_range("/other.rs", 0, 2) {
            ReadOutcome::Miss => {}
            _ => panic!("expected miss"),
        }
    }

    #[test]
    fn test_remove() {
        let mut pool = FileContentPool::new();
        pool.insert("/test.rs", "a\nb\n");
        assert!(pool.contains("/test.rs"));

        pool.remove("/test.rs");
        assert!(!pool.contains("/test.rs"));

        match pool.read_range("/test.rs", 0, 2) {
            ReadOutcome::Miss => {}
            _ => panic!("expected miss after remove"),
        }
    }

    #[test]
    fn test_lru_eviction() {
        let mut pool = FileContentPool::new();
        // Set a low limit for testing
        // (We can't easily change MAX_FILES, so test with fewer files)
        for i in 0..MAX_FILES {
            pool.insert(&format!("/file_{}.rs", i), &format!("content {}", i));
        }
        assert_eq!(pool.len(), MAX_FILES);

        // Access file_0 to promote it
        pool.read_range("/file_0.rs", 0, 1);

        // Insert one more — should evict the LRU (file_1, since file_0 was promoted)
        pool.insert("/new.rs", "new content");
        assert_eq!(pool.len(), MAX_FILES);
        assert!(pool.contains("/file_0.rs")); // Promoted — still present
        assert!(pool.contains("/new.rs")); // Newly inserted
    }

    #[test]
    fn test_range_coalescing() {
        let mut pool = FileContentPool::new();
        pool.insert("/test.rs", "0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n");

        // Read 0-3, then 2-6, then 5-10 — should coalesce to (0, 10)
        pool.read_range("/test.rs", 0, 3);
        pool.read_range("/test.rs", 2, 6);
        pool.read_range("/test.rs", 5, 10);

        // Now reading 0-10 should be fully covered
        match pool.read_range("/test.rs", 0, 10) {
            ReadOutcome::Hit { covered_prefix, .. } => {
                assert_eq!(covered_prefix, 10); // All 10 lines covered
            }
            _ => panic!("expected fully covered hit"),
        }
    }

    #[test]
    fn test_insert_preserving_ranges_same_content() {
        let mut pool = FileContentPool::new();
        pool.insert("/test.rs", "a\nb\nc\n");

        // Read some lines to build up read_ranges
        pool.read_range("/test.rs", 0, 2);

        // Re-insert same content — ranges should be preserved
        pool.insert_preserving_ranges("/test.rs", "a\nb\nc\n");

        // Lines 0-1 should still be covered
        match pool.read_range("/test.rs", 0, 3) {
            ReadOutcome::Hit { covered_prefix, .. } => {
                assert_eq!(covered_prefix, 2); // Lines 0-1 were previously read
            }
            _ => panic!("expected hit"),
        }
    }

    #[test]
    fn test_insert_preserving_ranges_different_content() {
        let mut pool = FileContentPool::new();
        pool.insert("/test.rs", "a\nb\nc\n");

        // Read some lines
        pool.read_range("/test.rs", 0, 2);

        // Re-insert different content — ranges should be reset
        pool.insert_preserving_ranges("/test.rs", "x\ny\nz\n");

        match pool.read_range("/test.rs", 0, 3) {
            ReadOutcome::Hit { covered_prefix, .. } => {
                assert_eq!(covered_prefix, 0); // Fresh — no overlap
            }
            _ => panic!("expected hit"),
        }
    }

    #[test]
    fn test_read_files_summary_empty_pool() {
        let pool = FileContentPool::new();
        assert!(pool.read_files_summary().is_none());

        // Insert a file but never read it — should still be None
        let mut pool = FileContentPool::new();
        pool.insert("/empty.rs", "a\nb\nc\n");
        assert!(pool.read_files_summary().is_none());
    }

    #[test]
    fn test_read_files_summary_fully_read() {
        let mut pool = FileContentPool::new();
        let content = "aaa\nbbb\nccc\nddd\neee\n";
        pool.insert("/test.rs", content);
        pool.read_range("/test.rs", 0, 5);

        let summary = pool.read_files_summary().unwrap();
        assert!(summary.contains("/test.rs"));
        assert!(summary.contains("fully read, 5 lines"));
    }

    #[test]
    fn test_read_files_summary_partial_read() {
        let mut pool = FileContentPool::new();
        let content = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        pool.insert("/partial.rs", content);
        // Read only first 3 lines
        pool.read_range("/partial.rs", 0, 3);

        let summary = pool.read_files_summary().unwrap();
        assert!(summary.contains("/partial.rs"));
    }

    #[test]
    fn test_read_files_summary_multiple_files() {
        let mut pool = FileContentPool::new();
        pool.insert("/a.rs", "1\n2\n3\n");
        pool.insert("/b.rs", "x\ny\nz\n");
        pool.read_range("/a.rs", 0, 3);
        pool.read_range("/b.rs", 0, 3);

        let summary = pool.read_files_summary().unwrap();
        assert!(summary.contains("/a.rs"));
        assert!(summary.contains("/b.rs"));
    }
}
