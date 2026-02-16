//! Incremental snapshot storage that reuses unchanged file entries.
//!
//! When creating a new snapshot, compares file hashes against a parent
//! snapshot's index and reuses `FileEntry` for unchanged files.

use anyhow::{Result, Context};
use uuid::Uuid;

use crate::file_index::{FileEntry, FileIndex, SnapshotIndex};

/// Result of an incremental snapshot store operation.
#[derive(Debug, Clone)]
pub struct IncrementalSnapshotReport {
    /// Total files in the new snapshot.
    pub total_files: usize,
    /// Files reused from the parent snapshot (unchanged).
    pub reused_files: usize,
    /// Files that were new or changed.
    pub changed_files: usize,
}

/// Build a new snapshot index incrementally by reusing entries from a parent snapshot
/// for files whose hashes haven't changed.
///
/// `new_files` contains the full set of files with their current entries.
/// `parent_index` is the index of the parent snapshot to compare against.
/// Returns a tuple of (optimized SnapshotIndex, report).
pub fn build_incremental_index(
    snapshot_id: Uuid,
    new_files: &[(String, FileEntry)],
    parent_index: Option<&SnapshotIndex>,
) -> (SnapshotIndex, IncrementalSnapshotReport) {
    let mut index = SnapshotIndex::new(snapshot_id);
    let mut reused = 0;
    let mut changed = 0;

    for (path, entry) in new_files {
        if let Some(parent) = parent_index {
            if let Some(parent_entry) = parent.get(path) {
                if parent_entry.file_hash == entry.file_hash {
                    // Reuse the parent's entry (same blocks)
                    index.insert(path.clone(), parent_entry.clone());
                    reused += 1;
                    continue;
                }
            }
        }
        // New or changed file
        index.insert(path.clone(), entry.clone());
        changed += 1;
    }

    let report = IncrementalSnapshotReport {
        total_files: new_files.len(),
        reused_files: reused,
        changed_files: changed,
    };

    (index, report)
}

/// Store a snapshot index incrementally, reusing entries from the parent.
/// Reads the parent index from disk if parent_snapshot_id is provided.
pub fn store_snapshot_incremental(
    file_index: &FileIndex,
    snapshot_id: Uuid,
    new_files: Vec<(String, FileEntry)>,
    parent_snapshot_id: Option<Uuid>,
) -> Result<IncrementalSnapshotReport> {
    let parent_index = match parent_snapshot_id {
        Some(parent_id) if file_index.has(parent_id) => {
            Some(file_index.read(parent_id).with_context(|| {
                format!("failed to read parent snapshot index {}", parent_id)
            })?)
        }
        _ => None,
    };

    let (index, report) = build_incremental_index(
        snapshot_id,
        &new_files,
        parent_index.as_ref(),
    );

    file_index.write(&index)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_index::{BlockRef, FileEntry};

    fn make_entry(hash: &str) -> FileEntry {
        FileEntry {
            blocks: vec![BlockRef {
                hash: hash.to_string(),
                offset: 0,
                length: 100,
            }],
            size: 100,
            file_hash: hash.to_string(),
        }
    }

    #[test]
    fn incremental_reuses_unchanged_files() {
        let parent_id = Uuid::from_u128(1);
        let mut parent_index = SnapshotIndex::new(parent_id);
        parent_index.insert("file1.rs".to_string(), make_entry("aaa"));
        parent_index.insert("file2.rs".to_string(), make_entry("bbb"));
        parent_index.insert("file3.rs".to_string(), make_entry("ccc"));

        let new_files = vec![
            ("file1.rs".to_string(), make_entry("aaa")),  // unchanged
            ("file2.rs".to_string(), make_entry("ddd")),  // changed
            ("file4.rs".to_string(), make_entry("eee")),  // new
        ];

        let new_id = Uuid::from_u128(2);
        let (index, report) = build_incremental_index(new_id, &new_files, Some(&parent_index));

        assert_eq!(report.total_files, 3);
        assert_eq!(report.reused_files, 1);  // file1.rs
        assert_eq!(report.changed_files, 2); // file2.rs + file4.rs
        assert_eq!(index.file_count(), 3);
    }

    #[test]
    fn no_parent_marks_all_as_changed() {
        let new_files = vec![
            ("file1.rs".to_string(), make_entry("aaa")),
            ("file2.rs".to_string(), make_entry("bbb")),
        ];

        let new_id = Uuid::from_u128(1);
        let (index, report) = build_incremental_index(new_id, &new_files, None);

        assert_eq!(report.total_files, 2);
        assert_eq!(report.reused_files, 0);
        assert_eq!(report.changed_files, 2);
        assert_eq!(index.file_count(), 2);
    }

    #[test]
    fn store_incremental_with_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file_index = FileIndex::for_root(dir.path());
        file_index.ensure_exists().unwrap();

        // Create parent index
        let parent_id = Uuid::from_u128(1);
        let mut parent_idx = SnapshotIndex::new(parent_id);
        parent_idx.insert("file1.rs".to_string(), make_entry("aaa"));
        parent_idx.insert("file2.rs".to_string(), make_entry("bbb"));
        file_index.write(&parent_idx).unwrap();

        // Store new snapshot incrementally
        let new_id = Uuid::from_u128(2);
        let new_files = vec![
            ("file1.rs".to_string(), make_entry("aaa")),  // unchanged
            ("file2.rs".to_string(), make_entry("ccc")),  // changed
        ];
        let report = store_snapshot_incremental(&file_index, new_id, new_files, Some(parent_id)).unwrap();

        assert_eq!(report.reused_files, 1);
        assert_eq!(report.changed_files, 1);

        // Verify the stored index
        let stored = file_index.read(new_id).unwrap();
        assert_eq!(stored.file_count(), 2);
    }
}
