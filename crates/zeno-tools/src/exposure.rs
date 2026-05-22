//! Tool exposure levels — control which tools are visible to the model.

use serde::{Deserialize, Serialize};

/// Visibility level controlling how a tool is exposed to the LLM.
///
/// Tools with different exposure levels are handled differently in the
/// tool schema sent to the model:
///
/// - `Explicit`: Always included in the tool list (default for most tools)
/// - `Suggested`: Included in a "suggested tools" section; model can request more
/// - `Hidden`: Not shown to the model; only invocable by the system or hooks
/// - `Disabled`: Registered but completely inactive (e.g. feature-flagged off)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExposure {
    /// Always visible to the model. Default for core tools.
    Explicit,
    /// Suggested to the model — included in a summary section.
    /// The model can request the full tool spec if needed.
    Suggested,
    /// Not visible to the model. Only invocable by the system, hooks,
    /// or other tools (e.g. a hidden `internal_log` tool).
    Hidden,
    /// Registered but completely inactive. Useful for feature flags.
    Disabled,
}

impl Default for ToolExposure {
    fn default() -> Self {
        Self::Explicit
    }
}

impl ToolExposure {
    /// Whether this tool should be included in the model's tool list.
    pub fn is_visible_to_model(&self) -> bool {
        matches!(self, Self::Explicit | Self::Suggested)
    }

    /// Whether this tool can be executed at all.
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}
