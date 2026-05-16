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

    // common_prefix_suffix_len

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

    // compress_edit_input

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
