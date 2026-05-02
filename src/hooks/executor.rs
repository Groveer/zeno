#![allow(dead_code)]

use std::collections::HashMap;

use super::types::*;

pub struct HookExecutor {
    hooks: HashMap<HookEvent, Vec<HookCallback>>,
}

impl HookExecutor {
    pub fn new() -> Self {
        Self {
            hooks: HashMap::new(),
        }
    }

    pub fn register(&mut self, event: HookEvent, callback: HookCallback) {
        self.hooks.entry(event).or_default().push(callback);
    }

    pub async fn execute(&self, event: HookEvent, context: &mut HookContext) -> PreToolUseResult {
        if let Some(callbacks) = self.hooks.get(&event) {
            for cb in callbacks {
                match cb(&event, context).await {
                    HookResult::Continue => continue,
                    HookResult::Block { reason } => {
                        return PreToolUseResult {
                            blocked: true,
                            reason: Some(reason),
                        };
                    }
                }
            }
        }
        PreToolUseResult {
            blocked: false,
            reason: None,
        }
    }
}

impl Default for HookExecutor {
    fn default() -> Self {
        Self::new()
    }
}
