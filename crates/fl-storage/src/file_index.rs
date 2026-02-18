//! File index mapping `(path, snapshot_id) -> Vec<BlockRef>`.
//!
//! Each snapshot has an index file stored at `.flock/store/index/<snapshot_id>.json`
//! that records, for every file in the snapshot, the ordered list of content blocks
//! (by BLAKE3 hash) that compose that file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::layout;

/// A reference to a content block within a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRef {
    /// BLAKE3 hash of the block content.
    pub hash: String,
    /// Byte offset of this block within the original file.
    pub offset: usize,
    /// Length of this block in bytes.
    pub length: usize,
}

/// Index entry for a single file within a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Ordered list of blocks that compose this file.
    pub blocks: Vec<BlockRef>,
    /// Total file size in bytes.
    pub size: u64,
    /// BLAKE3 hash of the complete file contents.
    pub file_hash: String,
}

/// The complete index for a snapshot: maps relative paths to their block composition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotIndex {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// The snapshot this index belongs to.
    pub snapshot_id: Uuid,
    /// Map of relative file paths to their block decomposition.
    pub files: BTreeMap<String, FileEntry>,
}

pub const CURRENT_INDEX_SCHEMA_VERSION: u32 = 1;

impl SnapshotIndex {
    pub fn new(snapshot_id: Uuid) -> Self {
        Self {
            schema_version: CURRENT_INDEX_SCHEMA_VERSION,
            snapshot_id,
            files: BTreeMap::new(),
        }
    }

    /// Insert an entry for a file path.
    pub fn insert(&mut self, rel_path: String, entry: FileEntry) {
        self.files.insert(rel_path, entry);
    }

    /// Look up the block composition of a file.
    pub fn get(&self, rel_path: &str) -> Option<&FileEntry> {
        self.files.get(rel_path)
    }

    /// List all file paths in this snapshot.
    pub fn paths(&self) -> Vec<&str> {
        self.files.keys().map(|s| s.as_str()).collect()
    }

    /// Total number of files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Total number of unique blocks across all files.
    pub fn unique_block_count(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for entry in self.files.values() {
            for block in &entry.blocks {
                seen.insert(block.hash.as_str());
            }
        }
        seen.len()
    }
}

/// Manages snapshot index files on disk.
pub struct FileIndex {
    index_dir: PathBuf,
}

impl FileIndex {
    pub fn for_root(root: &Path) -> Self {
        Self {
            index_dir: root.join(layout::STORE_INDEX_DIR),
        }
    }

    pub fn ensure_exists(&self) -> Result<()> {
        fs::create_dir_all(&self.index_dir)
            .with_context(|| format!("failed to create index directory {}", self.index_dir.display()))
    }

    /// Save a snapshot index to disk.
    pub fn write(&self, index: &SnapshotIndex) -> Result<()> {
        let path = self.index_path(index.snapshot_id);
        let json = serde_json::to_string(index)
            .context("failed to serialize snapshot index")?;
        fs::write(&path, json)
            .with_context(|| format!("failed to write index {}", path.display()))
    }

    /// Load a snapshot index from disk.
    pub fn read(&self, snapshot_id: Uuid) -> Result<SnapshotIndex> {
        let path = self.index_path(snapshot_id);
        if !path.exists() {
            bail!("index for snapshot {} not found", snapshot_id);
        }
        let json = fs::read_to_string(&path)
            .with_context(|| format!("failed to read index {}", path.display()))?;
        let index: SnapshotIndex = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse index {}", path.display()))?;
        Ok(index)
    }

    /// Check whether an index exists for a snapshot.
    pub fn has(&self, snapshot_id: Uuid) -> bool {
        self.index_path(snapshot_id).exists()
    }

    /// Delete an index. Returns true if it existed.
    pub fn delete(&self, snapshot_id: Uuid) -> Result<bool> {
        let path = self.index_path(snapshot_id);
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(&path)
            .with_context(|| format!("failed to delete index {}", path.display()))?;
        Ok(true)
    }

    /// List all snapshot IDs that have indexes.
    pub fn list(&self) -> Result<Vec<Uuid>> {
        if !self.index_dir.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.index_dir)
            .with_context(|| "failed to read index directory")?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".json") {
                if let Ok(id) = Uuid::parse_str(stem) {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    fn index_path(&self, snapshot_id: Uuid) -> PathBuf {
        self.index_dir.join(format!("{}.json", snapshot_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, FileIndex) {
        let dir = tempdir().unwrap();
        let index = FileIndex::for_root(dir.path());
        index.ensure_exists().unwrap();
        (dir, index)
    }

    fn sample_entry() -> FileEntry {
        FileEntry {
            blocks: vec![
                BlockRef {
                    hash: "aabbccdd".to_string(),
                    offset: 0,
                    length: 1024,
                },
                BlockRef {
                    hash: "eeff0011".to_string(),
                    offset: 1024,
                    length: 512,
                },
            ],
            size: 1536,
            file_hash: "deadbeef".to_string(),
        }
    }

    #[test]
    fn write_and_read_round_trips() {
        let (_dir, file_index) = setup();
        let id = Uuid::new_v4();
        let mut index = SnapshotIndex::new(id);
        index.insert("src/main.rs".to_string(), sample_entry());

        file_index.write(&index).unwrap();

        let loaded = file_index.read(id).unwrap();
        assert_eq!(loaded.snapshot_id, id);
        assert_eq!(loaded.file_count(), 1);
        assert_eq!(loaded.get("src/main.rs"), Some(&sample_entry()));
    }

    #[test]
    fn read_nonexistent_fails() {
        let (_dir, file_index) = setup();
        let result = file_index.read(Uuid::new_v4());
        assert!(result.is_err());
    }

    #[test]
    fn has_and_delete() {
        let (_dir, file_index) = setup();
        let id = Uuid::new_v4();
        let index = SnapshotIndex::new(id);
        file_index.write(&index).unwrap();

        assert!(file_index.has(id));
        assert!(file_index.delete(id).unwrap());
        assert!(!file_index.has(id));
    }

    #[test]
    fn list_indexes() {
        let (_dir, file_index) = setup();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        file_index.write(&SnapshotIndex::new(id1)).unwrap();
        file_index.write(&SnapshotIndex::new(id2)).unwrap();

        let mut ids = file_index.list().unwrap();
        ids.sort();
        let mut expected = vec![id1, id2];
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn unique_block_count() {
        let id = Uuid::new_v4();
        let mut index = SnapshotIndex::new(id);

        // Two files sharing a block
        let entry1 = FileEntry {
            blocks: vec![
                BlockRef { hash: "aaa".to_string(), offset: 0, length: 100 },
                BlockRef { hash: "bbb".to_string(), offset: 100, length: 100 },
            ],
            size: 200,
            file_hash: "xxx".to_string(),
        };
        let entry2 = FileEntry {
            blocks: vec![
                BlockRef { hash: "aaa".to_string(), offset: 0, length: 100 }, // shared
                BlockRef { hash: "ccc".to_string(), offset: 100, length: 50 },
            ],
            size: 150,
            file_hash: "yyy".to_string(),
        };

        index.insert("file1.txt".to_string(), entry1);
        index.insert("file2.txt".to_string(), entry2);

        assert_eq!(index.unique_block_count(), 3); // aaa, bbb, ccc
    }
}
