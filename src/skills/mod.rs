//! Skills module — progressive disclosure skill system.
//!
//! Three-tier architecture:
//! - Tier 0: Category index (injected into system prompt)
//! - Tier 1: Skill summaries (via `skill_list` tool)
//! - Tier 2: Full content (via `skill_view` tool)

pub mod builtin;
pub mod index_cache;
pub mod loader;
pub mod registry;
pub mod types;
