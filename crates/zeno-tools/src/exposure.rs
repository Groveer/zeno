//! Tool exposure levels — control which tools are visible to the model.
//!
//! Design inspired by Codex's `ToolExposure` enum which provides
//! fine-grained visibility control (Direct, Deferred, DirectModelOnly, Hidden).

use serde::{Deserialize, Serialize};

/// Visibility level controlling how a tool is exposed to the LLM.
///
/// The model sees only tools with `Explicit`, `Direct`, or `Suggested` exposure.
/// `Deferred` tools are registered but not shown initially — the model can
/// discover them at runtime via the `tool_search` mechanism.
/// `Hidden` tools are callable only by the system or hooks.
///
/// ## Mapping from Codex design
///
/// | Codex | Zeno | Note |
/// |-------|------|------|
/// | `Direct` | `Explicit` / `Direct` | Both mean "always in the tool list" |
/// | `Deferred` | `Deferred` | Registered but not shown; discoverable via `tool_search` |
/// | `DirectModelOnly` | (future) | Tool shown to model but excluded from nested code-mode |
/// | `Hidden` | `Hidden` | Registered but invisible to model |
/// | — | `Suggested` | Included in compact summary |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExposure {
    /// Always included in the tool list sent to the LLM.
    /// This is the default for all interactive tools.
    Explicit,

    /// Semantic alias for `Explicit`. Same behavior.
    Direct,

    /// Registered but excluded from the initial tool list.
    ///
    /// The model can discover these tools at runtime via the `tool_search`
    /// mechanism, reducing token waste from rarely-used tool schemas.
    ///
    /// Typical use: niche tools, infrequent operations, or tools with very
    /// large schemas that should only be loaded on demand.
    Deferred,

    /// Included in a compact "suggested tools" section; the model can
    /// request the full spec if needed.
    Suggested,

    /// Not visible to the model at all. Only invocable by the system,
    /// hooks, or internal orchestration (e.g. `internal_log`).
    Hidden,
}

impl Default for ToolExposure {
    fn default() -> Self {
        Self::Explicit
    }
}

impl ToolExposure {
    /// Whether this tool should be included in the model's initial tool list.
    ///
    /// Returns `true` for `Explicit`, `Direct`, and `Suggested`.
    /// Returns `false` for `Deferred` and `Hidden`.
    pub fn is_visible_to_model(&self) -> bool {
        matches!(self, Self::Explicit | Self::Direct | Self::Suggested)
    }

    /// Whether this tool is discoverable at runtime (via `tool_search`).
    ///
    /// Returns `true` for `Deferred`, `Explicit`, `Direct`, and `Suggested`.
    /// Returns `false` only for `Hidden` (system-only tooling).
    pub fn is_discoverable(&self) -> bool {
        !matches!(self, Self::Hidden)
    }
}
