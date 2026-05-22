//! UI → Engine submission types.
//!
//! Submissions represent user-initiated actions that the engine should process.
//! Currently the main submission path is through `Gateway` input handling,
//! but defining explicit submission types enables:
//!
//! - Clean protocol boundary between UI and Engine
//! - Future support for remote/multi-client scenarios
//! - Session recording/replay (submissions can be logged and replayed)

use serde::{Deserialize, Serialize};

/// Submissions from UI to Engine.
///
/// Each variant represents a user-initiated action that triggers
/// engine behavior. This enum is designed for future extensibility
/// when the engine runs as a separate process (App-Server mode).
#[derive(Debug, Clone)]
pub enum Submission {
    /// User typed a message (main input).
    UserInput { text: String },
    /// User submitted image blocks along with text.
    UserInputWithImages {
        text: String,
        images: Vec<ImageBlock>,
    },
    /// User pressed Ctrl+C to interrupt the current query.
    Interrupt,
    /// User responded to a permission prompt.
    PermissionResponse {
        /// The response: "y" (yes), "n" (no), "a" (all/always).
        response: String,
    },
    /// User responded to an ask_user question.
    AskResponse { response: String },
    /// Steer message: user typed while the agent was running.
    /// Injected into the next turn as context.
    Steer { text: String },
    /// User wants to switch the active model.
    SwitchModel { model: String },
    /// User wants to end the current session and start fresh.
    NewSession,
    /// User wants to quit the application.
    Quit,
}

/// Image block attached to a user submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageBlock {
    pub media_type: String,
    pub base64_data: String,
}
