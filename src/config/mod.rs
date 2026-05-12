pub mod loader;
pub mod model_context;
pub mod paths;
pub mod settings;
pub mod watcher;

// Convenience re-export: use crate::config::load()
pub use loader::load;
