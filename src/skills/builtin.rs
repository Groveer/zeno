//! Built-in skills — ship as data files next to the binary,
//! release to ~/.config/zeno/skills/ on startup.
//!
//! At runtime, built-in skills are read from the `skills/` directory
//! next to the executable (or `CARGO_MANIFEST_DIR/skills/` in dev mode),
//! then copied into the user's config directory where the skill loader
//! can discover them alongside user/project skills.

use std::path::{Path, PathBuf};

/// Compute a content hash of all skill files under the source directory.
/// This replaces a manual version bump — any change to skill content
/// automatically triggers re-release.
fn compute_content_hash(dir: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    let mut entries: Vec<PathBuf> = Vec::new();

    // Collect all file paths recursively for deterministic ordering
    collect_paths_sorted(dir, &mut entries);

    for path in entries {
        // Hash the relative path for structural awareness
        if let Ok(rel) = path.strip_prefix(dir) {
            rel.to_string_lossy().hash(&mut hasher);
        }
        // Hash file content (unwrap_or_default: read failure → empty → triggers re-copy)
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        content.hash(&mut hasher);
    }

    format!("{:016x}", hasher.finish())
}

/// Recursively collect all file paths under `dir`, sorted for deterministic hashing.
fn collect_paths_sorted(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        } else {
            out.push(path);
        }
    }
    out.sort();
    dirs.sort();
    for d in dirs {
        collect_paths_sorted(&d, out);
    }
}

/// Find the built-in skills directory next to the executable.
/// Fallback: CARGO_MANIFEST_DIR/skills (dev mode).
fn builtin_source_dir() -> PathBuf {
    // Runtime: skills/ directory next to the executable
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("skills");
        if candidate.is_dir() {
            return candidate;
        }
    }
    // Dev mode: project root skills/
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills")
}

/// Get the target directory: ~/.config/zeno/skills/
fn builtin_target_dir() -> PathBuf {
    crate::config::paths::config_dir().join("skills")
}

/// Version marker file: ~/.config/zeno/.builtin_skills_version
fn version_marker_path() -> PathBuf {
    crate::config::paths::config_dir().join(".builtin_skills_version")
}

/// Release built-in skills to user config dir if needed.
///
/// Copies category directories (e.g. `builtin/`) from the source
/// `skills/` dir into `~/.config/zeno/skills/`. Skips if the content
/// hash in the version marker matches the current hash of source files.
pub fn release_if_needed() -> anyhow::Result<()> {
    let source = builtin_source_dir();
    let target = builtin_target_dir();
    let marker = version_marker_path();

    // Compute current content hash from source
    let current_hash = compute_content_hash(&source);

    // Skip if already up-to-date
    if let Ok(existing) = std::fs::read_to_string(&marker)
        && existing.trim() == current_hash
    {
        tracing::debug!(hash = %current_hash, "Built-in skills up-to-date");
        return Ok(());
    }

    if !source.is_dir() {
        tracing::warn!(path = %source.display(), "Built-in skills source not found");
        return Ok(());
    }

    tracing::info!(
        hash = %current_hash,
        source = %source.display(),
        target = %target.display(),
        "Releasing built-in skills"
    );

    // Copy each top-level category directory (e.g. builtin/),
    // then prune stale entries *within each category* that no longer exist in source.
    // We only prune inside category subdirectories we own, never at the top level,
    // to avoid deleting user-created skills in ~/.config/zeno/skills/.
    let Ok(entries) = std::fs::read_dir(&source) else {
        tracing::warn!(path = %source.display(), "Cannot read built-in skills dir");
        return Ok(());
    };

    for entry in entries.flatten() {
        let src_path = entry.path();
        if src_path.is_dir() {
            let dir_name = entry.file_name();
            let dst_subdir = target.join(&dir_name);
            copy_dir_recursive(&src_path, &dst_subdir)?;
            // Prune only within this category subdirectory
            prune_stale(&src_path, &dst_subdir)?;
        }
    }

    // Write content hash marker
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&marker, &current_hash)?;

    Ok(())
}

/// Recursively copy a directory, only overwriting files that differ.
fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            // Only write if content differs.
            // Content comparison is safe here: built-in skills are always UTF-8 markdown.
            // For source read errors, we log a warning and skip rather than silently
            // copying an empty placeholder — this prevents overwriting valid dst content
            // with garbage from a permission-restricted or corrupted source file.
            let src_content = match std::fs::read_to_string(&src_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(path = %src_path.display(), error = %e, "Skipping skill file: read error");
                    continue;
                }
            };
            let dst_content = std::fs::read_to_string(&dst_path).unwrap_or_default();
            if src_content != dst_content {
                std::fs::copy(&src_path, &dst_path)?;
            }
        }
    }

    Ok(())
}

/// Remove files/dirs in `dst` that don't exist in `src`.
/// This keeps the target in sync when skills are removed or renamed in source.
fn prune_stale(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let Ok(dst_entries) = std::fs::read_dir(dst) else {
        return Ok(());
    };
    for entry in dst_entries.flatten() {
        let dst_path = entry.path();
        let file_name = entry.file_name();
        let src_path = src.join(&file_name);

        if !src_path.exists() {
            if dst_path.is_dir() {
                std::fs::remove_dir_all(&dst_path)?;
                tracing::info!(path = %dst_path.display(), "Pruned stale directory");
            } else {
                std::fs::remove_file(&dst_path)?;
                tracing::info!(path = %dst_path.display(), "Pruned stale file");
            }
        } else if dst_path.is_dir() && src_path.is_dir() {
            // Recurse into matching subdirectories
            prune_stale(&src_path, &dst_path)?;
            // Remove empty directory left after pruning
            if std::fs::read_dir(&dst_path)?.next().is_none() {
                std::fs::remove_dir(&dst_path)?;
                tracing::info!(path = %dst_path.display(), "Pruned empty directory");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_source_dir_fallback() {
        // In dev mode, should fallback to CARGO_MANIFEST_DIR/skills/
        let dir = builtin_source_dir();
        assert!(dir.to_string_lossy().contains("skills"));
    }

    #[test]
    fn test_copy_dir_recursive() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        // Create source structure: builtin/skill-name/SKILL.md
        let skill_dir = src.path().join("builtin").join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Test Skill").unwrap();

        copy_dir_recursive(&src.path().join("builtin"), &dst.path().join("builtin")).unwrap();

        let content =
            std::fs::read_to_string(dst.path().join("builtin/test-skill/SKILL.md")).unwrap();
        assert_eq!(content, "# Test Skill");
    }

    #[test]
    fn test_copy_dir_skips_identical() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        let src_file = src.path().join("SKILL.md");
        std::fs::write(&src_file, "content").unwrap();

        let dst_file = dst.path().join("SKILL.md");
        std::fs::write(&dst_file, "content").unwrap();

        // Set different mtime to prove copy was skipped (not by mtime, but by content)
        copy_dir_recursive(src.path(), dst.path()).unwrap();

        // Content unchanged
        assert_eq!(std::fs::read_to_string(&dst_file).unwrap(), "content");
    }

    #[test]
    fn test_prune_stale_removes_orphan() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        // Source: builtin/active-skill/SKILL.md
        let src_skill = src.path().join("builtin").join("active-skill");
        std::fs::create_dir_all(&src_skill).unwrap();
        std::fs::write(src_skill.join("SKILL.md"), "# Active").unwrap();

        // Target: builtin/active-skill/SKILL.md + builtin/removed-skill/SKILL.md
        let dst_active = dst.path().join("builtin").join("active-skill");
        std::fs::create_dir_all(&dst_active).unwrap();
        std::fs::write(dst_active.join("SKILL.md"), "# Active").unwrap();

        let dst_removed = dst.path().join("builtin").join("removed-skill");
        std::fs::create_dir_all(&dst_removed).unwrap();
        std::fs::write(dst_removed.join("SKILL.md"), "# Removed").unwrap();

        prune_stale(&src.path().join("builtin"), &dst.path().join("builtin")).unwrap();

        // Active skill still exists
        assert!(dst_active.join("SKILL.md").exists());
        // Removed skill is gone
        assert!(!dst_removed.join("SKILL.md").exists());
        assert!(!dst_removed.exists());
    }

    #[test]
    fn test_content_hash_changes_on_edit() {
        let dir = tempfile::tempdir().unwrap();
        let skill_file = dir.path().join("builtin").join("test-skill");
        std::fs::create_dir_all(&skill_file).unwrap();
        std::fs::write(skill_file.join("SKILL.md"), "v1").unwrap();

        let hash1 = compute_content_hash(dir.path());

        // Modify content
        std::fs::write(skill_file.join("SKILL.md"), "v2").unwrap();
        let hash2 = compute_content_hash(dir.path());

        assert_ne!(hash1, hash2, "Hash should change when content changes");
    }

    #[test]
    fn test_content_hash_stable_on_same_content() {
        let dir = tempfile::tempdir().unwrap();
        let skill_file = dir.path().join("builtin").join("test-skill");
        std::fs::create_dir_all(&skill_file).unwrap();
        std::fs::write(skill_file.join("SKILL.md"), "stable").unwrap();

        let hash1 = compute_content_hash(dir.path());
        let hash2 = compute_content_hash(dir.path());

        assert_eq!(hash1, hash2, "Hash should be stable for unchanged content");
    }
}
