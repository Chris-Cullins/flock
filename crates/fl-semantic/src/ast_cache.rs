//! Persistent AST cache: stores parsed symbol information to disk
//! so that `fl diff --semantic` doesn't re-parse unchanged files.
//!
//! Cache entries are stored at `.flock/semantic-cache/<hash_prefix>/<hash>.json`
//! where hash is the BLAKE3 hash of the file contents.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::SymbolInfo;

/// A cached set of symbols extracted from a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSymbols {
    /// BLAKE3 hash of the file contents.
    pub content_hash: String,
    /// Language used for parsing.
    pub language: String,
    /// Extracted symbols.
    pub symbols: Vec<SymbolInfo>,
}

/// Persistent cache for parsed AST symbols.
pub struct PersistentASTCache {
    cache_dir: PathBuf,
}

impl PersistentASTCache {
    pub fn new(root: &Path) -> Self {
        Self {
            cache_dir: root.join(".flock/semantic-cache"),
        }
    }

    /// Look up cached symbols for a file by its content hash.
    pub fn get(&self, content_hash: &str) -> Option<CachedSymbols> {
        let path = self.cache_path(content_hash);
        if !path.exists() {
            return None;
        }
        let json = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&json).ok()
    }

    /// Store symbols for a file by its content hash.
    pub fn put(&self, entry: &CachedSymbols) -> Result<()> {
        let path = self.cache_path(&entry.content_hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create cache directory {}", parent.display()))?;
        }
        let json = serde_json::to_string(entry)
            .context("failed to serialize cached symbols")?;
        fs::write(&path, json)
            .with_context(|| format!("failed to write cache entry {}", path.display()))
    }

    /// Check if a cache entry exists for a given content hash.
    pub fn has(&self, content_hash: &str) -> bool {
        self.cache_path(content_hash).exists()
    }

    /// Clear all cached entries.
    pub fn clear(&self) -> Result<()> {
        if self.cache_dir.exists() {
            fs::remove_dir_all(&self.cache_dir)
                .with_context(|| "failed to clear semantic cache")?;
        }
        Ok(())
    }

    fn cache_path(&self, content_hash: &str) -> PathBuf {
        let prefix = if content_hash.len() >= 2 {
            &content_hash[..2]
        } else {
            content_hash
        };
        self.cache_dir.join(prefix).join(format!("{}.json", content_hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_cached_symbols(hash: &str) -> CachedSymbols {
        CachedSymbols {
            content_hash: hash.to_string(),
            language: "typescript".to_string(),
            symbols: Vec::new(), // Empty is fine for testing the cache layer
        }
    }

    #[test]
    fn put_and_get_round_trips() {
        let dir = tempdir().unwrap();
        let cache = PersistentASTCache::new(dir.path());

        let entry = make_cached_symbols("abcdef1234567890");
        cache.put(&entry).unwrap();

        let loaded = cache.get("abcdef1234567890").unwrap();
        assert_eq!(loaded.content_hash, "abcdef1234567890");
        assert_eq!(loaded.language, "typescript");
    }

    #[test]
    fn get_returns_none_for_missing() {
        let dir = tempdir().unwrap();
        let cache = PersistentASTCache::new(dir.path());
        assert!(cache.get("nonexistent").is_none());
    }

    #[test]
    fn has_checks_existence() {
        let dir = tempdir().unwrap();
        let cache = PersistentASTCache::new(dir.path());

        assert!(!cache.has("abc123"));
        cache.put(&make_cached_symbols("abc123")).unwrap();
        assert!(cache.has("abc123"));
    }

    #[test]
    fn clear_removes_all() {
        let dir = tempdir().unwrap();
        let cache = PersistentASTCache::new(dir.path());

        cache.put(&make_cached_symbols("aaa")).unwrap();
        cache.put(&make_cached_symbols("bbb")).unwrap();
        assert!(cache.has("aaa"));

        cache.clear().unwrap();
        assert!(!cache.has("aaa"));
        assert!(!cache.has("bbb"));
    }
}
