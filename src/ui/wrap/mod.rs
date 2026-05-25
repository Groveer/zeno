//! Text wrapping utilities for terminal display.
//!
//! Three layers:
//! - [`prefix::take_prefix_by_width`] — raw width-based split (private)
//! - [`word::word_wrap`] — plain-text word-wrap with URL protection
//! - [`span::wrap_spans`] — style-preserving wrap for ratatui [`Span`]s

pub mod span;
pub mod word;

mod prefix;

pub use word::word_wrap;
