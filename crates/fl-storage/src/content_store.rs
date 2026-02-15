//! Block-level content-addressed storage using BLAKE3 hashes.
//!
//! Blocks are stored at `.flock/store/blocks/<hex-hash>` where the hash is
//! the BLAKE3 digest of the block contents. This provides automatic
//! deduplication: identical content across files or versions is stored once.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::layout;

/// A content-addressed block store backed by the filesystem.
///
/// Blocks are keyed by their BLAKE3 hash and stored as flat files under
/// `.flock/store/blocks/`. The first two hex characters of the hash are
/// used as a subdirectory for fan-out (e.g. `ab/cdef01...`).
pub struct ContentStore {
    blocks_dir: PathBuf,
}

impl ContentStore {
    /// Create a store rooted at the given repository root.
    pub fn for_root(root: &Path) -> Self {
        Self {
            blocks_dir: root.join(layout::STORE_BLOCKS_DIR),
        }
    }

    /// Ensure the store directories exist.
    pub fn ensure_exists(&self) -> Result<()> {
        fs::create_dir_all(&self.blocks_dir)
            .with_context(|| format!("failed to create block store at {}", self.blocks_dir.display()))
    }

    /// Store a block and return its BLAKE3 hash.
    ///
    /// If a block with the same hash already exists, this is a no-op
    /// (content-addressed deduplication).
    pub fn put(&self, data: &[u8]) -> Result<String> {
        let hash = blake3::hash(data);
        let hex_hash = hash.to_hex().to_string();
        let path = self.block_path(&hex_hash);

        if path.exists() {
            return Ok(hex_hash);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create block directory {}", parent.display()))?;
        }

        // Write to a temp file then rename for atomicity
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, data)
            .with_context(|| format!("failed to write block {}", hex_hash))?;
        fs::rename(&tmp_path, &path)
            .with_context(|| format!("failed to commit block {}", hex_hash))?;

        Ok(hex_hash)
    }

    /// Retrieve a block by its BLAKE3 hash.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.block_path(hash);
        if !path.exists() {
            bail!("block {} not found in content store", hash);
        }
        fs::read(&path)
            .with_context(|| format!("failed to read block {}", hash))
    }

    /// Check whether a block exists.
    pub fn has(&self, hash: &str) -> bool {
        self.block_path(hash).exists()
    }

    /// Delete a block. Returns true if it existed.
    pub fn delete(&self, hash: &str) -> Result<bool> {
        let path = self.block_path(hash);
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(&path)
            .with_context(|| format!("failed to delete block {}", hash))?;
        Ok(true)
    }

    /// Return the total number of blocks in the store.
    pub fn block_count(&self) -> Result<usize> {
        if !self.blocks_dir.exists() {
            return Ok(0);
        }
        let mut count = 0;
        for shard in fs::read_dir(&self.blocks_dir)
            .with_context(|| "failed to read block store directory")?
        {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    /// Return the total size of all blocks in bytes.
    pub fn total_size(&self) -> Result<u64> {
        if !self.blocks_dir.exists() {
            return Ok(0);
        }
        let mut total = 0u64;
        for shard in fs::read_dir(&self.blocks_dir)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    total += entry.metadata()?.len();
                }
            }
        }
        Ok(total)
    }

    /// Compute the path for a given block hash using 2-char fan-out.
    fn block_path(&self, hex_hash: &str) -> PathBuf {
        if hex_hash.len() < 2 {
            return self.blocks_dir.join(hex_hash);
        }
        self.blocks_dir.join(&hex_hash[..2]).join(hex_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, ContentStore) {
        let dir = tempdir().unwrap();
        let store = ContentStore::for_root(dir.path());
        store.ensure_exists().unwrap();
        (dir, store)
    }

    #[test]
    fn put_and_get_round_trips() {
        let (_dir, store) = setup();
        let data = b"hello world";
        let hash = store.put(data).unwrap();

        assert!(!hash.is_empty());
        assert!(store.has(&hash));

        let retrieved = store.get(&hash).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn put_is_idempotent() {
        let (_dir, store) = setup();
        let data = b"duplicate content";
        let hash1 = store.put(data).unwrap();
        let hash2 = store.put(data).unwrap();
        assert_eq!(hash1, hash2);
        assert_eq!(store.block_count().unwrap(), 1);
    }

    #[test]
    fn different_data_different_hashes() {
        let (_dir, store) = setup();
        let h1 = store.put(b"alpha").unwrap();
        let h2 = store.put(b"beta").unwrap();
        assert_ne!(h1, h2);
        assert_eq!(store.block_count().unwrap(), 2);
    }

    #[test]
    fn get_nonexistent_fails() {
        let (_dir, store) = setup();
        let result = store.get("0000000000000000000000000000000000000000000000000000000000000000");
        assert!(result.is_err());
    }

    #[test]
    fn delete_existing_block() {
        let (_dir, store) = setup();
        let hash = store.put(b"to delete").unwrap();
        assert!(store.has(&hash));
        assert!(store.delete(&hash).unwrap());
        assert!(!store.has(&hash));
    }

    #[test]
    fn delete_nonexistent_returns_false() {
        let (_dir, store) = setup();
        assert!(!store.delete("nonexistent").unwrap());
    }

    #[test]
    fn total_size_matches() {
        let (_dir, store) = setup();
        store.put(b"12345").unwrap();
        store.put(b"67890ab").unwrap();
        assert_eq!(store.total_size().unwrap(), 12); // 5 + 7
    }

    #[test]
    fn empty_store_stats() {
        let (_dir, store) = setup();
        assert_eq!(store.block_count().unwrap(), 0);
        assert_eq!(store.total_size().unwrap(), 0);
    }
}
