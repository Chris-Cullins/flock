use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::layout::REFS_FILE;

pub const CURRENT_REFS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Branch,
    Tag,
    Workspace,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRefConfig {
    pub auto_rebase: bool,
    #[serde(default)]
    pub base_snapshot_id: Option<Uuid>,
    #[serde(default)]
    pub max_snapshots: Option<usize>,
    #[serde(default)]
    pub max_events: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoRef {
    pub kind: RefKind,
    pub name: String,
    pub target_event_id: Uuid,
    #[serde(default)]
    pub workspace: Option<WorkspaceRefConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefRecord {
    pub schema_version: u32,
    pub refs: Vec<RepoRef>,
}

impl RefRecord {
    fn new(refs: Vec<RepoRef>) -> Self {
        Self {
            schema_version: CURRENT_REFS_SCHEMA_VERSION,
            refs,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RefStore {
    path: PathBuf,
}

impl RefStore {
    pub fn for_root(root: &Path) -> Self {
        Self {
            path: root.join(REFS_FILE),
        }
    }

    pub fn ensure_exists(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create refs directory {}", parent.display()))?;
        }

        if self.path.exists() {
            return Ok(());
        }

        File::create(&self.path)
            .with_context(|| format!("failed to create refs file {}", self.path.display()))?;
        self.write_all(&[])?;
        Ok(())
    }

    pub fn read_all(&self) -> Result<Vec<RepoRef>> {
        let raw = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;

        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let refs = if let Ok(record) = serde_json::from_str::<RefRecord>(trimmed) {
            if record.schema_version == 0 || record.schema_version > CURRENT_REFS_SCHEMA_VERSION {
                bail!(
                    "unsupported refs schema version {} (supported: {})",
                    record.schema_version,
                    CURRENT_REFS_SCHEMA_VERSION
                );
            }
            record.refs
        } else if let Ok(legacy) = serde_json::from_str::<Vec<RepoRef>>(trimmed) {
            legacy
        } else {
            return Err(anyhow!(
                "failed to parse refs schema in {}",
                self.path.display()
            ));
        };

        validate_refs(&refs)?;
        Ok(refs)
    }

    pub fn write_all(&self, refs: &[RepoRef]) -> Result<()> {
        validate_refs(refs)?;

        let mut normalized: Vec<RepoRef> = refs.to_vec();
        normalized.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));

        let contents = serde_json::to_string_pretty(&RefRecord::new(normalized))
            .context("failed to serialize refs record")?;
        fs::write(&self.path, format!("{}\n", contents))
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    pub fn upsert(&self, reference: RepoRef) -> Result<()> {
        let mut refs = self.read_all()?;
        if let Some(existing) = refs
            .iter_mut()
            .find(|entry| entry.kind == reference.kind && entry.name == reference.name)
        {
            *existing = reference;
        } else {
            refs.push(reference);
        }
        self.write_all(&refs)
    }

    pub fn delete(&self, kind: RefKind, name: &str) -> Result<bool> {
        let mut refs = self.read_all()?;
        let before_len = refs.len();
        refs.retain(|entry| !(entry.kind == kind && entry.name == name));
        let removed = refs.len() != before_len;
        if removed {
            self.write_all(&refs)?;
        }
        Ok(removed)
    }
}

fn validate_refs(refs: &[RepoRef]) -> Result<()> {
    let mut keys = std::collections::BTreeSet::new();

    for entry in refs {
        if entry.name.trim().is_empty() {
            bail!("ref name cannot be empty");
        }

        if !keys.insert((entry.kind, entry.name.clone())) {
            bail!("duplicate ref {}:{:?}", entry.name, entry.kind);
        }

        match entry.kind {
            RefKind::Workspace => {
                if entry.workspace.is_none() {
                    bail!("workspace ref {} is missing workspace config", entry.name);
                }
            }
            RefKind::Branch | RefKind::Tag => {
                if entry.workspace.is_some() {
                    bail!("non-workspace ref {} has workspace config", entry.name);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_exists_initializes_empty_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = RefStore::for_root(dir.path());

        store.ensure_exists().expect("ensure exists");
        let refs = store.read_all().expect("read refs");
        assert!(refs.is_empty());
    }

    #[test]
    fn upsert_overwrites_existing_ref() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = RefStore::for_root(dir.path());
        store.ensure_exists().expect("ensure exists");

        store
            .upsert(RepoRef {
                kind: RefKind::Branch,
                name: "main".to_string(),
                target_event_id: Uuid::from_u128(1),
                workspace: None,
            })
            .expect("insert branch");
        store
            .upsert(RepoRef {
                kind: RefKind::Branch,
                name: "main".to_string(),
                target_event_id: Uuid::from_u128(2),
                workspace: None,
            })
            .expect("update branch");

        let refs = store.read_all().expect("read refs");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target_event_id, Uuid::from_u128(2));
    }

    #[test]
    fn delete_removes_matching_ref() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = RefStore::for_root(dir.path());
        store.ensure_exists().expect("ensure exists");
        store
            .upsert(RepoRef {
                kind: RefKind::Tag,
                name: "v1".to_string(),
                target_event_id: Uuid::from_u128(10),
                workspace: None,
            })
            .expect("insert tag");

        assert!(store.delete(RefKind::Tag, "v1").expect("delete tag"));
        assert!(store.read_all().expect("read refs").is_empty());
    }
}
