pub mod loader;
pub mod model_context;
pub mod paths;
pub mod settings;

// Convenience re-export: use crate::config::load()
pub use loader::load;
