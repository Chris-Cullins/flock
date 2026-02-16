//! Migration from single-file refs.json to segmented per-ref files.

use std::path::Path;

use anyhow::{Result, bail};

use crate::refs::RefStore;
use crate::segmented_refs::SegmentedRefStore;

/// Result of migrating refs.
#[derive(Debug, Clone)]
pub struct RefsMigrationReport {
    pub refs_migrated: usize,
}

/// Migrate from monolithic refs.json to segmented per-ref files.
/// The original file is preserved (not deleted).
pub fn migrate_refs_to_segmented(root: &Path) -> Result<RefsMigrationReport> {
    let legacy = RefStore::for_root(root);
    let segmented = SegmentedRefStore::for_root(root);

    if segmented.exists() {
        bail!("segmented refs already exist; aborting migration");
    }

    // Read all refs from monolithic store
    let refs = match legacy.read_all() {
        Ok(r) => r,
        Err(_) => return Ok(RefsMigrationReport { refs_migrated: 0 }),
    };

    if refs.is_empty() {
        return Ok(RefsMigrationReport { refs_migrated: 0 });
    }

    segmented.ensure_exists()?;

    let mut count = 0;
    for r in &refs {
        segmented.upsert(r.clone())?;
        count += 1;
    }

    Ok(RefsMigrationReport {
        refs_migrated: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::{RefKind, RepoRef};
    use tempfile::tempdir;
    use uuid::Uuid;

    #[test]
    fn migrate_empty_refs() {
        let dir = tempdir().unwrap();
        let store = RefStore::for_root(dir.path());
        store.ensure_exists().unwrap();

        let report = migrate_refs_to_segmented(dir.path()).unwrap();
        assert_eq!(report.refs_migrated, 0);
    }

    #[test]
    fn migrate_preserves_refs() {
        let dir = tempdir().unwrap();
        let store = RefStore::for_root(dir.path());
        store.ensure_exists().unwrap();

        store
            .upsert(RepoRef {
                kind: RefKind::Branch,
                name: "main".to_string(),
                target_event_id: Uuid::from_u128(1),
                workspace: None,
            })
            .unwrap();
        store
            .upsert(RepoRef {
                kind: RefKind::Tag,
                name: "v1.0".to_string(),
                target_event_id: Uuid::from_u128(2),
                workspace: None,
            })
            .unwrap();

        let report = migrate_refs_to_segmented(dir.path()).unwrap();
        assert_eq!(report.refs_migrated, 2);

        let segmented = SegmentedRefStore::for_root(dir.path());
        let refs = segmented.read_all().unwrap();
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn migrate_rejects_if_already_migrated() {
        let dir = tempdir().unwrap();
        let store = RefStore::for_root(dir.path());
        store.ensure_exists().unwrap();

        let segmented = SegmentedRefStore::for_root(dir.path());
        segmented.ensure_exists().unwrap();

        assert!(migrate_refs_to_segmented(dir.path()).is_err());
    }
}
