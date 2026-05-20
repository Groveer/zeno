//! Skill usage telemetry + provenance tracking.
//!
//! Tracks per-skill usage metadata in a sidecar JSON file
//! (`~/.config/zeno/skills/.usage.json`) keyed by skill name.
//! Counters are bumped by skill_view and skill_manage; the curator
//! reads derived activity timestamps to decide lifecycle transitions.
//!
//! Design notes:
//! - Sidecar, not frontmatter. Keeps operational telemetry out of
//!   user-authored SKILL.md content.
//! - Atomic writes via tempfile + rename (same pattern as session saves).
//! - All counter bumps are best-effort: failures log at DEBUG and return
//!   silently. A broken sidecar never breaks the underlying tool call.
//!
//! Lifecycle states:
//!   active   -> default
//!   stale    -> unused > stale_after_days (config)
//!   archived -> unused > archive_after_days (config); moved to .archive/

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::config::paths::config_dir;
use crate::utils::time;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const USAGE_FILE: &str = ".usage.json";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Lifecycle state for a skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum SkillState {
    #[default]
    Active,
    Stale,
    Archived,
}

/// Usage record for a single skill.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageRecord {
    /// Who created this skill: "agent" (background review) or None (user/foreground).
    #[serde(default)]
    pub created_by: Option<String>,
    /// How many times the skill was used (loaded into context).
    #[serde(default)]
    pub use_count: u64,
    /// How many times the skill was viewed (skill_view).
    #[serde(default)]
    pub view_count: u64,
    /// When the skill was last actively used.
    #[serde(default)]
    pub last_used_at: Option<String>,
    /// When the skill was last viewed.
    #[serde(default)]
    pub last_viewed_at: Option<String>,
    /// How many times the skill was patched.
    #[serde(default)]
    pub patch_count: u64,
    /// When the skill was last patched.
    #[serde(default)]
    pub last_patched_at: Option<String>,
    /// When the record was created.
    #[serde(default)]
    pub created_at: String,
    /// Current lifecycle state.
    #[serde(default)]
    pub state: SkillState,
    /// If true, skip all auto-transitions (stale/archive/delete).
    #[serde(default)]
    pub pinned: bool,
    /// When the skill was archived (if applicable).
    #[serde(default)]
    pub archived_at: Option<String>,
}

impl UsageRecord {
    fn new() -> Self {
        Self {
            created_by: None,
            use_count: 0,
            view_count: 0,
            last_used_at: None,
            last_viewed_at: None,
            patch_count: 0,
            last_patched_at: None,
            created_at: time::now_iso(),
            state: SkillState::Active,
            pinned: false,
            archived_at: None,
        }
    }
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

fn usage_file_path() -> PathBuf {
    config_dir().join("skills").join(USAGE_FILE)
}

fn archive_dir() -> PathBuf {
    config_dir().join("skills").join(".archive")
}

/// Load all usage records from the sidecar file.
pub fn load_all() -> HashMap<String, UsageRecord> {
    let path = usage_file_path();
    if !path.exists() {
        return HashMap::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(data) => data,
            Err(e) => {
                tracing::debug!(error = %e, path = %path.display(), "Failed to parse usage file");
                HashMap::new()
            }
        },
        Err(e) => {
            tracing::debug!(error = %e, path = %path.display(), "Failed to read usage file");
            HashMap::new()
        }
    }
}

/// Save all usage records atomically.
fn save_all(data: &HashMap<String, UsageRecord>) {
    let path = usage_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Atomic write: tempfile + rename
    let tmp_path = path.with_extension("usage.tmp");
    match serde_json::to_string_pretty(data) {
        Ok(json) => {
            if std::fs::write(&tmp_path, &json).is_ok() {
                std::fs::rename(&tmp_path, &path).ok();
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "Failed to serialize usage data");
        }
    }
}

/// Get a usage record for a skill, creating a default if missing.
pub fn get_record(skill_name: &str) -> UsageRecord {
    let data = load_all();
    data.get(skill_name)
        .cloned()
        .unwrap_or_else(UsageRecord::new)
}

/// Mutate a usage record in-place.
fn mutate<F>(skill_name: &str, f: F)
where
    F: FnOnce(&mut UsageRecord),
{
    if skill_name.is_empty() {
        return;
    }
    let mut data = load_all();
    let mut record = data.remove(skill_name).unwrap_or_default();
    f(&mut record);
    data.insert(skill_name.to_string(), record);
    save_all(&data);
}

// ---------------------------------------------------------------------------
// Public counter-bump helpers
// ---------------------------------------------------------------------------

/// Bump view_count and last_viewed_at. Called from skill_view.
pub fn bump_view(skill_name: &str) {
    mutate(skill_name, |rec| {
        rec.view_count = rec.view_count.saturating_add(1);
        rec.last_viewed_at = Some(time::now_iso());
    });
}

/// Bump use_count and last_used_at. Called when a skill is actively used.
pub fn bump_use(skill_name: &str) {
    mutate(skill_name, |rec| {
        rec.use_count = rec.use_count.saturating_add(1);
        rec.last_used_at = Some(time::now_iso());
    });
}

/// Bump patch_count and last_patched_at. Called from skill_manage (patch/edit).
pub fn bump_patch(skill_name: &str) {
    mutate(skill_name, |rec| {
        rec.patch_count = rec.patch_count.saturating_add(1);
        rec.last_patched_at = Some(time::now_iso());
    });
}

/// Mark a skill as agent-created (curator-managed).
pub fn mark_agent_created(skill_name: &str) {
    mutate(skill_name, |rec| {
        rec.created_by = Some("agent".into());
    });
}

/// Set lifecycle state.
pub fn set_state(skill_name: &str, state: SkillState) {
    mutate(skill_name, |rec| {
        rec.state = state.clone();
        if state == SkillState::Archived {
            rec.archived_at = Some(time::now_iso());
        } else {
            rec.archived_at = None;
        }
    });
}

/// Set pinned status.
pub fn set_pinned(skill_name: &str, pinned: bool) {
    mutate(skill_name, |rec| {
        rec.pinned = pinned;
    });
}

/// Remove a usage entry entirely (when a skill is deleted).
pub fn forget(skill_name: &str) {
    if skill_name.is_empty() {
        return;
    }
    let mut data = load_all();
    data.remove(skill_name);
    save_all(&data);
}

// ---------------------------------------------------------------------------
// Lifecycle helpers
// ---------------------------------------------------------------------------

/// Return the latest activity timestamp for a record (excluding creation time).
pub fn latest_activity_at(record: &UsageRecord) -> Option<&str> {
    let candidates = [
        record.last_used_at.as_deref(),
        record.last_viewed_at.as_deref(),
        record.last_patched_at.as_deref(),
    ];
    candidates.into_iter().flatten().max()
}

/// Return total activity count (use + view + patch).
pub fn activity_count(record: &UsageRecord) -> u64 {
    record.use_count + record.view_count + record.patch_count
}

/// Determine if a skill should be transitioned to stale.
pub fn should_be_stale(record: &UsageRecord, stale_after_days: u64) -> bool {
    if record.pinned || record.state == SkillState::Archived {
        return false;
    }
    if record.state == SkillState::Stale {
        return false; // already stale
    }
    days_since_last_activity(record).is_some_and(|days| days >= stale_after_days)
}

/// Determine if a skill should be transitioned to archived.
pub fn should_be_archived(record: &UsageRecord, archive_after_days: u64) -> bool {
    if record.pinned || record.state == SkillState::Archived {
        return false;
    }
    days_since_last_activity(record).is_some_and(|days| days >= archive_after_days)
}

/// Days since the last activity on a skill. Returns None if never active.
fn days_since_last_activity(record: &UsageRecord) -> Option<u64> {
    let activity = latest_activity_at(record)?;
    let last = time::parse_iso_to_systemtime(activity)?;
    let now = SystemTime::now();
    let duration = now.duration_since(last).ok()?;
    Some(duration.as_secs() / 86400)
}

// ---------------------------------------------------------------------------
// Archive / restore
// ---------------------------------------------------------------------------

/// Move a skill directory to the .archive/ directory.
/// Returns (success, message).
pub fn archive_skill(skill_name: &str, skill_dir: &std::path::Path) -> (bool, String) {
    if !skill_dir.exists() {
        return (false, format!("Skill '{}' directory not found", skill_name));
    }

    let archive = archive_dir();
    if let Err(e) = std::fs::create_dir_all(&archive) {
        return (false, format!("Failed to create archive directory: {}", e));
    }

    // Compute destination — append timestamp if name collision
    let dest = archive.join(skill_name);
    let dest = if dest.exists() {
        let ts = time::now_iso().replace(':', "-");
        archive.join(format!("{}_{}", skill_name, ts))
    } else {
        dest
    };

    match std::fs::rename(skill_dir, &dest) {
        Ok(_) => {
            set_state(skill_name, SkillState::Archived);
            (true, format!("Archived to {}", dest.display()))
        }
        Err(e) => (false, format!("Failed to archive: {}", e)),
    }
}

/// Restore a skill from the .archive/ directory.
pub fn restore_skill(skill_name: &str, target_dir: &std::path::Path) -> (bool, String) {
    let archive = archive_dir().join(skill_name);
    if !archive.exists() {
        return (false, format!("Archived skill '{}' not found", skill_name));
    }

    if target_dir.exists() {
        return (
            false,
            format!("Target directory already exists: {}", target_dir.display()),
        );
    }

    match std::fs::rename(&archive, target_dir) {
        Ok(_) => {
            set_state(skill_name, SkillState::Active);
            (true, format!("Restored to {}", target_dir.display()))
        }
        Err(e) => (false, format!("Failed to restore: {}", e)),
    }
}

/// List archived skill names.
pub fn list_archived() -> Vec<String> {
    let archive = archive_dir();
    if !archive.exists() {
        return Vec::new();
    }
    let mut names: Vec<String> = std::fs::read_dir(&archive)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn test_roundtrip() {
        let mut data: HashMap<String, UsageRecord> = HashMap::new();
        let rec = UsageRecord {
            created_by: Some("agent".into()),
            use_count: 5,
            view_count: 10,
            patch_count: 2,
            last_used_at: Some("2025-07-23T10:30:00Z".into()),
            ..UsageRecord::new()
        };
        data.insert("test-skill".into(), rec);

        let json = serde_json::to_string_pretty(&data).unwrap();
        let restored: HashMap<String, UsageRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored["test-skill"].use_count, 5);
        assert_eq!(restored["test-skill"].created_by.as_deref(), Some("agent"));
    }

    #[test]
    fn test_parse_iso_roundtrip() {
        let ts = "2025-07-23T10:30:00Z";
        let st = time::parse_iso_to_systemtime(ts).unwrap();
        let duration = st.duration_since(UNIX_EPOCH).unwrap();
        let days = duration.as_secs() / 86400;
        let (y, m, d) = time::days_to_date(days);
        assert_eq!(y, 2025);
        assert_eq!(m, 7);
        assert_eq!(d, 23);
    }

    #[test]
    fn test_mutate_creates_record() {
        // Use a temp dir by manipulating the path via config_dir override
        // Since we can't override config_dir in test easily, just test logic
        let rec = UsageRecord::new();
        assert_eq!(rec.use_count, 0);
        assert_eq!(rec.state, SkillState::Active);
        assert!(!rec.pinned);
    }

    #[test]
    fn test_latest_activity() {
        let mut rec = UsageRecord::new();
        assert!(latest_activity_at(&rec).is_none());

        rec.last_viewed_at = Some("2025-07-20T10:00:00Z".into());
        rec.last_used_at = Some("2025-07-23T10:00:00Z".into());
        assert_eq!(latest_activity_at(&rec).unwrap(), "2025-07-23T10:00:00Z");
    }

    #[test]
    fn test_should_be_stale() {
        let mut rec = UsageRecord::new();
        rec.last_used_at = Some("2025-06-01T10:00:00Z".into());
        // 52+ days ago from now (assuming now is July 23+)
        // Set the test time manually by using a low threshold
        assert!(should_be_stale(&rec, 1)); // 1 day threshold should be stale
        assert!(!should_be_stale(&rec, 365)); // 365 day threshold should not be stale
    }

    #[test]
    fn test_pinned_blocks_transitions() {
        let mut rec = UsageRecord::new();
        rec.last_used_at = Some("2020-01-01T10:00:00Z".into());
        rec.pinned = true;
        assert!(!should_be_stale(&rec, 1));
        assert!(!should_be_archived(&rec, 1));
    }

    #[test]
    fn test_days_to_date() {
        // 2025-07-23 in days since epoch
        let days = time::date_to_days(2025, 7, 23);
        let (y, m, d) = time::days_to_date(days);
        assert_eq!(y, 2025);
        assert_eq!(m, 7);
        assert_eq!(d, 23);
    }
}
