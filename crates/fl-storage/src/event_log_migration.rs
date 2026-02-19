//! Migration from single-file JSONL event log to segmented format.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};

use crate::event::{Event, EventRecord};
use crate::layout::EVENT_LOG_FILE;
use crate::segmented_event_log::SegmentedEventLog;

/// Result of migrating the event log.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub events_migrated: usize,
    pub segments_created: u32,
}

/// Migrate from a single JSONL event log to segmented format.
/// The original file is preserved (not deleted).
pub fn migrate_to_segmented(root: &Path) -> Result<MigrationReport> {
    let legacy_path = root.join(EVENT_LOG_FILE);
    if !legacy_path.exists() {
        return Ok(MigrationReport {
            events_migrated: 0,
            segments_created: 0,
        });
    }

    let segmented = SegmentedEventLog::for_root(root);
    if segmented.exists() {
        anyhow::bail!("segmented event log already exists; aborting migration");
    }

    segmented.ensure_exists()?;

    // Read legacy events
    let file = File::open(&legacy_path)
        .with_context(|| format!("failed to open legacy event log {}", legacy_path.display()))?;
    let reader = BufReader::new(file);

    let mut count = 0;
    for line in reader.lines() {
        let line = line.with_context(|| "failed to read line from legacy event log")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse as EventRecord (versioned) or bare Event (legacy)
        let event: Event = if let Ok(record) = serde_json::from_str::<EventRecord>(trimmed) {
            record.event
        } else {
            serde_json::from_str::<Event>(trimmed)
                .with_context(|| format!("failed to parse legacy event: {}", trimmed))?
        };

        segmented.append(&event)?;
        count += 1;
    }

    let segments_created = ((count + crate::segmented_event_log::SEGMENT_SIZE - 1) / crate::segmented_event_log::SEGMENT_SIZE) as u32;
    if count == 0 {
        // empty log
        return Ok(MigrationReport {
            events_migrated: 0,
            segments_created: 0,
        });
    }

    Ok(MigrationReport {
        events_migrated: count,
        segments_created,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CheckpointEvent, EventKind};
    use crate::event_log::EventLog;
    use std::fs;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[test]
    fn migrate_empty_log() {
        let dir = tempdir().unwrap();
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().unwrap();

        let report = migrate_to_segmented(dir.path()).unwrap();
        assert_eq!(report.events_migrated, 0);
    }

    #[test]
    fn migrate_preserves_events() {
        let dir = tempdir().unwrap();

        // Write legacy events directly (without signatures to keep it simple)
        let e1 = Event {
            id: Uuid::from_u128(1),
            timestamp: "1000000000".to_string(),
            actor: "tester".to_string(),
            parent_id: None,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            exploration_id: None,
            session_id: None,
            workspace_name: None,
            kind: EventKind::Checkpoint(CheckpointEvent {
                label: "cp1".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(100),
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
        };
        let e2 = Event {
            id: Uuid::from_u128(2),
            timestamp: "1000000001".to_string(),
            actor: "tester".to_string(),
            parent_id: Some(e1.id),
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            exploration_id: None,
            session_id: None,
            workspace_name: None,
            kind: EventKind::Checkpoint(CheckpointEvent {
                label: "cp2".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(101),
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
        };

        // Write as bare JSON events (legacy format)
        let legacy_path = dir.path().join(crate::layout::EVENT_LOG_DIR);
        fs::create_dir_all(&legacy_path).unwrap();
        let log_file = dir.path().join(crate::layout::EVENT_LOG_FILE);
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&e1).unwrap(),
            serde_json::to_string(&e2).unwrap()
        );
        fs::write(&log_file, content).unwrap();

        let report = migrate_to_segmented(dir.path()).unwrap();
        assert_eq!(report.events_migrated, 2);

        // Verify segmented log has the events
        let segmented = SegmentedEventLog::for_root(dir.path());
        let events = segmented.read_all().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, e1.id);
        assert_eq!(events[1].id, e2.id);
    }

    #[test]
    fn migrate_rejects_if_already_migrated() {
        let dir = tempdir().unwrap();
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().unwrap();

        // Create segmented log first
        let segmented = SegmentedEventLog::for_root(dir.path());
        segmented.ensure_exists().unwrap();

        assert!(migrate_to_segmented(dir.path()).is_err());
    }
}
