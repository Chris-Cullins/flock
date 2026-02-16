use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::layout::FLOCK_DIR;

const ROOSTS_FILE: &str = "roosts.toml";

/// Path to `.flock/roosts.toml`.
pub fn roosts_path(root: &Path) -> PathBuf {
    root.join(FLOCK_DIR).join(ROOSTS_FILE)
}

/// Top-level roosts config stored in `.flock/roosts.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoostsConfig {
    #[serde(default)]
    pub roosts: Vec<RoostEntry>,
}

/// A single roost (remote) entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoostEntry {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_event: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Shallow clone depth (number of checkpoint events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_depth: Option<usize>,
    /// Sparse checkout glob patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sparse_patterns: Vec<String>,
    /// Whether this clone uses lazy block fetching.
    #[serde(default)]
    pub lazy: bool,
    /// Pinned glob patterns for offline access.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pin_patterns: Vec<String>,
}

/// Load roosts config from `.flock/roosts.toml`. Returns default if file
/// doesn't exist yet.
pub fn load_roosts(root: &Path) -> Result<RoostsConfig> {
    let path = roosts_path(root);
    if !path.exists() {
        return Ok(RoostsConfig::default());
    }
    let content = fs::read_to_string(&path)?;
    let config: RoostsConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Save roosts config to `.flock/roosts.toml`.
pub fn save_roosts(root: &Path, config: &RoostsConfig) -> Result<()> {
    let path = roosts_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(config)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Find a roost by name. Returns None if not found.
pub fn find_roost<'a>(config: &'a RoostsConfig, name: &str) -> Option<&'a RoostEntry> {
    config.roosts.iter().find(|r| r.name == name)
}

/// Find a roost by name (mutable). Returns None if not found.
pub fn find_roost_mut<'a>(config: &'a mut RoostsConfig, name: &str) -> Option<&'a mut RoostEntry> {
    config.roosts.iter_mut().find(|r| r.name == name)
}

/// Add a roost. Errors if the name is already taken.
pub fn add_roost(config: &mut RoostsConfig, name: &str, url: &str) -> Result<()> {
    if find_roost(config, name).is_some() {
        bail!("roost '{}' already exists", name);
    }
    config.roosts.push(RoostEntry {
        name: name.to_string(),
        url: url.to_string(),
        last_synced_event: None,
        token: None,
        clone_depth: None,
        sparse_patterns: Vec::new(),
        lazy: false,
        pin_patterns: Vec::new(),
    });
    Ok(())
}

/// Remove a roost by name. Errors if not found.
pub fn remove_roost(config: &mut RoostsConfig, name: &str) -> Result<()> {
    let before = config.roosts.len();
    config.roosts.retain(|r| r.name != name);
    if config.roosts.len() == before {
        bail!("roost '{}' not found", name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_roosts(dir.path()).unwrap();
        assert!(config.roosts.is_empty());
    }

    #[test]
    fn round_trip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        // Create .flock directory
        fs::create_dir_all(dir.path().join(FLOCK_DIR)).unwrap();

        let mut config = RoostsConfig::default();
        add_roost(&mut config, "origin", "flock://example.com/acme/repo").unwrap();
        save_roosts(dir.path(), &config).unwrap();

        let loaded = load_roosts(dir.path()).unwrap();
        assert_eq!(loaded.roosts.len(), 1);
        assert_eq!(loaded.roosts[0].name, "origin");
        assert_eq!(loaded.roosts[0].url, "flock://example.com/acme/repo");
    }

    #[test]
    fn add_duplicate_fails() {
        let mut config = RoostsConfig::default();
        add_roost(&mut config, "origin", "flock://a.com/x/y").unwrap();
        assert!(add_roost(&mut config, "origin", "flock://b.com/x/y").is_err());
    }

    #[test]
    fn remove_nonexistent_fails() {
        let mut config = RoostsConfig::default();
        assert!(remove_roost(&mut config, "nope").is_err());
    }

    #[test]
    fn find_and_mutate() {
        let mut config = RoostsConfig::default();
        add_roost(&mut config, "origin", "flock://a.com/x/y").unwrap();
        let entry = find_roost_mut(&mut config, "origin").unwrap();
        entry.url = "flock://b.com/x/y".to_string();
        assert_eq!(find_roost(&config, "origin").unwrap().url, "flock://b.com/x/y");
    }
}
