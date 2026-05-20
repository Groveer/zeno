//! Gateway method handlers.
//!
//! Each handler processes a specific type of user input or command.
//!
//! - `slash` — slash commands (/help, /model, /cost, etc.) dispatched via `Gateway::dispatch_slash()`
//! - `session` — session lifecycle (create, list) via `MethodHandler` trait
//! - `config` — configuration queries (get, set) via `MethodHandler` trait
//! - `prompt` — prompt submission via `MethodHandler` trait

pub mod config;
pub mod prompt;
pub mod session;
pub mod slash;
