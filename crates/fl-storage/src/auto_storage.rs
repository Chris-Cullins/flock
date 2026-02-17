//! Auto-detecting storage facade that transparently uses segmented storage
//! when available and auto-migrates from monolithic format when size thresholds
//! are crossed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use uuid::Uuid;

use crate::event::Event;
use crate::event_log::EventLog;
use crate::event_log_migration::migrate_to_segmented;
use crate::file_lock::{FileLock, LockPaths};
use crate::refs::{RefKind, RefStore, RepoRef};
use crate::refs_migration::migrate_refs_to_segmented;
use crate::segmented_event_log::SegmentedEventLog;
use crate::segmented_refs::SegmentedRefStore;

/// Event log file size threshold (in bytes) above which auto-migration triggers.
/// 1 MB — a typical event is ~500 bytes, so this is roughly 2,000 events.
const EVENT_LOG_SIZE_THRESHOLD: u64 = 1_024 * 1_024;

/// Refs file size threshold (in bytes) above which auto-migration triggers.
/// 64 KB — enough for ~100+ refs.
const REFS_SIZE_THRESHOLD: u64 = 64 * 1_024;

/// Auto-detecting event log that prefers segmented format when available.
pub struct AutoEventLog {
    root: PathBuf,
}

impl AutoEventLog {
    pub fn for_root(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    fn segmented(&self) -> SegmentedEventLog {
        SegmentedEventLog::for_root(&self.root)
    }

    fn monolithic(&self) -> EventLog {
        EventLog::for_root(&self.root)
    }

    fn use_segmented(&self) -> bool {
        self.segmented().exists()
    }

    /// Ensure the backing store exists.
    pub fn ensure_exists(&self) -> Result<()> {
        if self.use_segmented() {
            self.segmented().ensure_exists()
        } else {
            self.monolithic().ensure_exists()
        }
    }

    /// Return the ID of the last event, or `None` if the log is empty.
    pub fn latest_event_id(&self) -> Result<Option<Uuid>> {
        if self.use_segmented() {
            self.segmented().latest_event_id()
        } else {
            self.monolithic().latest_event_id()
        }
    }

    /// Read all events, preferring segmented format.
    pub fn read_all(&self) -> Result<Vec<Event>> {
        if self.use_segmented() {
            self.segmented().read_all()
        } else {
            self.monolithic().read_all()
        }
    }

    /// Append a single event. Resolves `parent_id` atomically under the lock
    /// to prevent concurrent writers from creating broken causal chains.
    ///
    /// The `finalize` callback is invoked after `parent_id` is set but before
    /// the event is written, allowing the caller to re-sign the event with
    /// the correct parent.
    ///
    /// Acquires an exclusive filesystem lock to prevent corruption from
    /// concurrent writers.
    pub fn append(
        &self,
        event: &mut Event,
        finalize: impl FnOnce(&mut Event) -> Result<()>,
    ) -> Result<()> {
        let _lock = FileLock::acquire(&LockPaths::event_log(&self.root))
            .context("failed to acquire event log lock")?;

        // Resolve parent_id under the lock so concurrent writers always see
        // the true latest event, not a stale snapshot from before the lock.
        let latest_id = if self.use_segmented() {
            self.segmented().latest_event_id()?
        } else {
            self.monolithic().latest_event_id()?
        };
        event.parent_id = latest_id;
        finalize(event)?;

        if self.use_segmented() {
            return self.segmented().append(event);
        }

        self.monolithic().append(event)?;
        self.maybe_migrate_event_log()?;
        Ok(())
    }

    /// Append a batch of events.
    ///
    /// Acquires an exclusive filesystem lock to prevent corruption from
    /// concurrent writers.
    pub fn append_batch(&self, events: &[Event]) -> Result<()> {
        let _lock = FileLock::acquire(&LockPaths::event_log(&self.root))
            .context("failed to acquire event log lock")?;

        if self.use_segmented() {
            let seg = self.segmented();
            for event in events {
                seg.append(event)?;
            }
            return Ok(());
        }

        self.monolithic().append_batch(events)?;
        self.maybe_migrate_event_log()?;
        Ok(())
    }

    /// Append a batch of events from a remote pull.
    ///
    /// The first event's `parent_id` may not match the local log's tail because
    /// local-only events (e.g. RemoteSync) may have been appended since the
    /// last synced event. Internal chain of pulled events is still validated.
    pub fn append_batch_for_pull(&self, events: &[Event]) -> Result<()> {
        let _lock = FileLock::acquire(&LockPaths::event_log(&self.root))
            .context("failed to acquire event log lock")?;

        if self.use_segmented() {
            return self.segmented().append_batch_for_pull(events);
        }

        self.monolithic().append_batch_from_pull(events)?;
        self.maybe_migrate_event_log()?;
        Ok(())
    }

    /// Check if the monolithic event log exceeds the size threshold and
    /// migrate to segmented format if so.
    fn maybe_migrate_event_log(&self) -> Result<()> {
        let legacy_path = self.root.join(crate::layout::EVENT_LOG_FILE);
        let size = match std::fs::metadata(&legacy_path) {
            Ok(m) => m.len(),
            Err(_) => return Ok(()),
        };

        if size >= EVENT_LOG_SIZE_THRESHOLD {
            migrate_to_segmented(&self.root)
                .context("auto-migration of event log to segmented format failed")?;
        }
        Ok(())
    }
}

/// Auto-detecting ref store that prefers segmented format when available.
pub struct AutoRefStore {
    root: PathBuf,
}

impl AutoRefStore {
    pub fn for_root(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    fn segmented(&self) -> SegmentedRefStore {
        SegmentedRefStore::for_root(&self.root)
    }

    fn monolithic(&self) -> RefStore {
        RefStore::for_root(&self.root)
    }

    fn use_segmented(&self) -> bool {
        self.segmented().exists()
    }

    /// Ensure backing store exists.
    pub fn ensure_exists(&self) -> Result<()> {
        if self.use_segmented() {
            self.segmented().ensure_exists()
        } else {
            self.monolithic().ensure_exists()
        }
    }

    /// Read all refs.
    pub fn read_all(&self) -> Result<Vec<RepoRef>> {
        if self.use_segmented() {
            self.segmented().read_all()
        } else {
            self.monolithic().read_all()
        }
    }

    /// Write all refs (used during init and bulk operations).
    ///
    /// Acquires an exclusive filesystem lock to prevent corruption from
    /// concurrent writers.
    pub fn write_all(&self, refs: &[RepoRef]) -> Result<()> {
        let _lock = FileLock::acquire(&LockPaths::refs(&self.root))
            .context("failed to acquire refs lock")?;

        if self.use_segmented() {
            let seg = self.segmented();
            seg.ensure_exists()?;
            // Delete existing refs first, then write new ones
            let existing = seg.read_all().unwrap_or_default();
            for r in &existing {
                seg.delete(r.kind, &r.name)?;
            }
            for r in refs {
                seg.upsert(r.clone())?;
            }
            Ok(())
        } else {
            self.monolithic().write_all(refs)
        }
    }

    /// Upsert a single ref. Checks size threshold after write.
    ///
    /// Acquires an exclusive filesystem lock to prevent corruption from
    /// concurrent writers.
    pub fn upsert(&self, reference: RepoRef) -> Result<()> {
        let _lock = FileLock::acquire(&LockPaths::refs(&self.root))
            .context("failed to acquire refs lock")?;

        if self.use_segmented() {
            return self.segmented().upsert(reference);
        }

        self.monolithic().upsert(reference)?;
        self.maybe_migrate_refs()?;
        Ok(())
    }

    /// Delete a ref.
    ///
    /// Acquires an exclusive filesystem lock to prevent corruption from
    /// concurrent writers.
    pub fn delete(&self, kind: RefKind, name: &str) -> Result<bool> {
        let _lock = FileLock::acquire(&LockPaths::refs(&self.root))
            .context("failed to acquire refs lock")?;

        if self.use_segmented() {
            self.segmented().delete(kind, name)
        } else {
            self.monolithic().delete(kind, name)
        }
    }

    /// Check if the monolithic refs file exceeds the size threshold and
    /// migrate to segmented format if so.
    fn maybe_migrate_refs(&self) -> Result<()> {
        let legacy_path = self.root.join(crate::layout::REFS_FILE);
        let size = match std::fs::metadata(&legacy_path) {
            Ok(m) => m.len(),
            Err(_) => return Ok(()),
        };

        if size >= REFS_SIZE_THRESHOLD {
            migrate_refs_to_segmented(&self.root)
                .context("auto-migration of refs to segmented format failed")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CheckpointEvent, EventKind};
    use crate::refs::WorkspaceRefConfig;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn make_event(id: u128, parent_id: Option<Uuid>) -> Event {
        Event {
            id: Uuid::from_u128(id),
            timestamp: format!("1739571600000000{}", id),
            actor: "tester".to_string(),
            parent_id,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            kind: EventKind::Checkpoint(CheckpointEvent {
                label: format!("cp-{}", id),
                message: None,
                snapshot_id: Uuid::from_u128(id + 1000),
                parent_checkpoint_event: None,
                snapshot_merkle_root: None,
                ai_intent: None,
                intent_confidence: None,
                files_changed: None,
                category: None,
                scope_label: None,
                structured_description: None,
                git_commit_sha: None,
            }),
        }
    }

    /// Write events as bare JSON (legacy format, no signature required).
    fn write_legacy_events(root: &std::path::Path, events: &[Event]) {
        let log_path = root.join(crate::layout::EVENT_LOG_FILE);
        let dir = log_path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let mut content = String::new();
        for e in events {
            content.push_str(&serde_json::to_string(e).unwrap());
            content.push('\n');
        }
        std::fs::write(&log_path, content).unwrap();
    }

    #[test]
    fn auto_event_log_uses_monolithic_by_default() {
        let dir = tempdir().unwrap();

        let e1 = make_event(1, None);
        let e2 = make_event(2, Some(e1.id));
        write_legacy_events(dir.path(), &[e1.clone(), e2.clone()]);

        let auto_log = AutoEventLog::for_root(dir.path());
        assert!(!auto_log.use_segmented());

        let events = auto_log.read_all().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, e1.id);
        assert_eq!(events[1].id, e2.id);
    }

    #[test]
    fn auto_event_log_prefers_segmented() {
        let dir = tempdir().unwrap();
        // Create both monolithic and segmented
        write_legacy_events(dir.path(), &[]);

        let seg = SegmentedEventLog::for_root(dir.path());
        seg.ensure_exists().unwrap();

        let e1 = make_event(1, None);
        seg.append(&e1).unwrap();

        let auto_log = AutoEventLog::for_root(dir.path());
        assert!(auto_log.use_segmented());

        let events = auto_log.read_all().unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn auto_event_log_migrates_on_threshold() {
        let dir = tempdir().unwrap();

        // Write many legacy events to exceed the 1 MB threshold
        let mut events = Vec::new();
        let mut parent = None;
        // Each event is ~300-400 bytes as JSON; write enough to exceed 1 MB
        for i in 1..=4000u128 {
            let e = make_event(i, parent);
            parent = Some(e.id);
            events.push(e);
        }
        write_legacy_events(dir.path(), &events);

        let auto_log = AutoEventLog::for_root(dir.path());
        assert!(!auto_log.use_segmented());

        // Appending should trigger auto-migration since file > 1 MB
        // We need to use the segmented path for the actual append since
        // monolithic append requires signatures. Instead, verify the
        // threshold detection works by checking the file size.
        let log_path = dir.path().join(crate::layout::EVENT_LOG_FILE);
        let size = std::fs::metadata(&log_path).unwrap().len();
        assert!(size >= EVENT_LOG_SIZE_THRESHOLD, "test file should exceed threshold");

        // Call maybe_migrate directly
        auto_log.maybe_migrate_event_log().unwrap();
        assert!(auto_log.use_segmented());

        // Verify segmented log has all events
        let read_events = auto_log.read_all().unwrap();
        assert_eq!(read_events.len(), 4000);
    }

    #[test]
    fn auto_event_log_no_migrate_below_threshold() {
        let dir = tempdir().unwrap();

        let e1 = make_event(1, None);
        write_legacy_events(dir.path(), &[e1]);

        let auto_log = AutoEventLog::for_root(dir.path());
        auto_log.maybe_migrate_event_log().unwrap();

        // File is small, should still be monolithic
        assert!(!auto_log.use_segmented());
    }

    #[test]
    fn auto_ref_store_uses_monolithic_by_default() {
        let dir = tempdir().unwrap();
        let mono = RefStore::for_root(dir.path());
        mono.ensure_exists().unwrap();

        let auto_refs = AutoRefStore::for_root(dir.path());
        assert!(!auto_refs.use_segmented());

        auto_refs
            .upsert(RepoRef {
                kind: RefKind::Branch,
                name: "main".to_string(),
                target_event_id: Uuid::from_u128(1),
                workspace: None,
            })
            .unwrap();

        let refs = auto_refs.read_all().unwrap();
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn auto_ref_store_prefers_segmented() {
        let dir = tempdir().unwrap();
        let mono = RefStore::for_root(dir.path());
        mono.ensure_exists().unwrap();

        let seg = SegmentedRefStore::for_root(dir.path());
        seg.ensure_exists().unwrap();
        seg.upsert(RepoRef {
            kind: RefKind::Branch,
            name: "main".to_string(),
            target_event_id: Uuid::from_u128(1),
            workspace: None,
        })
        .unwrap();

        let auto_refs = AutoRefStore::for_root(dir.path());
        assert!(auto_refs.use_segmented());

        let refs = auto_refs.read_all().unwrap();
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn auto_ref_store_write_all_segmented() {
        let dir = tempdir().unwrap();
        let seg = SegmentedRefStore::for_root(dir.path());
        seg.ensure_exists().unwrap();

        let auto_refs = AutoRefStore::for_root(dir.path());
        auto_refs
            .write_all(&[
                RepoRef {
                    kind: RefKind::Branch,
                    name: "main".to_string(),
                    target_event_id: Uuid::from_u128(1),
                    workspace: None,
                },
                RepoRef {
                    kind: RefKind::Tag,
                    name: "v1".to_string(),
                    target_event_id: Uuid::from_u128(2),
                    workspace: None,
                },
            ])
            .unwrap();

        let refs = auto_refs.read_all().unwrap();
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn auto_ref_store_delete() {
        let dir = tempdir().unwrap();
        let mono = RefStore::for_root(dir.path());
        mono.ensure_exists().unwrap();

        let auto_refs = AutoRefStore::for_root(dir.path());
        auto_refs
            .upsert(RepoRef {
                kind: RefKind::Branch,
                name: "main".to_string(),
                target_event_id: Uuid::from_u128(1),
                workspace: None,
            })
            .unwrap();

        assert!(auto_refs.delete(RefKind::Branch, "main").unwrap());
        assert!(auto_refs.read_all().unwrap().is_empty());
    }

    #[test]
    fn auto_ref_store_workspace_ref() {
        let dir = tempdir().unwrap();
        let mono = RefStore::for_root(dir.path());
        mono.ensure_exists().unwrap();

        let auto_refs = AutoRefStore::for_root(dir.path());
        auto_refs
            .upsert(RepoRef {
                kind: RefKind::Workspace,
                name: "dev".to_string(),
                target_event_id: Uuid::from_u128(1),
                workspace: Some(WorkspaceRefConfig {
                    auto_rebase: true,
                    base_snapshot_id: None,
                    max_snapshots: None,
                    max_events: None,
                }),
            })
            .unwrap();

        let refs = auto_refs.read_all().unwrap();
        assert_eq!(refs.len(), 1);
        assert!(refs[0].workspace.as_ref().unwrap().auto_rebase);
    }

    #[test]
    fn auto_ref_store_migrates_on_threshold() {
        let dir = tempdir().unwrap();
        let mono = RefStore::for_root(dir.path());
        mono.ensure_exists().unwrap();

        // Write enough refs to exceed 64 KB threshold
        for i in 0..500u128 {
            mono.upsert(RepoRef {
                kind: RefKind::Branch,
                name: format!("branch-{:04}", i),
                target_event_id: Uuid::from_u128(i + 1),
                workspace: None,
            })
            .unwrap();
        }

        let refs_path = dir.path().join(crate::layout::REFS_FILE);
        let size = std::fs::metadata(&refs_path).unwrap().len();
        assert!(size >= REFS_SIZE_THRESHOLD, "test refs file should exceed threshold");

        let auto_refs = AutoRefStore::for_root(dir.path());
        assert!(!auto_refs.use_segmented());

        // Upserting one more ref should trigger auto-migration
        auto_refs
            .upsert(RepoRef {
                kind: RefKind::Branch,
                name: "trigger".to_string(),
                target_event_id: Uuid::from_u128(9999),
                workspace: None,
            })
            .unwrap();

        assert!(auto_refs.use_segmented());

        // Verify all refs are readable
        let refs = auto_refs.read_all().unwrap();
        assert_eq!(refs.len(), 501);
    }

    #[test]
    fn auto_ref_store_no_migrate_below_threshold() {
        let dir = tempdir().unwrap();
        let mono = RefStore::for_root(dir.path());
        mono.ensure_exists().unwrap();

        let auto_refs = AutoRefStore::for_root(dir.path());
        auto_refs
            .upsert(RepoRef {
                kind: RefKind::Branch,
                name: "main".to_string(),
                target_event_id: Uuid::from_u128(1),
                workspace: None,
            })
            .unwrap();

        assert!(!auto_refs.use_segmented());
    }
}
