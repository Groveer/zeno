//! Skill content validation — reusable frontmatter and content checks.
//!
//! Extracted from `loader.rs` so both the loader and `skill_manage` tool
//! can validate SKILL.md content without duplicating logic.

use serde_yaml;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum skill name length (matches hermes-agent convention).
pub const MAX_NAME_LENGTH: usize = 64;

/// Maximum description length.
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// Maximum SKILL.md content size in characters (~36k tokens at 2.75 chars/token).
pub const MAX_SKILL_CONTENT_CHARS: usize = 100_000;

// ---------------------------------------------------------------------------
// Validation result
// ---------------------------------------------------------------------------

/// Result of frontmatter validation.
#[derive(Debug, Clone)]
pub struct FrontmatterInfo {
    /// Parsed `name` field (if present and valid).
    #[allow(dead_code, reason = "public API field, consumed by skill_manage tool")]
    pub name: Option<String>,
    /// Parsed `description` field (if present and valid).
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate that content has proper YAML frontmatter with required fields.
///
/// Checks:
/// 1. Content starts with `---`
/// 2. Closing `---` delimiter exists
/// 3. YAML parses to a mapping
/// 4. `name` field exists and is ≤ MAX_NAME_LENGTH
/// 5. `description` field exists and is ≤ MAX_DESCRIPTION_LENGTH
/// 6. Body content exists after frontmatter
///
/// Returns `Ok(FrontmatterInfo)` on success, `Err(message)` on failure.
pub fn validate_frontmatter(content: &str) -> Result<FrontmatterInfo, String> {
    if content.trim().is_empty() {
        return Err("Content cannot be empty.".into());
    }

    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return Err("SKILL.md must start with YAML frontmatter (---). \
             Example:\n---\nname: my-skill\ndescription: What this skill does\n---\n\n# Content"
            .into());
    }

    // Find closing ---
    let after_opening = &content[4..];
    let end_index = after_opening.find("\n---\n").or_else(|| {
        if after_opening.ends_with("\n---") {
            Some(after_opening.len() - 4)
        } else {
            None
        }
    });

    let end_index = match end_index {
        Some(i) => i,
        None => {
            return Err("Frontmatter is not closed. Ensure you have a closing '---' line.".into());
        }
    };

    let yaml_content = &after_opening[..end_index];
    let body = after_opening[end_index + 4..].trim();

    let parsed: serde_yaml::Value = match serde_yaml::from_str(yaml_content) {
        Ok(v) => v,
        Err(e) => return Err(format!("YAML frontmatter parse error: {}", e)),
    };

    let mapping = match parsed.as_mapping() {
        Some(m) => m,
        None => return Err("Frontmatter must be a YAML mapping (key: value pairs).".into()),
    };

    // Check name
    let name = mapping
        .get(serde_yaml::Value::String("name".into()))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());

    let name = match name {
        Some(n) if !n.is_empty() => {
            if n.len() > MAX_NAME_LENGTH {
                return Err(format!(
                    "Skill name exceeds {} characters (got {}).",
                    MAX_NAME_LENGTH,
                    n.len()
                ));
            }
            n
        }
        _ => return Err("Frontmatter must include a non-empty 'name' field.".into()),
    };

    // Check description
    let description = mapping
        .get(serde_yaml::Value::String("description".into()))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());

    let description = match description {
        Some(d) if !d.is_empty() => {
            if d.len() > MAX_DESCRIPTION_LENGTH {
                return Err(format!(
                    "Description exceeds {} characters (got {}).",
                    MAX_DESCRIPTION_LENGTH,
                    d.len()
                ));
            }
            d
        }
        _ => {
            return Err("Frontmatter must include a non-empty 'description' field.".into());
        }
    };

    // Check body
    if body.is_empty() {
        return Err(
            "SKILL.md must have content after the frontmatter (instructions, procedures, etc.)."
                .into(),
        );
    }

    // Check content size
    if content.len() > MAX_SKILL_CONTENT_CHARS {
        return Err(format!(
            "SKILL.md content is {:?} characters (limit: {}). Consider splitting into \
             a smaller SKILL.md with supporting files in references/ or templates/.",
            content.len(),
            MAX_SKILL_CONTENT_CHARS,
        ));
    }

    Ok(FrontmatterInfo {
        name: Some(name),
        description: Some(description),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_frontmatter() {
        let content =
            "---\nname: my-skill\ndescription: A test skill\n---\n# My Skill\nContent here";
        let info = validate_frontmatter(content).unwrap();
        assert_eq!(info.name.unwrap(), "my-skill");
        assert_eq!(info.description.unwrap(), "A test skill");
    }

    #[test]
    fn test_empty_content() {
        assert!(validate_frontmatter("").is_err());
        assert!(validate_frontmatter("   ").is_err());
    }

    #[test]
    fn test_no_frontmatter() {
        assert!(validate_frontmatter("# Just a heading\nSome content").is_err());
    }

    #[test]
    fn test_unclosed_frontmatter() {
        assert!(validate_frontmatter("---\nname: test\ndescription: desc").is_err());
    }

    #[test]
    fn test_missing_name() {
        let content = "---\ndescription: A test\n---\n# Content";
        assert!(validate_frontmatter(content).is_err());
    }

    #[test]
    fn test_missing_description() {
        let content = "---\nname: test\n---\n# Content";
        assert!(validate_frontmatter(content).is_err());
    }

    #[test]
    fn test_empty_body() {
        let content = "---\nname: test\ndescription: A test\n---\n";
        assert!(validate_frontmatter(content).is_err());
    }

    #[test]
    fn test_invalid_yaml() {
        let content = "---\n: invalid yaml : [\n---\n# Content";
        assert!(validate_frontmatter(content).is_err());
    }

    #[test]
    fn test_name_too_long() {
        let long_name = "a".repeat(65);
        let content = format!(
            "---\nname: {}\ndescription: test\n---\n# Content",
            long_name
        );
        assert!(validate_frontmatter(&content).is_err());
    }

    #[test]
    fn test_description_too_long() {
        let long_desc = "a".repeat(1025);
        let content = format!(
            "---\nname: test\ndescription: {}\n---\n# Content",
            long_desc
        );
        assert!(validate_frontmatter(&content).is_err());
    }
}
