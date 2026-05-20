//! Background skill review — runs after every N turns to capture learnings.
//!
//! After each user interaction, if the turn count exceeds the review interval,
//! a background sub-agent is spawned to review the conversation and update
//! the skill library. This runs asynchronously and never blocks the user.
//!
//! Design reference: hermes-agent `run_agent.py::_spawn_background_review()`

use crate::config::settings::SkillsConfig;
use crate::engine::curator::{BackgroundWorkGuard, try_lock_background_work};
use crate::engine::sub_agent::run_delegated_task;
use crate::tools::base::SubAgentDeps;
use tokio_util::sync::CancellationToken;

/// The review prompt given to the background sub-agent.
/// Adapted from hermes-agent's `_SKILL_REVIEW_PROMPT`.
const SKILL_REVIEW_PROMPT: &str = r#"Review the conversation above and update the skill library. Be ACTIVE — most sessions produce at least one skill update, even if small. A pass that does nothing is a missed learning opportunity, not a neutral outcome.

Target shape of the library: CLASS-LEVEL skills, each with a rich SKILL.md and a references/ directory for session-specific detail. Not a long flat list of narrow one-session-one-skill entries.

Signals to look for (any one of these warrants action):
  • User corrected your style, tone, format, legibility, or verbosity.
  • User corrected your workflow, approach, or sequence of steps.
  • Non-trivial technique, fix, workaround, debugging path, or tool-usage pattern emerged.
  • A skill that got loaded or consulted this session turned out to be wrong, missing a step, or outdated.

Preference order:
  1. UPDATE A CURRENTLY-LOADED SKILL via skill_manage(action="patch").
  2. UPDATE AN EXISTING UMBRELLA via skill_list + skill_view + skill_manage(action="patch").
  3. ADD A SUPPORT FILE under an existing umbrella via skill_manage(action="write_file").
  4. CREATE A NEW CLASS-LEVEL UMBRELLA SKILL via skill_manage(action="create").

Do NOT capture:
  • Environment-dependent failures (missing binaries, fresh-install errors).
  • Negative claims about tools or features ("X tool is broken").
  • Session-specific transient errors that resolved before the conversation ended.
  • One-off task narratives.

If nothing to save, just say "Nothing to save." and stop."#;

/// Check if a background review should run based on the config and turn count.
pub fn should_run_review(turn_count: u32, config: &SkillsConfig) -> bool {
    if !config.background_review_enabled {
        return false;
    }
    if config.review_interval_turns == 0 {
        return false;
    }
    // Run when turn_count is a multiple of review_interval_turns
    // and turn_count > 0 (skip the first turn)
    turn_count > 0 && turn_count.is_multiple_of(config.review_interval_turns)
}

/// Spawn a background review sub-agent.
///
/// This runs asynchronously and never blocks the caller. The review agent
/// gets access to skill_view, skill_list, and skill_manage tools so it can
/// inspect and update the skill library.
///
/// Returns immediately. The review runs in a background tokio task.
pub fn spawn_background_review(
    deps: SubAgentDeps,
    cwd: std::path::PathBuf,
    conversation_summary: String,
    parent_cancel: Option<CancellationToken>,
) {
    let goal = format!(
        "{}\n\n## Conversation Summary\n\n{}",
        SKILL_REVIEW_PROMPT, conversation_summary
    );

    tokio::spawn(async move {
        // Acquire background work lock — prevents concurrent review + curator runs
        if !try_lock_background_work() {
            tracing::debug!("Background review skipped: background work already in progress");
            return;
        }
        let _guard = BackgroundWorkGuard;

        // Build a progress channel (dropped — we don't display background progress)
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Allow skill_view, skill_list, skill_manage, read, grep, glob
        let extra_tools = vec![
            "skill_view".to_string(),
            "skill_list".to_string(),
            "skill_manage".to_string(),
            "read".to_string(),
            "grep".to_string(),
            "glob".to_string(),
        ];

        let cancel = if let Some(ref pc) = parent_cancel {
            let child = CancellationToken::new();
            let child_link = child.clone();
            let pc = pc.clone();
            tokio::spawn(async move {
                pc.cancelled().await;
                child_link.cancel();
            });
            child
        } else {
            CancellationToken::new()
        };

        // Run the review with a timeout and cancellation support
        let result = if let Some(ref pc) = parent_cancel {
            tokio::select! {
                result = run_delegated_task(
                    &deps, cwd, goal, None, extra_tools, cancel, tx,
                ) => Some(result),
                _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
                    tracing::warn!("Background skill review timed out after 300s");
                    None
                }
                _ = pc.cancelled() => {
                    tracing::info!("Background skill review cancelled by shutdown");
                    None
                }
            }
        } else {
            tokio::select! {
                result = run_delegated_task(
                    &deps, cwd, goal, None, extra_tools, cancel, tx,
                ) => Some(result),
                _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
                    tracing::warn!("Background skill review timed out after 300s");
                    None
                }
            }
        };

        if let Some(result) = result {
            if let Some(ref err) = result.error {
                tracing::error!(
                    exit_reason = %result.exit_reason,
                    error = %err,
                    api_calls = result.api_calls,
                    duration_secs = result.duration_seconds,
                    "Background skill review failed"
                );
            } else if result.summary.is_empty() {
                tracing::info!(
                    exit_reason = %result.exit_reason,
                    "Background skill review completed with no changes"
                );
            } else {
                tracing::info!(
                    summary_len = result.summary.len(),
                    api_calls = result.api_calls,
                    duration_secs = result.duration_seconds,
                    "Background skill review completed"
                );
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::SkillsConfig;

    #[test]
    fn test_should_run_review_disabled() {
        let config = SkillsConfig {
            background_review_enabled: false,
            review_interval_turns: 5,
            ..Default::default()
        };
        assert!(!should_run_review(5, &config));
    }

    #[test]
    fn test_should_run_review_interval_zero() {
        let config = SkillsConfig {
            background_review_enabled: true,
            review_interval_turns: 0,
            ..Default::default()
        };
        assert!(!should_run_review(5, &config));
    }

    #[test]
    fn test_should_run_review_turn_zero() {
        let config = SkillsConfig {
            background_review_enabled: true,
            review_interval_turns: 5,
            ..Default::default()
        };
        assert!(!should_run_review(0, &config));
    }

    #[test]
    fn test_should_run_review_exact_multiple() {
        let config = SkillsConfig {
            background_review_enabled: true,
            review_interval_turns: 5,
            ..Default::default()
        };
        assert!(should_run_review(5, &config));
        assert!(should_run_review(10, &config));
        assert!(should_run_review(15, &config));
    }

    #[test]
    fn test_should_run_review_not_multiple() {
        let config = SkillsConfig {
            background_review_enabled: true,
            review_interval_turns: 5,
            ..Default::default()
        };
        assert!(!should_run_review(3, &config));
        assert!(!should_run_review(7, &config));
        assert!(!should_run_review(12, &config));
    }
}
