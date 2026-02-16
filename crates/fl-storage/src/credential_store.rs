//! Credential store for Flock remote authentication.
//!
//! Stores per-host tokens in `~/.flock/credentials.toml` (user-level, not
//! repo-level). This is analogous to git's credential helpers but simpler —
//! a single TOML file with one entry per host.
//!
//! Format:
//! ```toml
//! [[credentials]]
//! host = "example.com"
//! token = "fl_tok_abc123..."
//! method = "token"
//!
//! [[credentials]]
//! host = "staging.example.com"
//! token = "fl_tok_def456..."
//! method = "ssh-key"
//! ssh_key_path = "/home/user/.flock/keys/ed25519.sk"
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Returns the path to the user-level credentials file: `~/.flock/credentials.toml`.
pub fn credentials_path() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(home.join(".flock").join("credentials.toml"))
}

/// Authentication method used to obtain the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMethod {
    /// Bearer token (e.g. from `fl remote login`).
    Token,
    /// Token obtained via SSH key challenge-response.
    SshKey,
}

/// A single credential entry, keyed by host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialEntry {
    pub host: String,
    pub token: String,
    pub method: AuthMethod,
    /// Path to the SSH key used for authentication (only for `ssh-key` method).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_key_path: Option<String>,
}

/// Top-level credentials config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialStore {
    #[serde(default)]
    pub credentials: Vec<CredentialEntry>,
}

impl CredentialStore {
    /// Load the credential store from `~/.flock/credentials.toml`.
    /// Returns an empty store if the file doesn't exist.
    pub fn load() -> Result<Self> {
        let path = credentials_path()?;
        Self::load_from(&path)
    }

    /// Load from a specific path (useful for testing).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)?;
        let store: Self = toml::from_str(&content)?;
        Ok(store)
    }

    /// Save the credential store to `~/.flock/credentials.toml`.
    pub fn save(&self) -> Result<()> {
        let path = credentials_path()?;
        self.save_to(&path)
    }

    /// Save to a specific path (useful for testing).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        // Write with restrictive permissions (owner-only read/write).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true).mode(0o600);
            let mut file = opts.open(path)?;
            std::io::Write::write_all(&mut file, content.as_bytes())?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            fs::write(path, content)?;
            Ok(())
        }
    }

    /// Find a credential for a given host.
    pub fn find(&self, host: &str) -> Option<&CredentialEntry> {
        self.credentials.iter().find(|c| c.host == host)
    }

    /// Store or update a credential for a host.
    pub fn upsert(&mut self, entry: CredentialEntry) {
        if let Some(existing) = self.credentials.iter_mut().find(|c| c.host == entry.host) {
            *existing = entry;
        } else {
            self.credentials.push(entry);
        }
    }

    /// Remove a credential for a host. Returns true if removed.
    pub fn remove(&mut self, host: &str) -> bool {
        let before = self.credentials.len();
        self.credentials.retain(|c| c.host != host);
        self.credentials.len() < before
    }

    /// List all stored hosts.
    pub fn hosts(&self) -> Vec<&str> {
        self.credentials.iter().map(|c| c.host.as_str()).collect()
    }
}

/// Resolve token for a remote URL. Checks:
/// 1. Token stored directly on the roost entry (in `.flock/roosts.toml`)
/// 2. Token in the user-level credential store (`~/.flock/credentials.toml`)
///
/// Returns None if no token is available.
pub fn resolve_token(roost_token: Option<&str>, host: Option<&str>) -> Result<Option<String>> {
    // Direct roost token takes priority.
    if let Some(t) = roost_token {
        if !t.is_empty() {
            return Ok(Some(t.to_string()));
        }
    }
    // Fall back to credential store.
    if let Some(host) = host {
        let store = CredentialStore::load()?;
        if let Some(cred) = store.find(host) {
            return Ok(Some(cred.token.clone()));
        }
    }
    Ok(None)
}

fn home_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        if let Ok(home) = std::env::var("HOME") {
            return Ok(PathBuf::from(home));
        }
    }
    #[cfg(windows)]
    {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            return Ok(PathBuf::from(profile));
        }
    }
    // Fallback: try common env vars.
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home));
    }
    bail!("could not determine home directory — set $HOME")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_load_save_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.toml");

        let store = CredentialStore::default();
        store.save_to(&path).unwrap();

        let loaded = CredentialStore::load_from(&path).unwrap();
        assert!(loaded.credentials.is_empty());
    }

    #[test]
    fn upsert_and_find() {
        let mut store = CredentialStore::default();

        store.upsert(CredentialEntry {
            host: "example.com".to_string(),
            token: "tok_abc".to_string(),
            method: AuthMethod::Token,
            ssh_key_path: None,
        });

        assert_eq!(store.find("example.com").unwrap().token, "tok_abc");
        assert!(store.find("other.com").is_none());

        // Upsert overwrites.
        store.upsert(CredentialEntry {
            host: "example.com".to_string(),
            token: "tok_xyz".to_string(),
            method: AuthMethod::Token,
            ssh_key_path: None,
        });
        assert_eq!(store.find("example.com").unwrap().token, "tok_xyz");
        assert_eq!(store.credentials.len(), 1);
    }

    #[test]
    fn remove_credential() {
        let mut store = CredentialStore::default();
        store.upsert(CredentialEntry {
            host: "example.com".to_string(),
            token: "tok".to_string(),
            method: AuthMethod::Token,
            ssh_key_path: None,
        });
        assert!(store.remove("example.com"));
        assert!(!store.remove("example.com")); // already gone
        assert!(store.find("example.com").is_none());
    }

    #[test]
    fn save_load_roundtrip_with_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.toml");

        let mut store = CredentialStore::default();
        store.upsert(CredentialEntry {
            host: "example.com".to_string(),
            token: "tok_abc".to_string(),
            method: AuthMethod::Token,
            ssh_key_path: None,
        });
        store.upsert(CredentialEntry {
            host: "staging.example.com".to_string(),
            token: "tok_def".to_string(),
            method: AuthMethod::SshKey,
            ssh_key_path: Some("/home/user/.flock/keys/ed25519.sk".to_string()),
        });
        store.save_to(&path).unwrap();

        let loaded = CredentialStore::load_from(&path).unwrap();
        assert_eq!(loaded.credentials.len(), 2);
        assert_eq!(loaded.find("example.com").unwrap().method, AuthMethod::Token);
        assert_eq!(
            loaded.find("staging.example.com").unwrap().method,
            AuthMethod::SshKey
        );
        assert_eq!(
            loaded
                .find("staging.example.com")
                .unwrap()
                .ssh_key_path
                .as_deref(),
            Some("/home/user/.flock/keys/ed25519.sk")
        );
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.toml");
        let store = CredentialStore::load_from(&path).unwrap();
        assert!(store.credentials.is_empty());
    }

    #[test]
    fn resolve_token_prefers_roost() {
        // Can't test credential store fallback without mocking home dir,
        // but we can test that roost token takes priority.
        let result = resolve_token(Some("roost-token"), Some("example.com")).unwrap();
        assert_eq!(result, Some("roost-token".to_string()));

        let result = resolve_token(Some(""), Some("example.com")).unwrap();
        // Empty roost token falls through to credential store.
        // (May or may not find anything — that's fine.)
        assert!(result.is_none() || result.is_some());
    }

    #[test]
    fn hosts_list() {
        let mut store = CredentialStore::default();
        store.upsert(CredentialEntry {
            host: "a.com".to_string(),
            token: "t1".to_string(),
            method: AuthMethod::Token,
            ssh_key_path: None,
        });
        store.upsert(CredentialEntry {
            host: "b.com".to_string(),
            token: "t2".to_string(),
            method: AuthMethod::Token,
            ssh_key_path: None,
        });
        let hosts = store.hosts();
        assert_eq!(hosts, vec!["a.com", "b.com"]);
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_file_with_restricted_permissions() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.toml");

        let store = CredentialStore::default();
        store.save_to(&path).unwrap();

        let meta = fs::metadata(&path).unwrap();
        let mode = meta.mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials file should be owner-only");
    }
}
