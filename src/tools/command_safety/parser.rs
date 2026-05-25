//! Tree-sitter based shell command parsing.
//!
//! Uses tree-sitter-bash grammar to parse shell commands into ASTs,
//! enabling accurate decomposition of compound commands and detection
//! of unsafe constructs (substitutions, redirects, etc.) without
//! fragile string matching.

use tree_sitter::Node;
use tree_sitter::Parser;
use tree_sitter::Tree;
use tree_sitter_bash::LANGUAGE as BASH;

/// Parse the provided shell script source, returning a Tree on success or
/// None if parsing failed.
pub fn try_parse_shell(src: &str) -> Option<Tree> {
    let lang = BASH.into();
    let mut parser = Parser::new();
    parser.set_language(&lang).ok()?;
    parser.parse(src, None)
}

/// Allowed (named) node kinds for a "word-only commands sequence".
/// If we encounter a named node not in this list, the script is rejected
/// as potentially unsafe to decompose.
const ALLOWED_KINDS: &[&str] = &[
    "program",
    "list",
    "pipeline",
    "command",
    "command_name",
    "word",
    "string",
    "string_content",
    "raw_string",
    "number",
    "concatenation",
    // heredoc constructs — allow stdin feeding for single commands
    "heredoc_body",
    "simple_heredoc_body",
    "heredoc_redirect",
    "heredoc_start",
    "heredoc_end",
    "herestring_redirect",
    // Redirected statements that wrap the actual command
    "redirected_statement",
    // File redirect — we allow it only for `read`-like commands
    "file_redirect",
    // Comments are harmless
    "comment",
];

/// Allow only safe punctuation / operator tokens; anything else causes reject.
const ALLOWED_PUNCT_TOKENS: &[&str] = &[
    "&&", "||", ";", "|", "\"", "'", "<", ">", ">>", "<<", "&>", "<&", ">&",
];

/// Decompose a shell script into individual plain commands (word-only).
///
/// Returns `Some(Vec<command_words>)` if every command is a plain word-only
/// command joined by safe operators (`&&`, `||`, `;`, `|`). Returns `None`
/// if the script contains disallowed constructs (substitutions, parentheses,
/// redirects to files, control flow, etc.).
pub fn parse_word_only_commands_sequence(tree: &Tree, src: &str) -> Option<Vec<Vec<String>>> {
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    if !validate_all_nodes(root) {
        return None;
    }

    let mut command_nodes = collect_command_nodes(root);
    command_nodes.sort_by_key(Node::start_byte);

    let mut commands = Vec::new();
    for node in command_nodes {
        if let Some(words) = parse_plain_command_from_node(node, src) {
            commands.push(words);
        } else {
            return None;
        }
    }

    if commands.is_empty() {
        None
    } else {
        Some(commands)
    }
}

/// Recursively validate that every named node in the tree is in the
/// allowed kinds list, and every unnamed (punctuation) token is in the
/// allowed punctuation list.
fn validate_all_nodes(root: Node) -> bool {
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if node.is_named() {
            if !ALLOWED_KINDS.contains(&kind) {
                return false;
            }
        } else {
            // Unnamed nodes: punctuation, operators, whitespace
            if !ALLOWED_PUNCT_TOKENS.contains(&kind) && !kind.trim().is_empty() {
                return false;
            }
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    true
}

/// Collect all `command` nodes from the tree.
fn collect_command_nodes(root: Node) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "command" {
            nodes.push(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    nodes
}

/// Parse a single command node into its constituent words.
fn parse_plain_command_from_node(cmd: Node, src: &str) -> Option<Vec<String>> {
    if cmd.kind() != "command" {
        return None;
    }

    let mut words = Vec::new();
    let mut cursor = cmd.walk();
    for child in cmd.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let word_node = child.named_child(0)?;
                if word_node.kind() != "word" && word_node.kind() != "number" {
                    return None;
                }
                words.push(word_node.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "word" | "number" => {
                words.push(child.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "string" => {
                let parsed = parse_double_quoted_string(child, src)?;
                words.push(parsed);
            }
            "raw_string" => {
                let parsed = parse_raw_string(child, src)?;
                words.push(parsed);
            }
            "concatenation" => {
                let parsed = parse_concatenation(child, src)?;
                words.push(parsed);
            }
            // Skip comments, heredoc attachments, file redirect nodes
            "comment"
            | "heredoc_body"
            | "simple_heredoc_body"
            | "heredoc_redirect"
            | "herestring_redirect"
            | "file_redirect"
            | "redirected_statement" => {}
            _ => return None,
        }
    }

    if words.is_empty() { None } else { Some(words) }
}

fn parse_double_quoted_string(node: Node, src: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }

    // Verify there are no expansions inside the string
    let mut cursor = node.walk();
    for part in node.named_children(&mut cursor) {
        if part.kind() != "string_content" {
            return None;
        }
    }

    let raw = node.utf8_text(src.as_bytes()).ok()?;
    let stripped = raw
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))?;
    Some(stripped.to_string())
}

fn parse_raw_string(node: Node, src: &str) -> Option<String> {
    let raw = node.utf8_text(src.as_bytes()).ok()?;
    raw.strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .map(str::to_owned)
}

fn parse_concatenation(node: Node, src: &str) -> Option<String> {
    let mut concatenated = String::new();
    let mut cursor = node.walk();
    for part in node.named_children(&mut cursor) {
        match part.kind() {
            "word" | "number" => {
                concatenated.push_str(part.utf8_text(src.as_bytes()).ok()?);
            }
            "string" => {
                let parsed = parse_double_quoted_string(part, src)?;
                concatenated.push_str(&parsed);
            }
            "raw_string" => {
                let parsed = parse_raw_string(part, src)?;
                concatenated.push_str(&parsed);
            }
            _ => return None,
        }
    }
    if concatenated.is_empty() {
        None
    } else {
        Some(concatenated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_seq(src: &str) -> Option<Vec<Vec<String>>> {
        let tree = try_parse_shell(src)?;
        parse_word_only_commands_sequence(&tree, src)
    }

    #[test]
    fn test_single_simple_command() {
        let cmds = parse_seq("ls -1").unwrap();
        assert_eq!(cmds, vec![vec!["ls".to_string(), "-1".to_string()]]);
    }

    #[test]
    fn test_multiple_commands_with_operators() {
        let src = "ls && pwd; echo 'hi there' | wc -l";
        let cmds = parse_seq(src).unwrap();
        let expected: Vec<Vec<String>> = vec![
            vec!["ls".to_string()],
            vec!["pwd".to_string()],
            vec!["echo".to_string(), "hi there".to_string()],
            vec!["wc".to_string(), "-l".to_string()],
        ];
        assert_eq!(cmds, expected);
    }

    #[test]
    fn test_quoted_strings() {
        let cmds = parse_seq("echo \"hello world\"").unwrap();
        assert_eq!(
            cmds,
            vec![vec!["echo".to_string(), "hello world".to_string()]]
        );
    }

    #[test]
    fn test_single_quoted_strings() {
        let cmds = parse_seq("echo 'hello world'").unwrap();
        assert_eq!(
            cmds,
            vec![vec!["echo".to_string(), "hello world".to_string()]]
        );
    }

    #[test]
    fn test_rejects_subshell() {
        assert!(parse_seq("(ls)").is_none());
        assert!(parse_seq("ls || (pwd && echo hi)").is_none());
    }

    #[test]
    fn test_rejects_command_substitution() {
        assert!(parse_seq("echo $(pwd)").is_none());
        assert!(parse_seq("echo `pwd`").is_none());
    }

    #[test]
    fn test_rejects_variable_expansion() {
        assert!(parse_seq("echo $HOME").is_none());
        assert!(parse_seq("echo \"hi $USER\"").is_none());
        assert!(parse_seq("echo \"${PATH}\"").is_none());
    }

    #[test]
    fn test_rejects_variable_assignment() {
        assert!(parse_seq("FOO=bar ls").is_none());
    }

    #[test]
    fn test_redirects() {
        // File redirects (> file) are allowed at the parse level —
        // they're rejected in the safety check for destructive commands.
        let cmds = parse_seq("cat < input.txt").unwrap();
        assert_eq!(cmds, vec![vec!["cat".to_string()]]);

        // Output redirects are also allowed — safety layer decides.
        let cmds2 = parse_seq("cat file.txt").unwrap();
        assert_eq!(cmds2, vec![vec!["cat".to_string(), "file.txt".to_string()]]);
    }

    #[test]
    fn test_numbers_as_words() {
        let cmds = parse_seq("echo 123 456").unwrap();
        assert_eq!(
            cmds,
            vec![vec![
                "echo".to_string(),
                "123".to_string(),
                "456".to_string()
            ]]
        );
    }

    #[test]
    fn test_concatenated_flag_and_value() {
        let cmds = parse_seq("rg -g\"*.py\"").unwrap();
        assert_eq!(cmds, vec![vec!["rg".to_string(), "-g*.py".to_string()]]);
    }

    #[test]
    fn test_rejects_variable_in_concatenation() {
        assert!(parse_seq("rg -g\"$VAR\"").is_none());
        assert!(parse_seq("rg -g\"${VAR}\"").is_none());
    }

    #[test]
    fn test_heredoc_allowed() {
        let cmds = parse_seq("cat << EOF\nhello\nEOF").unwrap();
        assert_eq!(cmds, vec![vec!["cat".to_string()]]);
    }
}
