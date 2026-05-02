#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    Notification,
}

pub struct PreToolUseResult {
    pub blocked: bool,
    pub reason: Option<String>,
}

pub type HookContext = serde_json::Map<String, serde_json::Value>;

pub enum HookResult {
    Continue,
    Block { reason: String },
}

pub type HookCallback = Box<
    dyn Fn(&HookEvent, &mut HookContext) -> Pin<Box<dyn Future<Output = HookResult> + Send>>
        + Send
        + Sync,
>;
