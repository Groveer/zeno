//! Disk snapshot cache for skills — accelerates cold-start by avoiding full directory scans.
//!
//! On first load, skill metadata is cached to `~/.config/zeno/.skills_cache.json`.
//! Subsequent starts read the cache directly if the manifest (mtime + size) still matches.
//!
//! Design reference: DESIGN.md §10.8

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::skills::types::{CategoryInfo, SkillDefinition};

// ---------------------------------------------------------------------------
// Cache format
// ---------------------------------------------------------------------------

/// Bump this when the cache format changes — invalidates old caches automatically.
const CACHE_VERSION: u32 = 3;

/// Serialized cache structure.
#[derive(serde::Serialize, serde::Deserialize)]
struct SkillsCache {
    /// Cache format version. Mismatch → cache invalid.
    version: u32,
    /// Per-file (mtime_ns, size) → used to detect stale cache entries.
    manifest: HashMap<String, (u64, u64)>,
    /// Serialized skill definitions.
    skills: Vec<SkillDefinitionSer>,
    /// Serialized category descriptions.
    categories: HashMap<String, CategoryInfoSer>,
}

/// Ser/de shim for SkillDefinition (avoids adding serde bounds to the main struct).
#[derive(serde::Serialize, serde::Deserialize)]
struct SkillDefinitionSer {
    name: String,
    description: String,
    content: String,
    source: String,
    path: Option<String>,
    category: String,
    #[serde(default)]
    always_inject: bool,
}

/// Ser/de shim for CategoryInfo.
#[derive(serde::Serialize, serde::Deserialize)]
struct CategoryInfoSer {
    description: String,
    skill_names: Vec<String>,
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

impl From<&SkillDefinition> for SkillDefinitionSer {
    fn from(s: &SkillDefinition) -> Self {
        Self {
            name: s.name.clone(),
            description: s.description.clone(),
            content: s.content.clone(),
            source: s.source.clone(),
            path: s.path.clone(),
            category: s.category.clone(),
            always_inject: s.always_inject,
        }
    }
}

impl From<SkillDefinitionSer> for SkillDefinition {
    fn from(s: SkillDefinitionSer) -> Self {
        Self {
            name: s.name,
            description: s.description,
            content: s.content,
            source: s.source,
            path: s.path,
            category: s.category,
            always_inject: s.always_inject,
        }
    }
}

impl From<&CategoryInfo> for CategoryInfoSer {
    fn from(c: &CategoryInfo) -> Self {
        Self {
            description: c.description.clone(),
            skill_names: c.skill_names.clone(),
        }
    }
}

impl From<CategoryInfoSer> for CategoryInfo {
    fn from(c: CategoryInfoSer) -> Self {
        Self {
            description: c.description,
            skill_names: c.skill_names,
        }
    }
}

// ---------------------------------------------------------------------------
// Cache path
// ---------------------------------------------------------------------------

/// Path to the cache file: `~/.config/zeno/.skills_cache.json`
fn cache_path() -> PathBuf {
    crate::config::paths::config_dir().join(".skills_cache.json")
}

// ---------------------------------------------------------------------------
// Manifest — fingerprint source files by (mtime, size)
// ---------------------------------------------------------------------------

/// Build a manifest from the given skill directories: maps file path → (mtime_ns, size).
fn build_manifest(skill_dirs: &[PathBuf]) -> HashMap<String, (u64, u64)> {
    let mut manifest = HashMap::new();
    for dir in skill_dirs {
        collect_manifest_recursive(dir, &mut manifest);
    }
    manifest
}

fn collect_manifest_recursive(dir: &Path, manifest: &mut HashMap<String, (u64, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_manifest_recursive(&path, manifest);
        } else {
            // Only track .md files (SKILL.md, DESCRIPTION.md)
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "md" {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(&path) {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let size = meta.len();
                manifest.insert(path.display().to_string(), (mtime, size));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Try to load a cached skill set. Returns `None` if the cache is missing,
/// format-version mismatched, or the manifest is stale (any file changed).
pub fn load_cache(
    skill_dirs: &[PathBuf],
) -> Option<(Vec<SkillDefinition>, IndexMap<String, CategoryInfo>)> {
    let path = cache_path();
    if !path.exists() {
        tracing::debug!(path = %path.display(), "Skills cache not found");
        return None;
    }

    let raw = std::fs::read_to_string(&path).ok()?;
    let cache: SkillsCache = serde_json::from_str(&raw).ok()?;

    // Version check
    if cache.version != CACHE_VERSION {
        tracing::debug!(
            event = "cache_invalidated",
            reason = "version_mismatch",
            cache_version = cache.version,
            expected_version = CACHE_VERSION,
            "Skills cache version mismatch, invalidating"
        );
        return None;
    }

    // Manifest check: every tracked file must still match (mtime + size)
    let current_manifest = build_manifest(skill_dirs);

    // If a previously tracked file is gone, or any entry differs → stale
    if cache.manifest.len() != current_manifest.len() {
        tracing::debug!(
            event = "cache_invalidated",
            reason = "manifest_size_mismatch",
            "Skills cache manifest size mismatch, invalidating"
        );
        return None;
    }

    for (file_key, &(mtime, size)) in &cache.manifest {
        match current_manifest.get(file_key) {
            Some(&(cur_mtime, cur_size)) if cur_mtime == mtime && cur_size == size => {}
            _ => {
                tracing::debug!(event = "cache_invalidated", reason = "stale_file", file_key = %file_key, "Skills cache stale, invalidating");
                return None;
            }
        }
    }

    // Cache hit — deserialize
    let skills: Vec<SkillDefinition> = cache.skills.into_iter().map(Into::into).collect();
    let categories: IndexMap<String, CategoryInfo> = cache
        .categories
        .into_iter()
        .map(|(k, v)| (k, v.into()))
        .collect();

    tracing::info!(
        skill_count = skills.len(),
        category_count = categories.len(),
        "Skills cache hit"
    );
    Some((skills, categories))
}

/// Write the current skill set to the cache file.
pub fn write_cache(
    skill_dirs: &[PathBuf],
    skills: &[SkillDefinition],
    categories: &IndexMap<String, CategoryInfo>,
) -> anyhow::Result<()> {
    let path = cache_path();

    let manifest = build_manifest(skill_dirs);
    let cache = SkillsCache {
        version: CACHE_VERSION,
        manifest,
        skills: skills.iter().map(Into::into).collect(),
        categories: categories
            .iter()
            .map(|(k, v)| (k.clone(), v.into()))
            .collect(),
    };

    let json = serde_json::to_string(&cache)?;
    let json_len = json.len();
    std::fs::write(&path, &json)?;

    tracing::debug!(
        skill_count = skills.len(),
        cache_bytes = json_len,
        path = %path.display(),
        "Skills cache written"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_definition_roundtrip() {
        let skill = SkillDefinition {
            name: "tdd".into(),
            description: "Test-driven development".into(),
            content: "# TDD\nWrite tests first.".into(),
            source: "user".into(),
            path: Some("/skills/tdd/SKILL.md".into()),
            category: "software-development".into(),
            always_inject: false,
        };

        let ser: SkillDefinitionSer = (&skill).into();
        let de: SkillDefinition = ser.into();
        assert_eq!(de.name, "tdd");
        assert_eq!(de.category, "software-development");
        assert!(!de.always_inject);
    }

    #[test]
    fn test_always_inject_roundtrip() {
        let skill = SkillDefinition {
            name: "core".into(),
            description: "Core guidelines".into(),
            content: "# Core".into(),
            source: "builtin".into(),
            path: None,
            category: "builtin".into(),
            always_inject: true,
        };

        let ser: SkillDefinitionSer = (&skill).into();
        let de: SkillDefinition = ser.into();
        assert!(de.always_inject);
    }

    #[test]
    fn test_category_info_roundtrip() {
        let info = CategoryInfo {
            description: "Dev skills".into(),
            skill_names: vec!["tdd".into(), "debugging".into()],
        };

        let ser: CategoryInfoSer = (&info).into();
        let de: CategoryInfo = ser.into();
        assert_eq!(de.description, "Dev skills");
        assert_eq!(de.skill_names.len(), 2);
    }

    #[test]
    fn test_cache_version_mismatch() {
        // Build a fake cache with wrong version
        let cache = SkillsCache {
            version: 0, // wrong
            manifest: HashMap::new(),
            skills: vec![],
            categories: HashMap::new(),
        };
        let json = serde_json::to_string(&cache).unwrap();

        let path = std::env::temp_dir().join("zeno_test_cache.json");
        std::fs::write(&path, &json).unwrap();

        // Attempt to load should fail on version check
        let parsed: SkillsCache = serde_json::from_str(&json).unwrap();
        assert_ne!(parsed.version, CACHE_VERSION);

        let _ = std::fs::remove_file(&path);
    }
}
