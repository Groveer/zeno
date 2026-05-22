//! Tool trait definitions and shared types for Zeno agent tools.
//!
//! This crate defines the core abstractions that all Zeno tools implement.
//! Design inspired by Codex's `ToolExecutor` trait pattern:
//!
//! - [`Tool`] trait: the core tool interface with schema, execution, and metadata
//! - [`ToolExposure`]: visibility levels controlling which tools the model sees
//! - [`ToolOutput`]: structured output trait for consistent tool results
//!
//! # Design Principles
//!
//! - **Declarative metadata**: Tools declare their capabilities (read-only, parallel-safe)
//! - **Structured output**: Consistent output format with logging and serialization
//! - **Exposure control**: Fine-grained visibility (Hidden, Explicit, Suggested)

pub mod definition;
pub mod exposure;
pub mod output;

pub use definition::ToolDefinition;
pub use exposure::ToolExposure;
pub use output::{JsonToolOutput, ToolOutput};
