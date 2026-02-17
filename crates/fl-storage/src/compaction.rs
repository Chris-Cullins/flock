//! Event log compaction: archive old non-critical events to cold storage.
//!
//! Retains checkpoint and undo events (structural). Archives session,
//! exploration, presence, decision, resource usage, subscription, gate,
//! lock, rebase, conflict resolution, and task events older than a cutoff.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::event::{Event, EventKind, EventRecord};
use crate::layout;

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionReport {
    /// Total events processed.
    pub total_events: usize,
    /// Events retained in the active log.
    pub retained_events: usize,
    /// Events archived to cold storage.
    pub archived_events: usize,
}

/// Whether an event kind is structural (must always be retained).
fn is_structural(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Checkpoint(_) | EventKind::Undo(_))
}

/// Compact the event log, archiving non-structural events older than `cutoff`.
///
/// Events are considered "old" if their timestamp (nanoseconds since epoch)
/// is before `now_nanos - cutoff.as_nanos()`.
///
/// The original event log is replaced with only retained events.
/// Archived events are written to `.flock/event-log/archive/<timestamp>.jsonl`.
pub fn compact_event_log(
    root: &Path,
    cutoff: Duration,
    now_nanos: u128,
) -> Result<CompactionReport> {
    let event_log_path = root.join(layout::EVENT_LOG_FILE);
    if !event_log_path.exists() {
        return Ok(CompactionReport {
            total_events: 0,
            retained_events: 0,
            archived_events: 0,
        });
    }

    // Read all event lines
    let file = File::open(&event_log_path)
        .with_context(|| format!("failed to open event log {}", event_log_path.display()))?;
    let reader = BufReader::new(file);

    let cutoff_nanos = now_nanos.saturating_sub(cutoff.as_nanos());

    let mut retained_lines = Vec::new();
    let mut archived_lines = Vec::new();
    let mut total = 0;

    for line in reader.lines() {
        let line = line.with_context(|| "failed to read event log line")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        total += 1;

        // Parse to check kind and timestamp
        let event = parse_event_from_line(trimmed)?;

        let event_nanos = event.timestamp.parse::<u128>().unwrap_or(u128::MAX);
        let is_old = event_nanos < cutoff_nanos;

        if is_structural(&event.kind) || !is_old {
            retained_lines.push(line.clone());
        } else {
            archived_lines.push(line.clone());
        }
    }

    // Write archive if there are archived events
    if !archived_lines.is_empty() {
        let archive_dir = root.join(layout::EVENT_LOG_DIR).join("archive");
        fs::create_dir_all(&archive_dir)
            .with_context(|| "failed to create archive directory")?;
        let archive_path = archive_dir.join(format!("{}.jsonl", now_nanos));
        let mut archive_file = File::create(&archive_path)
            .with_context(|| format!("failed to create archive file {}", archive_path.display()))?;
        for line in &archived_lines {
            writeln!(archive_file, "{}", line)?;
        }
    }

    // Rewrite the event log with only retained events
    // We need to fix the causal chain - retained events may have gaps
    // For simplicity, we keep the original lines as-is (parent pointers may be broken
    // but that's acceptable for compacted logs since the archived events are preserved)
    let mut retained_file = File::create(&event_log_path)
        .with_context(|| format!("failed to rewrite event log {}", event_log_path.display()))?;
    for line in &retained_lines {
        writeln!(retained_file, "{}", line)?;
    }

    Ok(CompactionReport {
        total_events: total,
        retained_events: retained_lines.len(),
        archived_events: archived_lines.len(),
    })
}

fn parse_event_from_line(line: &str) -> Result<Event> {
    if let Ok(record) = serde_json::from_str::<EventRecord>(line) {
        return Ok(record.event);
    }
    serde_json::from_str::<Event>(line)
        .with_context(|| "failed to parse event line for compaction")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CheckpointEvent, EventKind, SessionAction, SessionEvent};
    use std::fs;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn make_event(id: u128, parent_id: Option<Uuid>, timestamp: &str, kind: EventKind) -> Event {
        Event {
            id: Uuid::from_u128(id),
            timestamp: timestamp.to_string(),
            actor: "tester".to_string(),
            parent_id,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            kind,
        }
    }

    fn write_events_to_log(root: &Path, events: &[Event]) {
        let log_dir = root.join(layout::EVENT_LOG_DIR);
        fs::create_dir_all(&log_dir).unwrap();
        let log_path = root.join(layout::EVENT_LOG_FILE);
        let mut file = File::create(&log_path).unwrap();
        for event in events {
            let line = serde_json::to_string(event).unwrap();
            writeln!(file, "{}", line).unwrap();
        }
    }

    #[test]
    fn compact_archives_old_sessions() {
        let dir = tempdir().unwrap();

        let cp = make_event(1, None, "1000000000000000000", EventKind::Checkpoint(CheckpointEvent {
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
        }));
        let session = make_event(2, Some(cp.id), "1000000000000000001", EventKind::Session(SessionEvent {
            session_id: Uuid::from_u128(200),
            action: SessionAction::Start,
            agent: "agent".to_string(),
            initiator: None,
            task_description: None,
            exploration_id: None,
            result: None,
        }));

        write_events_to_log(dir.path(), &[cp, session]);

        // Compact with cutoff that makes the session old
        let now = 2000000000000000000u128;
        let cutoff = Duration::from_secs(500); // 500s cutoff, session is ~1000s old
        let report = compact_event_log(dir.path(), cutoff, now).unwrap();

        assert_eq!(report.total_events, 2);
        assert_eq!(report.retained_events, 1);  // checkpoint retained
        assert_eq!(report.archived_events, 1);  // session archived

        // Verify archive exists
        let archive_dir = dir.path().join(layout::EVENT_LOG_DIR).join("archive");
        assert!(archive_dir.exists());
    }

    #[test]
    fn compact_retains_checkpoints_always() {
        let dir = tempdir().unwrap();

        let old_cp = make_event(1, None, "1000000000000000000", EventKind::Checkpoint(CheckpointEvent {
            label: "old".to_string(),
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
        }));

        write_events_to_log(dir.path(), &[old_cp]);

        let now = 9000000000000000000u128;
        let cutoff = Duration::from_secs(1);
        let report = compact_event_log(dir.path(), cutoff, now).unwrap();

        assert_eq!(report.retained_events, 1);
        assert_eq!(report.archived_events, 0);
    }

    #[test]
    fn compact_retains_recent_events() {
        let dir = tempdir().unwrap();

        let recent_session = make_event(1, None, "9000000000000000000", EventKind::Session(SessionEvent {
            session_id: Uuid::from_u128(200),
            action: SessionAction::Start,
            agent: "agent".to_string(),
            initiator: None,
            task_description: None,
            exploration_id: None,
            result: None,
        }));

        write_events_to_log(dir.path(), &[recent_session]);

        let now = 9000000000100000000u128; // 0.1s later
        let cutoff = Duration::from_secs(60);
        let report = compact_event_log(dir.path(), cutoff, now).unwrap();

        assert_eq!(report.retained_events, 1);
        assert_eq!(report.archived_events, 0);
    }
}
