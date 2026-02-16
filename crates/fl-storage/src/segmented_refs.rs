//! Segmented refs: one file per ref instead of single refs.json.
//!
//! Refs are stored at `.flock/refs-v2/<kind>/<name>.json` for atomic
//! per-ref writes, avoiding lock contention on a single file.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::layout;
use crate::refs::{RefKind, RepoRef};

/// Segmented ref store with one file per ref.
pub struct SegmentedRefStore {
    dir: PathBuf,
}

impl SegmentedRefStore {
    pub fn for_root(root: &Path) -> Self {
        Self {
            dir: root.join(layout::SEGMENTED_REFS_DIR),
        }
    }

    /// Check if segmented refs exist.
    pub fn exists(&self) -> bool {
        self.dir.exists()
    }

    pub fn ensure_exists(&self) -> Result<()> {
        fs::create_dir_all(self.dir.join("branch"))
            .with_context(|| "failed to create branch refs directory")?;
        fs::create_dir_all(self.dir.join("tag"))
            .with_context(|| "failed to create tag refs directory")?;
        fs::create_dir_all(self.dir.join("workspace"))
            .with_context(|| "failed to create workspace refs directory")?;
        Ok(())
    }

    /// Read all refs across all kinds.
    pub fn read_all(&self) -> Result<Vec<RepoRef>> {
        let mut refs = Vec::new();
        for kind in &[RefKind::Branch, RefKind::Tag, RefKind::Workspace] {
            let kind_dir = self.kind_dir(*kind);
            if !kind_dir.exists() {
                continue;
            }
            for entry in fs::read_dir(&kind_dir)
                .with_context(|| format!("failed to read refs directory {}", kind_dir.display()))?
            {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.ends_with(".json") {
                    continue;
                }
                let json = fs::read_to_string(entry.path())
                    .with_context(|| format!("failed to read ref {}", entry.path().display()))?;
                let repo_ref: RepoRef = serde_json::from_str(&json)
                    .with_context(|| format!("failed to parse ref {}", entry.path().display()))?;
                refs.push(repo_ref);
            }
        }
        refs.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
        Ok(refs)
    }

    /// Read a single ref.
    pub fn read(&self, kind: RefKind, name: &str) -> Result<Option<RepoRef>> {
        let path = self.ref_path(kind, name);
        if !path.exists() {
            return Ok(None);
        }
        let json = fs::read_to_string(&path)
            .with_context(|| format!("failed to read ref {}", path.display()))?;
        let repo_ref: RepoRef = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse ref {}", path.display()))?;
        Ok(Some(repo_ref))
    }

    /// Write (upsert) a single ref atomically.
    pub fn upsert(&self, reference: RepoRef) -> Result<()> {
        self.ensure_exists()?;
        self.validate_ref(&reference)?;
        let path = self.ref_path(reference.kind, &reference.name);

        // Write to temp file then rename for atomicity
        let tmp_path = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&reference)
            .context("failed to serialize ref")?;
        fs::write(&tmp_path, &json)
            .with_context(|| format!("failed to write temp ref {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &path)
            .with_context(|| format!("failed to rename temp ref to {}", path.display()))?;
        Ok(())
    }

    /// Delete a ref. Returns true if it existed.
    pub fn delete(&self, kind: RefKind, name: &str) -> Result<bool> {
        let path = self.ref_path(kind, name);
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(&path)
            .with_context(|| format!("failed to delete ref {}", path.display()))?;
        Ok(true)
    }

    fn kind_dir(&self, kind: RefKind) -> PathBuf {
        self.dir.join(match kind {
            RefKind::Branch => "branch",
            RefKind::Tag => "tag",
            RefKind::Workspace => "workspace",
        })
    }

    fn ref_path(&self, kind: RefKind, name: &str) -> PathBuf {
        self.kind_dir(kind).join(format!("{}.json", name))
    }

    fn validate_ref(&self, reference: &RepoRef) -> Result<()> {
        if reference.name.trim().is_empty() {
            bail!("ref name cannot be empty");
        }
        match reference.kind {
            RefKind::Workspace => {
                if reference.workspace.is_none() {
                    bail!("workspace ref {} is missing workspace config", reference.name);
                }
            }
            RefKind::Branch | RefKind::Tag => {
                if reference.workspace.is_some() {
                    bail!("non-workspace ref {} has workspace config", reference.name);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::WorkspaceRefConfig;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[test]
    fn upsert_and_read_all() {
        let dir = tempdir().unwrap();
        let store = SegmentedRefStore::for_root(dir.path());
        store.ensure_exists().unwrap();

        store.upsert(RepoRef {
            kind: RefKind::Branch,
            name: "main".to_string(),
            target_event_id: Uuid::from_u128(1),
            workspace: None,
        }).unwrap();

        store.upsert(RepoRef {
            kind: RefKind::Tag,
            name: "v1.0".to_string(),
            target_event_id: Uuid::from_u128(2),
            workspace: None,
        }).unwrap();

        let refs = store.read_all().unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].kind, RefKind::Branch);
        assert_eq!(refs[1].kind, RefKind::Tag);
    }

    #[test]
    fn upsert_overwrites() {
        let dir = tempdir().unwrap();
        let store = SegmentedRefStore::for_root(dir.path());
        store.ensure_exists().unwrap();

        store.upsert(RepoRef {
            kind: RefKind::Branch,
            name: "main".to_string(),
            target_event_id: Uuid::from_u128(1),
            workspace: None,
        }).unwrap();

        store.upsert(RepoRef {
            kind: RefKind::Branch,
            name: "main".to_string(),
            target_event_id: Uuid::from_u128(2),
            workspace: None,
        }).unwrap();

        let r = store.read(RefKind::Branch, "main").unwrap().unwrap();
        assert_eq!(r.target_event_id, Uuid::from_u128(2));
    }

    #[test]
    fn delete_ref() {
        let dir = tempdir().unwrap();
        let store = SegmentedRefStore::for_root(dir.path());
        store.ensure_exists().unwrap();

        store.upsert(RepoRef {
            kind: RefKind::Tag,
            name: "v1".to_string(),
            target_event_id: Uuid::from_u128(1),
            workspace: None,
        }).unwrap();

        assert!(store.delete(RefKind::Tag, "v1").unwrap());
        assert!(!store.delete(RefKind::Tag, "v1").unwrap());
        assert!(store.read_all().unwrap().is_empty());
    }

    #[test]
    fn workspace_ref() {
        let dir = tempdir().unwrap();
        let store = SegmentedRefStore::for_root(dir.path());
        store.ensure_exists().unwrap();

        store.upsert(RepoRef {
            kind: RefKind::Workspace,
            name: "dev".to_string(),
            target_event_id: Uuid::from_u128(1),
            workspace: Some(WorkspaceRefConfig {
                auto_rebase: true,
                base_snapshot_id: None,
                max_snapshots: None,
                max_events: None,
            }),
        }).unwrap();

        let r = store.read(RefKind::Workspace, "dev").unwrap().unwrap();
        assert!(r.workspace.unwrap().auto_rebase);
    }

    #[test]
    fn validates_workspace_config() {
        let dir = tempdir().unwrap();
        let store = SegmentedRefStore::for_root(dir.path());
        store.ensure_exists().unwrap();

        // Workspace without config should fail
        let result = store.upsert(RepoRef {
            kind: RefKind::Workspace,
            name: "dev".to_string(),
            target_event_id: Uuid::from_u128(1),
            workspace: None,
        });
        assert!(result.is_err());
    }
}
