//! Permission overlay component — manages permission requests and ask_user prompts.
//!
//! Handles a queue of pending permission/ask_user requests, showing one at a time.
//! Supports "allow all" mode for auto-approving permission requests.
//!
//! Implements the `Component` trait so it can participate in the component tree.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ratatui::{Frame, layout::Rect};
use tokio::sync::oneshot;

use crate::gateway::UiCommand;
use crate::ui::component::Component;

/// Request type: Permission (tool permission) vs AskUser (user question).
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionType {
    /// Tool permission request (supports allow_all).
    Permission,
    /// User question request (must be manually answered).
    AskUser,
}

/// A queued permission/ask_user request (used for both active and queued entries).
#[derive(Debug)]
#[allow(dead_code)]
pub struct ActivePermission {
    pub prompt_type: InteractionType,
    /// Tool name (for permission requests) or empty (for ask_user).
    pub tool_name: String,
    /// Reason for the permission request or context for the question.
    pub reason: String,
    /// Detail/input data shown to the user for context.
    pub detail: String,
    pub response_tx: Arc<Mutex<Option<oneshot::Sender<String>>>>,
}

/// Type alias for queued entries (semantic alias, structurally identical).
#[allow(dead_code)]
pub type PendingPermission = ActivePermission;

/// Result of enqueuing a new prompt request.
pub enum PromptResult {
    /// Auto-approved (permission_allow_all was set).
    AutoApproved,
    /// Queued behind an existing active prompt.
    Queued,
    /// Shown to user (set as active).
    Shown,
}

/// Result of responding to a prompt.
pub enum QueueAction {
    /// All requests processed.
    Drained,
    /// Next request is now shown.
    ShownNext,
    /// "all" mode: all queued requests auto-approved.
    AllApproved,
    /// "all" mode but active request is AskUser (still needs manual response).
    AskUserActive,
}

/// Permission overlay state.
///
/// Manages a queue of pending permission/ask_user requests.
/// When multiple requests arrive concurrently, they are queued
/// and processed one at a time after the user responds.
pub struct PermissionOverlay {
    /// Currently active (displayed) request.
    active: Option<ActivePermission>,
    /// Queue of pending requests.
    queue: VecDeque<ActivePermission>,
    /// Whether "allow all" is enabled (auto-approve Permission requests).
    allow_all: bool,
    /// Whether the overlay needs re-rendering.
    render_dirty: bool,
}

impl PermissionOverlay {
    pub fn new() -> Self {
        Self {
            active: None,
            queue: VecDeque::new(),
            allow_all: false,
            render_dirty: true,
        }
    }

    /// Whether the active request is an AskUser type.
    pub fn is_ask_user_active(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| matches!(a.prompt_type, InteractionType::AskUser))
    }

    /// Enqueue a new permission request.
    /// Returns `PromptResult::AutoApproved` if allow_all is set for Permission types.
    pub fn enqueue_permission(
        &mut self,
        tool_name: String,
        reason: String,
        detail: String,
        response_tx: Arc<Mutex<Option<oneshot::Sender<String>>>>,
    ) -> PromptResult {
        self.enqueue(
            InteractionType::Permission,
            tool_name,
            reason,
            detail,
            response_tx,
        )
    }

    /// Enqueue a new ask_user request.
    pub fn enqueue_ask_user(
        &mut self,
        question: String,
        response_tx: Arc<Mutex<Option<oneshot::Sender<String>>>>,
    ) -> PromptResult {
        self.enqueue(
            InteractionType::AskUser,
            String::new(),
            String::new(),
            question,
            response_tx,
        )
    }

    /// Internal enqueue method.
    fn enqueue(
        &mut self,
        prompt_type: InteractionType,
        tool_name: String,
        reason: String,
        detail: String,
        response_tx: Arc<Mutex<Option<oneshot::Sender<String>>>>,
    ) -> PromptResult {
        // AskUser is never auto-approved
        if matches!(prompt_type, InteractionType::Permission) && self.allow_all {
            if let Some(tx) = response_tx.lock().unwrap().take() {
                let _ = tx.send("y".into());
            }
            return PromptResult::AutoApproved;
        }

        let entry = ActivePermission {
            prompt_type,
            tool_name,
            reason,
            detail,
            response_tx,
        };

        if self.active.is_some() {
            self.queue.push_back(entry);
            self.render_dirty = true;
            PromptResult::Queued
        } else {
            self.active = Some(entry);
            self.render_dirty = true;
            PromptResult::Shown
        }
    }

    /// Respond to the active request.
    /// Returns a QueueAction indicating what happened.
    pub fn respond(&mut self, text: &str) -> QueueAction {
        let lower = text.trim().to_lowercase();
        if matches!(lower.as_str(), "a" | "all" | "always") {
            // "all" mode: only for Permission type
            self.allow_all = true;
            // Auto-approve all queued Permission requests
            while let Some(next) = self.queue.pop_front() {
                if matches!(next.prompt_type, InteractionType::Permission) {
                    if let Some(tx) = next.response_tx.lock().unwrap().take() {
                        let _ = tx.send("y".into());
                    }
                } else {
                    // AskUser: put back at front
                    self.queue.push_front(next);
                    break;
                }
            }
            // If active is AskUser, don't close — still needs manual response
            if self
                .active
                .as_ref()
                .is_some_and(|a| matches!(a.prompt_type, InteractionType::AskUser))
            {
                return QueueAction::AskUserActive;
            }
            self.active = None;
            self.render_dirty = true;
            return QueueAction::AllApproved;
        }

        // Send response for active request
        if let Some(active) = self.active.take()
            && let Some(tx) = active.response_tx.lock().unwrap().take()
        {
            let _ = tx.send(text.to_string());
        }

        // Promote next queued request
        if let Some(next) = self.queue.pop_front() {
            self.active = Some(next);
            self.render_dirty = true;
            return QueueAction::ShownNext;
        }

        self.render_dirty = true;
        QueueAction::Drained
    }

    /// Clear all requests (e.g. on interrupt).
    pub fn clear(&mut self) {
        self.active = None;
        self.queue.clear();
        self.render_dirty = true;
    }
}

impl Component for PermissionOverlay {
    /// Process a UiCommand by taking ownership of it.
    fn update(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::ShowPermission {
                tool_name,
                reason,
                detail,
                response_tx,
            } => {
                // Take the actual oneshot sender from the Arc<Mutex<Option>> wrapper
                let tx = {
                    let mut guard = response_tx.lock().unwrap();
                    guard.take()
                };
                if let Some(tx) = tx {
                    let _ = self.enqueue_permission(
                        tool_name,
                        reason,
                        detail,
                        Arc::new(Mutex::new(Some(tx))),
                    );
                }
            }
            UiCommand::ShowAskUser {
                question,
                response_tx,
            } => {
                let tx = {
                    let mut guard = response_tx.lock().unwrap();
                    guard.take()
                };
                if let Some(tx) = tx {
                    let _ = self.enqueue_ask_user(question, Arc::new(Mutex::new(Some(tx))));
                }
            }
            UiCommand::HideOverlay => {
                self.clear();
            }
            _ => {}
        }
    }

    /// PermissionOverlay has no direct visual rendering — its state is displayed
    /// via `OutputSegment::PermissionPrompt` in the output area.
    fn view(&mut self, _area: Rect, _frame: &mut Frame) {}

    fn needs_render(&self) -> bool {
        self.render_dirty
    }

    fn clear_dirty(&mut self) {
        self.render_dirty = false;
    }
}

impl Default for PermissionOverlay {
    fn default() -> Self {
        Self::new()
    }
}
