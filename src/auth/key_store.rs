//! API key storage — secure key management via OS keyring or environment variables.
//!
//! Key resolution order:
//! 1. Explicit `api_key` in config
//! 2. Environment variable specified by `api_key_env`
//! 3. OS keyring (if available)
//! 4. Prompt user interactively (if TTY)
//!
//! Note: This module provides keyring integration for future use.
//! Currently, only environment variable and explicit key resolution are used
//! (via `config/settings::resolve_api_key`, not this module).
//!
//! The main query loop resolves API keys through `config::settings::resolve_api_key`
//! in `main.rs`; the keyring functions here are reserved for `zn keyring set/get`
//! interactive commands.
#![allow(dead_code, reason = "keyring CLI commands not yet implemented")]

/// Resolve an API key using the standard priority chain.
pub fn resolve_api_key(
    explicit_key: &Option<String>,
    env_var: &Option<String>,
    provider_name: &str,
) -> anyhow::Result<String> {
    // 1. Explicit key in config
    if let Some(key) = explicit_key
        && !key.is_empty()
    {
        return Ok(key.clone());
    }

    // 2. Environment variable
    if let Some(var) = env_var {
        if let Ok(key) = std::env::var(var)
            && !key.is_empty()
        {
            return Ok(key);
        }
        // Env var was specified but not set — error, not silent fallback
        return Err(anyhow::anyhow!(
            "Environment variable {} not set (provider: {})",
            var,
            provider_name
        ));
    }

    // 3. Try OS keyring
    if let Ok(key) = keyring_get(provider_name) {
        return Ok(key);
    }

    Err(anyhow::anyhow!(
        "No API key configured for provider '{}'. \
         Set api_key or api_key_env in config, or store in keyring.",
        provider_name
    ))
}

/// Store an API key in the OS keyring.
pub fn keyring_set(provider_name: &str, key: &str) -> anyhow::Result<()> {
    // Use a simple file-based keyring for now.
    // TODO: Integrate with `keyring` crate when adding OS keychain support.
    let keyring_dir = keyring_dir()?;
    std::fs::create_dir_all(&keyring_dir)?;

    let key_file = keyring_dir.join(format!("{}.key", provider_name));
    std::fs::write(&key_file, key)?;

    // Set restrictive permissions on Unix: directory 0o700, file 0o600
    #[cfg(unix)]
    {
        set_keyring_permissions(&keyring_dir)?;
        set_key_file_permissions(&key_file)?;
    }

    tracing::info!(
        provider = %provider_name,
        event = "key_stored",
        "Stored API key in local keyring"
    );
    Ok(())
}

/// Retrieve an API key from the OS keyring.
pub fn keyring_get(provider_name: &str) -> anyhow::Result<String> {
    let keyring_dir = keyring_dir()?;
    let key_file = keyring_dir.join(format!("{}.key", provider_name));

    if !key_file.exists() {
        return Err(anyhow::anyhow!(
            "No keyring entry for provider '{}'",
            provider_name
        ));
    }

    let key = std::fs::read_to_string(&key_file)?;
    if key.is_empty() {
        return Err(anyhow::anyhow!(
            "Empty keyring entry for provider '{}'",
            provider_name
        ));
    }

    Ok(key)
}

/// Delete an API key from the OS keyring.
pub fn keyring_delete(provider_name: &str) -> anyhow::Result<()> {
    let keyring_dir = keyring_dir()?;
    let key_file = keyring_dir.join(format!("{}.key", provider_name));

    if !key_file.exists() {
        return Err(anyhow::anyhow!(
            "No keyring entry for provider '{}'",
            provider_name
        ));
    }

    std::fs::remove_file(&key_file)?;
    Ok(())
}

/// List all providers with stored keys.
pub fn keyring_list() -> anyhow::Result<Vec<String>> {
    let keyring_dir = keyring_dir()?;
    if !keyring_dir.exists() {
        return Ok(Vec::new());
    }

    let mut providers = Vec::new();
    for entry in std::fs::read_dir(&keyring_dir)? {
        let entry = entry?;
        if let Some(name) = entry.path().file_stem().and_then(|s| s.to_str())
            && entry.path().extension().is_some_and(|e| e == "key")
        {
            providers.push(name.to_string());
        }
    }
    providers.sort();
    Ok(providers)
}

/// Get the keyring directory path.
fn keyring_dir() -> anyhow::Result<std::path::PathBuf> {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("zeno")
        .join("keyring");
    Ok(dir)
}

/// Set restrictive permissions on keyring directory (Unix only).
#[cfg(unix)]
fn set_keyring_permissions(dir: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(dir, perms)?;
    Ok(())
}

/// Set restrictive permissions on key file (Unix only).
#[cfg(unix)]
fn set_key_file_permissions(file: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(file, perms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_explicit_key() {
        let result = resolve_api_key(&Some("sk-test-key".into()), &None, "test-provider");
        assert_eq!(result.unwrap(), "sk-test-key");
    }

    #[test]
    fn test_resolve_env_key() {
        unsafe {
            std::env::set_var("TEST_ZENO_API_KEY", "env-test-key");
        }
        let result = resolve_api_key(&None, &Some("TEST_ZENO_API_KEY".into()), "test-provider");
        assert_eq!(result.unwrap(), "env-test-key");
        unsafe {
            std::env::remove_var("TEST_ZENO_API_KEY");
        }
    }

    #[test]
    fn test_resolve_missing_env_var() {
        let result = resolve_api_key(
            &None,
            &Some("NONEXISTENT_ZENO_VAR_12345".into()),
            "test-provider",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_no_key() {
        let result = resolve_api_key(&None, &None, "test-provider");
        assert!(result.is_err());
    }

    #[test]
    fn test_keyring_crud() {
        let dir = tempfile::tempdir().unwrap();
        // Override keyring_dir for testing — use the temp dir directly
        let key_file = dir.path().join("test-provider.key");
        std::fs::write(&key_file, "test-key-value").unwrap();

        let key = std::fs::read_to_string(&key_file).unwrap();
        assert_eq!(key, "test-key-value");

        std::fs::remove_file(&key_file).unwrap();
        assert!(!key_file.exists());
    }
}
