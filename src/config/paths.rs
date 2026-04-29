use std::path::PathBuf;

/// Returns the rcode config directory following XDG spec.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("rcode")
}

/// Returns the path to the main config file.
pub fn config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

/// Returns the data directory for rcode (memory, sessions, etc.).
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("rcode")
}

/// Ensures the config directory exists, returns its path.
pub fn ensure_config_dir() -> anyhow::Result<PathBuf> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
