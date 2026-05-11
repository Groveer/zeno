//! Skill write-origin provenance — distinguishes background-review skill writes
//! from foreground user-directed writes.
//!
//! The curator only consolidates/prunes skills autonomously created via the
//! background self-improvement review fork. Skills a user asks the foreground
//! agent to write belong to the user and must never be auto-curated.
//!
//! Instead of thread-local storage (which breaks with tokio::spawn), the write
//! origin is carried through `SubAgentDeps.write_origin` → `ToolContext` and
//! read by `skill_manage` at execution time.

/// The sentinel value the background review fork uses.
pub const BACKGROUND_REVIEW: &str = "background_review";
