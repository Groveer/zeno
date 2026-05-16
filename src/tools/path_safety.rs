//! Write-path safety — deny list for sensitive system/credential files.
//!
//! Prevents the LLM from overwriting critical files like SSH keys, shell
//! configs, and system files.  The deny list covers common targets; the
//! `HERMES_WRITE_SAFE_ROOT`-style env-var scoping is not yet implemented.

use std::path::Path;

/// Suffix patterns that match sensitive files.  Each entry is checked
/// as a case-insensitive suffix of the resolved path.
const DENIED_SUFFIXES: &[&str] = &[
    // SSH
    ".ssh/id_rsa",
    ".ssh/id_rsa.pub",
    ".ssh/id_ed25519",
    ".ssh/id_ed25519.pub",
    ".ssh/id_ecdsa",
    ".ssh/authorized_keys",
    ".ssh/config",
    ".ssh/known_hosts",
    // GPG
    ".gnupg/secring.gpg",
    ".gnupg/private-keys-v1.d",
    // Shell configs
    ".bashrc",
    ".bash_profile",
    ".bash_logout",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".profile",
    ".login",
    ".cshrc",
    ".tcshrc",
    ".kshrc",
    ".config/fish/config.fish",
    // Git
    ".gitconfig",
    ".git-credentials",
    // AWS
    ".aws/credentials",
    ".aws/config",
    // GCP
    ".config/gcloud/credentials.db",
    ".config/gcloud/access_tokens.db",
    // Kubernetes
    ".kube/config",
    // npm
    ".npmrc",
    // Docker
    ".docker/config.json",
    // netrc
    ".netrc",
    // macOS keychain plist
    "Library/Keychains/login.keychain-db",
];

/// Paths / prefixes that are always denied regardless of suffix match.
const DENIED_PREFIXES: &[&str] = &[
    "/etc/", "/usr/", "/bin/", "/sbin/", "/boot/", "/dev/", "/proc/", "/sys/", "/var/",
];

/// Check whether writing to `path` should be denied.
///
/// Returns `Some(reason)` if the path matches the deny list, `None` if
/// the write is allowed.
pub fn is_write_denied(path: &Path) -> Option<String> {
    let normalized = path.to_string_lossy();

    // Check prefixes first (fast path for system dirs)
    for prefix in DENIED_PREFIXES {
        if normalized.starts_with(prefix) {
            return Some(format!(
                "Write denied: '{}' is inside a protected system directory ({})",
                normalized, prefix
            ));
        }
    }

    // Check suffixes — case-insensitive on the last segment
    let lower = normalized.to_lowercase();
    for suffix in DENIED_SUFFIXES {
        if lower.ends_with(suffix) {
            return Some(format!(
                "Write denied: '{}' is a protected file ({})",
                normalized, suffix
            ));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_ssh_key() {
        let p = Path::new("/home/user/.ssh/id_rsa");
        assert!(is_write_denied(p).is_some());
    }

    #[test]
    fn denies_ssh_config() {
        let p = Path::new("/home/user/.ssh/config");
        assert!(is_write_denied(p).is_some());
    }

    #[test]
    fn denies_etc_shadow() {
        let p = Path::new("/etc/shadow");
        assert!(is_write_denied(p).is_some());
    }

    #[test]
    fn allows_project_file() {
        let p = Path::new("/home/user/projects/myapp/src/main.rs");
        assert!(is_write_denied(p).is_none());
    }

    #[test]
    fn denies_bashrc() {
        let p = Path::new("/home/user/.bashrc");
        assert!(is_write_denied(p).is_some());
    }

    #[test]
    fn denies_git_credentials() {
        let p = Path::new("/home/user/.git-credentials");
        assert!(is_write_denied(p).is_some());
    }

    #[test]
    fn allows_relative_path() {
        let p = Path::new("src/main.rs");
        assert!(is_write_denied(p).is_none());
    }

    #[test]
    fn denies_aws_credentials() {
        let p = Path::new("/home/user/.aws/credentials");
        assert!(is_write_denied(p).is_some());
    }
}
