use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::layout;

/// Store for materialized state snapshots.
/// Stores JSON blobs at `.flock/materialized-states/<event_count>.json`.
/// The content is opaque to this store - the caller handles serialization.
pub struct MaterializedStateStore {
    dir: PathBuf,
}

impl MaterializedStateStore {
    pub fn for_root(root: &Path) -> Self {
        Self {
            dir: root.join(layout::MATERIALIZED_STATES_DIR),
        }
    }

    pub fn ensure_exists(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create materialized states directory {}", self.dir.display()))
    }

    /// Save a materialized state snapshot as JSON at the given event count.
    pub fn save(&self, event_count: usize, json: &str) -> Result<()> {
        self.ensure_exists()?;
        let path = self.state_path(event_count);
        fs::write(&path, json)
            .with_context(|| format!("failed to write materialized state {}", path.display()))
    }

    /// Load the latest materialized state, returning (event_count, json_string).
    /// Returns None if no materialized state exists.
    pub fn load_latest(&self) -> Result<Option<(usize, String)>> {
        if !self.dir.exists() {
            return Ok(None);
        }

        let mut latest: Option<usize> = None;
        for entry in fs::read_dir(&self.dir)
            .with_context(|| format!("failed to read materialized states dir {}", self.dir.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".json") {
                if let Ok(count) = stem.parse::<usize>() {
                    if latest.map_or(true, |l| count > l) {
                        latest = Some(count);
                    }
                }
            }
        }

        match latest {
            None => Ok(None),
            Some(count) => {
                let json = self.load(count)?;
                Ok(Some((count, json)))
            }
        }
    }

    /// Load a materialized state at a specific event count.
    pub fn load(&self, event_count: usize) -> Result<String> {
        let path = self.state_path(event_count);
        fs::read_to_string(&path)
            .with_context(|| format!("failed to read materialized state {}", path.display()))
    }

    /// List all materialized state event counts, sorted ascending.
    pub fn list(&self) -> Result<Vec<usize>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut counts = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".json") {
                if let Ok(count) = stem.parse::<usize>() {
                    counts.push(count);
                }
            }
        }
        counts.sort();
        Ok(counts)
    }

    fn state_path(&self, event_count: usize) -> PathBuf {
        self.dir.join(format!("{}.json", event_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn save_and_load_round_trips() {
        let dir = tempdir().unwrap();
        let store = MaterializedStateStore::for_root(dir.path());

        let json = r#"{"test": true}"#;
        store.save(100, json).unwrap();

        let loaded = store.load(100).unwrap();
        assert_eq!(loaded, json);
    }

    #[test]
    fn load_latest_returns_highest_count() {
        let dir = tempdir().unwrap();
        let store = MaterializedStateStore::for_root(dir.path());

        store.save(50, "a").unwrap();
        store.save(200, "b").unwrap();
        store.save(100, "c").unwrap();

        let (count, data) = store.load_latest().unwrap().unwrap();
        assert_eq!(count, 200);
        assert_eq!(data, "b");
    }

    #[test]
    fn load_latest_returns_none_when_empty() {
        let dir = tempdir().unwrap();
        let store = MaterializedStateStore::for_root(dir.path());
        assert!(store.load_latest().unwrap().is_none());
    }

    #[test]
    fn list_returns_sorted_counts() {
        let dir = tempdir().unwrap();
        let store = MaterializedStateStore::for_root(dir.path());

        store.save(300, "a").unwrap();
        store.save(100, "b").unwrap();
        store.save(200, "c").unwrap();

        assert_eq!(store.list().unwrap(), vec![100, 200, 300]);
    }
}
