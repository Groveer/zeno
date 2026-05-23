//! ACP (Agent Client Protocol) server implementation.
//!
//! Allows external ACP-compatible clients (IDEs, editors) to connect to
//! zeno via stdio transport. The server handles:
//!
//! - `initialize` — protocol version negotiation
//! - `session/new` — create a new conversation session
//! - `session/prompt` — process user prompts with streaming updates
//! - `session/cancel` — cancel ongoing operations
//!
//! ## Usage
//!
//! ```bash
//! zeno --acp
//! ```
//!
//! The ACP server reads JSON-RPC 2.0 messages from stdin and writes
//! responses/notifications to stdout, following the ACP specification.

pub mod server;
