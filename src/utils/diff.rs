/// Compute the longest common prefix and suffix of two string slices.
/// Returns (prefix_count, suffix_count) in lines.
///
/// prefix = count of identical lines from the start.
/// suffix = count of identical lines from the end (clamped to avoid overlap).
fn common_prefix_suffix_len(old: &[&str], new: &[&str]) -> (usize, usize) {
    let prefix = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .take(max_suffix)
        .count();

    (prefix, suffix)
}

/// Compute an edit diff for display purposes.
///
/// Given the old_string and new_string from an edit tool call, finds the
/// common prefix and suffix lines, then returns only the changed portion
/// with 1 line of context on each side.
///
/// Returns a list of diff lines, each prefixed with "-" or "+".
/// Unchanged context lines are included without prefix.
pub fn compute_edit_diff(old_str: &str, new_str: &str) -> Vec<String> {
    let old_lines: Vec<&str> = old_str.lines().collect();
    let new_lines: Vec<&str> = new_str.lines().collect();

    if old_lines.is_empty() && new_lines.is_empty() {
        return Vec::new();
    }

    // Single-line edits — just show old and new
    if old_lines.len() == 1 && new_lines.len() == 1 {
        let mut result = Vec::new();
        result.push(format!("-{}", old_lines[0]));
        result.push(format!("+{}", new_lines[0]));
        return result;
    }

    let (prefix, suffix) = common_prefix_suffix_len(&old_lines, &new_lines);

    // If everything is common (identical), nothing to diff.
    // Lengths must also match — otherwise one side has extra lines (pure add/delete).
    if old_lines.len() == new_lines.len() && prefix + suffix >= old_lines.len() {
        return Vec::new();
    }

    let old_diff_start = prefix.saturating_sub(1); // 1 line of context before
    let old_diff_end = old_lines.len() - suffix;
    let new_diff_start = prefix.saturating_sub(1);
    let new_diff_end = new_lines.len() - suffix;

    let mut result = Vec::new();

    // Context: first common line (if we skipped some prefix)
    if prefix > 1 {
        result.push(format!(" {}", old_lines[0])); // first line for orientation
        if prefix > 2 {
            result.push(" ...".into());
        }
    }

    // Removed lines with context
    for line in &old_lines[old_diff_start..old_diff_end] {
        result.push(format!("-{}", line));
    }
    // Added lines with context
    for line in &new_lines[new_diff_start..new_diff_end] {
        result.push(format!("+{}", line));
    }

    // Context: last common line (if we skipped some suffix)
    if suffix > 1 {
        if suffix > 2 {
            result.push(" ...".into());
        }
        result.push(format!(" {}", old_lines[old_lines.len() - 1]));
    }

    result
}

/// Compress the `old_string` and `new_string` fields inside an edit tool's
/// `input` JSON value, stripping the common prefix/suffix context lines.
///
/// This reduces token consumption for historical ToolUse blocks since
/// the full context is no longer needed after the edit is applied.
///
/// Only modifies the value if compression saves at least 20% of characters.
pub fn compress_edit_input(input: &mut serde_json::Value) {
    use serde_json::json;

    let old_str = match input.get("old_string").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    let new_str = input
        .get("new_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let old_lines: Vec<&str> = old_str.lines().collect();
    let new_lines: Vec<&str> = new_str.lines().collect();

    // Need at least 2 lines of context to make compression worthwhile
    if old_lines.len() < 2 && new_lines.len() < 2 {
        return;
    }

    let (prefix, suffix) = common_prefix_suffix_len(&old_lines, &new_lines);

    // Nothing to strip if no common lines
    if prefix == 0 && suffix == 0 {
        return;
    }

    // If all lines are common and lengths match (identical strings with
    // possible trailing whitespace), there's nothing meaningful to compress.
    // When lengths differ, one side has extra lines worth compressing out.
    if old_lines.len() == new_lines.len() && prefix + suffix >= old_lines.len() {
        return;
    }

    // Extract only the diff portion with 1 line context on each side
    let old_diff_start = prefix.saturating_sub(1);
    let old_diff_end = old_lines.len() - suffix;
    let new_diff_start = prefix.saturating_sub(1);
    let new_diff_end = new_lines.len() - suffix;

    let compact_old: String = old_lines[old_diff_start..old_diff_end].join("\n");
    let compact_new: String = new_lines[new_diff_start..new_diff_end].join("\n");

    let original_len = old_str.len() + new_str.len();
    let compact_len = compact_old.len() + compact_new.len();

    // Only apply if we save at least 20%
    if compact_len < original_len * 80 / 100 {
        let mut new_input = input.clone();
        new_input["old_string"] = json!(compact_old);
        new_input["new_string"] = json!(compact_new);
        *input = new_input;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── common_prefix_suffix_len ────────────────────────────────────────

    #[test]
    fn prefix_suffix_basic() {
        let old = ["a", "b", "c", "d", "e"];
        let new = ["a", "b", "x", "y", "e"];
        assert_eq!(common_prefix_suffix_len(&old, &new), (2, 1));
    }

    #[test]
    fn prefix_suffix_identical() {
        let lines = ["a", "b", "c"];
        assert_eq!(common_prefix_suffix_len(&lines, &lines), (3, 0));
    }

    #[test]
    fn prefix_suffix_no_common() {
        let old = ["a", "b"];
        let new = ["x", "y"];
        assert_eq!(common_prefix_suffix_len(&old, &new), (0, 0));
    }

    #[test]
    fn prefix_suffix_all_common_prefix() {
        // old is a prefix of new
        let old = ["a", "b"];
        let new = ["a", "b", "c"];
        assert_eq!(common_prefix_suffix_len(&old, &new), (2, 0));
    }

    #[test]
    fn prefix_suffix_single_change_at_start() {
        let old = ["X", "b", "c", "d"];
        let new = ["Y", "b", "c", "d"];
        assert_eq!(common_prefix_suffix_len(&old, &new), (0, 3));
    }

    // ── compute_edit_diff ───────────────────────────────────────────────

    #[test]
    fn diff_single_line() {
        let diff = compute_edit_diff("old_thing()", "new_thing()");
        assert_eq!(diff, vec!["-old_thing()", "+new_thing()"]);
    }

    #[test]
    fn diff_multi_line_with_context() {
        let old = "  fn init() {\n    let cfg = load();\n    do_old();\n    cleanup();\n  }";
        let new = "  fn init() {\n    let cfg = load();\n    do_new();\n    cleanup();\n  }";
        let diff = compute_edit_diff(old, new);
        // Should show 1 context line, the change, and 1 context line
        assert!(diff.iter().any(|l| l.starts_with("-    do_old()")));
        assert!(diff.iter().any(|l| l.starts_with("+    do_new()")));
        // Should NOT show all 5 lines
        assert!(diff.len() < 8, "too many diff lines: {:?}", diff);
    }

    #[test]
    fn diff_identical_strings() {
        let diff = compute_edit_diff("same\nlines", "same\nlines");
        assert!(diff.is_empty());
    }

    #[test]
    fn diff_empty_strings() {
        let diff = compute_edit_diff("", "");
        assert!(diff.is_empty());
    }

    #[test]
    fn diff_append_lines_at_end() {
        // new = old + extra lines at the end → should show additions, not "no diff"
        let old = "line1\nline2\nline3";
        let new = "line1\nline2\nline3\nline4\nline5";
        let diff = compute_edit_diff(old, new);
        assert!(
            !diff.is_empty(),
            "should not be empty when lines are appended"
        );
        assert!(diff.iter().any(|l| l.starts_with("+line4")));
        assert!(diff.iter().any(|l| l.starts_with("+line5")));
    }

    #[test]
    fn diff_delete_lines_at_end() {
        // new = prefix of old → should show deletions
        let old = "line1\nline2\nline3\nline4\nline5";
        let new = "line1\nline2\nline3";
        let diff = compute_edit_diff(old, new);
        assert!(
            !diff.is_empty(),
            "should not be empty when lines are deleted at end"
        );
        assert!(diff.iter().any(|l| l.starts_with("-line4")));
        assert!(diff.iter().any(|l| l.starts_with("-line5")));
    }

    #[test]
    fn diff_many_context_lines() {
        // 10 lines context + 1 line changed + 10 lines context
        let mut old_lines: Vec<String> = (0..10).map(|i| format!("context_{}", i)).collect();
        old_lines.push("    do_old_thing();".into());
        old_lines.extend((0..10).map(|i| format!("after_{}", i)));
        let old = old_lines.join("\n");

        let mut new_lines: Vec<String> = (0..10).map(|i| format!("context_{}", i)).collect();
        new_lines.push("    do_new_thing(extra);".into());
        new_lines.extend((0..10).map(|i| format!("after_{}", i)));
        let new = new_lines.join("\n");

        let diff = compute_edit_diff(&old, &new);
        // Should have "..." markers for truncated context
        assert!(diff.iter().any(|l| l.contains("...")));
        // Should show the actual change
        assert!(diff.iter().any(|l| l.contains("do_old_thing")));
        assert!(diff.iter().any(|l| l.contains("do_new_thing")));
    }

    // ── compress_edit_input ─────────────────────────────────────────────

    #[test]
    fn compress_strips_common_context() {
        let mut input = json!({
            "path": "main.rs",
            "old_string": "  fn init() {\n    let cfg = load();\n    do_old();\n    cleanup();\n  }",
            "new_string": "  fn init() {\n    let cfg = load();\n    do_new(extra);\n    cleanup();\n  }"
        });
        let original = input.clone();
        compress_edit_input(&mut input);
        let new_old = input["old_string"].as_str().unwrap();
        let new_new = input["new_string"].as_str().unwrap();
        // Should be shorter
        assert!(new_old.len() < original["old_string"].as_str().unwrap().len());
        assert!(new_new.len() < original["new_string"].as_str().unwrap().len());
    }

    #[test]
    fn compress_single_line_noop() {
        let mut input = json!({
            "path": "main.rs",
            "old_string": "old_thing()",
            "new_string": "new_thing()"
        });
        let original = input.clone();
        compress_edit_input(&mut input);
        // Single-line edits: no common prefix/suffix to strip
        assert_eq!(input, original);
    }

    #[test]
    fn compress_small_change_noop() {
        // If compression saves less than 20%, don't bother
        let mut input = json!({
            "path": "main.rs",
            "old_string": "a\nb\nc\nd",
            "new_string": "a\nb\nc\nd\n"
        });
        let original = input.clone();
        compress_edit_input(&mut input);
        assert_eq!(input, original);
    }
}
