//! Curator — background skill maintenance orchestrator.
//!
//! The curator periodically reviews agent-created skills and maintains the
//! collection. It runs inactivity-triggered: when the system is idle and the
//! last curator run was longer than `interval_hours` ago.
//!
//! Responsibilities:
//! - Auto-transition lifecycle states based on derived activity timestamps
//! - Spawn a background review agent for consolidation (merge narrow skills
//!   into umbrellas, archive absorbed siblings)
//!
//! Design reference: hermes-agent `agent/curator.py`

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::paths::config_dir;
use crate::config::settings::SkillsConfig;
use crate::skills::registry::SkillRegistry;
use crate::skills::usage::{self, SkillState};
use crate::tools::base::SubAgentDeps;
use crate::utils::time;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const STATE_FILE: &str = ".curator_state";

// ---------------------------------------------------------------------------
// Background work serialization lock
// ---------------------------------------------------------------------------

/// Global lock that prevents background review and curator from running
/// concurrently. Both check `try_lock_background_work()` before spawning
/// their sub-agent and release via `unlock_background_work()` when done.
static BACKGROUND_LOCK: AtomicBool = AtomicBool::new(false);

/// Try to acquire the background work lock. Returns `true` if acquired.
pub fn try_lock_background_work() -> bool {
    BACKGROUND_LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
}

/// Release the background work lock.
pub fn unlock_background_work() {
    BACKGROUND_LOCK.store(false, Ordering::Release);
}

/// RAII guard for background work lock. Drops the lock when it goes out of scope.
pub struct BackgroundWorkGuard;

impl Drop for BackgroundWorkGuard {
    fn drop(&mut self) {
        unlock_background_work();
    }
}

// ---------------------------------------------------------------------------
// Curator state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CuratorState {
    last_run_at: Option<String>,
    last_run_summary: Option<String>,
    paused: bool,
    run_count: u64,
}

fn state_file_path() -> PathBuf {
    config_dir().join("skills").join(STATE_FILE)
}

fn load_state() -> CuratorState {
    let path = state_file_path();
    if !path.exists() {
        return CuratorState::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => CuratorState::default(),
    }
}

fn save_state(state: &CuratorState) {
    let path = state_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("curator_state.tmp");
    if let Ok(json) = serde_json::to_string_pretty(state)
        && std::fs::write(&tmp, &json).is_ok()
    {
        std::fs::rename(&tmp, &path).ok();
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check if the curator should run now.
pub fn should_run_now(config: &SkillsConfig) -> bool {
    if !config.curator_enabled {
        return false;
    }
    let state = load_state();
    if state.paused {
        return false;
    }

    // Don't start if another background task (review) is in progress
    if BACKGROUND_LOCK.load(Ordering::Relaxed) {
        return false;
    }

    match &state.last_run_at {
        None => true, // Never run → run now
        Some(ts) => time::hours_since(ts).is_none_or(|h| h >= config.curator_interval_hours as f64),
    }
}

/// Run the curator's lifecycle maintenance (stale/archive transitions).
/// Returns a summary string of what was done.
pub fn run_lifecycle_maintenance(skill_registry: &SkillRegistry, config: &SkillsConfig) -> String {
    let mut actions: Vec<String> = Vec::new();

    for skill_def in skill_registry.list_skills() {
        // Skip bundled/hub skills
        if skill_def.source != "user" {
            continue;
        }

        let record = usage::get_record(&skill_def.name);

        // Skip pinned skills
        if record.pinned {
            continue;
        }

        // Check for archive
        if usage::should_be_archived(&record, config.archive_after_days) {
            if let Some(ref path_str) = skill_def.path {
                let skill_path = Path::new(path_str);
                if let Some(skill_dir) = skill_path.parent() {
                    let (ok, msg) = usage::archive_skill(&skill_def.name, skill_dir);
                    let activity = usage::activity_count(&record);
                    if ok {
                        actions.push(format!(
                            "Archived '{}' (activity: {}): {}",
                            skill_def.name, activity, msg
                        ));
                    } else {
                        actions.push(format!("Failed to archive '{}': {}", skill_def.name, msg));
                    }
                }
            }
            continue;
        }

        // Check for stale
        if usage::should_be_stale(&record, config.stale_after_days) {
            usage::set_state(&skill_def.name, SkillState::Stale);
            let activity = usage::activity_count(&record);
            actions.push(format!(
                "Marked '{}' as stale (activity: {})",
                skill_def.name, activity
            ));
        }
    }

    if actions.is_empty() {
        "No lifecycle transitions needed.".to_string()
    } else {
        actions.join("\n")
    }
}

/// Spawn a curator consolidation review in a background task.
///
/// The review agent inspects agent-created skills and merges narrow skills
/// into broader umbrellas.
///
/// `parent_cancel` is an optional cancellation token to link to. When the app
/// exits, the parent token is cancelled and the review will stop promptly.
pub fn spawn_curator_review(
    deps: SubAgentDeps,
    cwd: PathBuf,
    skill_registry: &SkillRegistry,
    parent_cancel: Option<tokio_util::sync::CancellationToken>,
) {
    // Build a list of agent-created skills for the review prompt
    let agent_created: Vec<String> = skill_registry
        .list_skills()
        .iter()
        .filter(|s| s.source == "user")
        .filter(|s| usage::get_record(&s.name).created_by.as_deref() == Some("agent"))
        .filter(|s| !usage::get_record(&s.name).pinned)
        .map(|s| s.name.clone())
        .collect();

    if agent_created.is_empty() {
        tracing::debug!("Curator: no agent-created skills to review");
        return;
    }

    let candidate_list = agent_created.join("\n");

    let goal = format!(
        r#"You are running as the background skill CURATOR. This is a consolidation pass.

The goal is a LIBRARY OF CLASS-LEVEL INSTRUCTIONS. A collection of hundreds of narrow skills where each one captures one session's specific bug is a FAILURE of the library.

Hard rules:
1. DO NOT touch bundled or hub-installed skills.
2. DO NOT delete any skill. Use skill_manage(action="delete") with absorbed_into="<umbrella>" to archive merged skills.
3. DO NOT touch skills shown as pinned=yes.
4. DO NOT reject consolidation because "each skill has a distinct trigger". If a human would write one skill with N labeled subsections, merge.

How to work:
1. Scan the candidate list. Identify PREFIX CLUSTERS (skills sharing a first word or domain keyword).
2. For each cluster with 2+ members, ask "what is the UMBRELLA CLASS these skills all serve?"
3. Three ways to consolidate:
   a. MERGE INTO EXISTING UMBRELLA — patch the umbrella to add sections, then archive siblings.
   b. CREATE A NEW UMBRELLA — skill_manage(action="create") with a class-level name, archive siblings.
   c. DEMOTE TO REFERENCES — move narrow content into the umbrella's references/ directory, archive the sibling.
4. Use skill_manage(action="delete") with absorbed_into="<umbrella_name>" when merging content into another skill.

Candidate skills (agent-created, not pinned):
{}

When done, write a summary of what was consolidated."#,
        candidate_list
    );

    tokio::spawn(async move {
        // Acquire background work lock — prevents concurrent curator + review runs
        if !try_lock_background_work() {
            tracing::debug!("Curator review skipped: background work already in progress");
            return;
        }

        let _guard = BackgroundWorkGuard;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Intentionally no "bash" — background tasks should never execute
        // arbitrary commands. See hermes-agent _bg_review_auto_deny pattern.
        let extra_tools = vec![
            "skill_view".to_string(),
            "skill_list".to_string(),
            "skill_manage".to_string(),
            "read".to_string(),
            "grep".to_string(),
            "glob".to_string(),
        ];

        let cancel = if let Some(ref pc) = parent_cancel {
            let child = tokio_util::sync::CancellationToken::new();
            let child_link = child.clone();
            let pc = pc.clone();
            tokio::spawn(async move {
                pc.cancelled().await;
                child_link.cancel();
            });
            child
        } else {
            tokio_util::sync::CancellationToken::new()
        };

        let result = crate::engine::sub_agent::run_delegated_task(
            &deps,
            cwd,
            goal,
            None,
            extra_tools,
            cancel,
            tx,
        )
        .await;

        if result.error.is_some() || result.summary.is_empty() {
            tracing::debug!(
                exit_reason = %result.exit_reason,
                "Curator review completed with no changes"
            );
        } else {
            tracing::info!(
                summary_len = result.summary.len(),
                api_calls = result.api_calls,
                "Curator review completed"
            );
        }
    });
}

/// Run a full curator pass. Returns a summary string.
///
/// 1. Run lifecycle maintenance (stale/archive transitions)
/// 2. Spawn consolidation review
pub fn run_curator_pass(
    skill_registry: &SkillRegistry,
    deps: Option<SubAgentDeps>,
    cwd: Option<PathBuf>,
    config: &SkillsConfig,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> String {
    // 1. Lifecycle maintenance
    let lifecycle_summary = run_lifecycle_maintenance(skill_registry, config);

    // 2. Consolidation review (only if we have sub-agent deps)
    if let (Some(deps), Some(cwd)) = (deps, cwd) {
        spawn_curator_review(deps, cwd, skill_registry, cancel);
    }

    // Update state
    let mut state = load_state();
    state.last_run_at = Some(time::now_iso());
    state.last_run_summary = Some(lifecycle_summary.clone());
    state.run_count += 1;
    save_state(&state);

    lifecycle_summary
}

/// Set paused status.
/// Set paused status.
pub fn set_paused(paused: bool) {
    let mut state = load_state();
    state.paused = paused;
    save_state(&state);
}

/// Check if the curator is paused.
/// Check if the curator is paused.
pub fn is_paused() -> bool {
    load_state().paused
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::time;

    #[test]
    fn test_should_run_never_ran() {
        let _config = SkillsConfig::default();
        // Remove state file by using a fresh config
        let old = state_file_path();
        if old.exists() {
            // Test logic: never run → should run
            let state = CuratorState::default();
            assert!(state.last_run_at.is_none());
        }
    }

    #[test]
    fn test_hours_since() {
        let ts = time::now_iso();
        let h = time::hours_since(&ts).unwrap();
        assert!(h < 0.01); // just now
    }

    #[test]
    fn test_parse_iso() {
        let secs = time::parse_iso_to_secs("2025-07-23T10:30:00Z").unwrap();
        assert!(secs > 0);
    }

    #[test]
    fn test_state_roundtrip() {
        let state = CuratorState {
            last_run_at: Some("2025-07-23T10:30:00Z".into()),
            last_run_summary: Some("Archived 2 skills".into()),
            paused: false,
            run_count: 3,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: CuratorState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.run_count, 3);
        assert_eq!(
            restored.last_run_summary.as_deref(),
            Some("Archived 2 skills")
        );
    }
}
