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
use ed25519_dalek::{Signer, SigningKey};

use crate::event::{Event, EventKind, EventRecord, compute_event_hash, event_signing_payload};
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
    matches!(
        kind,
        EventKind::Init(_) | EventKind::Checkpoint(_) | EventKind::Undo(_)
    )
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

    // Parse retained events and rebuild the causal chain + hash chain.
    // Compaction may have removed events from the middle of the chain,
    // so we re-parent each retained event to point to the previous retained event.
    let mut retained_parsed: Vec<ParsedCompactEvent> = retained_lines
        .iter()
        .map(|line| parse_event_with_version(line.trim()))
        .collect::<Result<Vec<_>>>()?;

    // Load signing key for re-signing re-parented events.
    let signing_key = load_signing_key(root).ok();

    let mut prev_id: Option<uuid::Uuid> = None;
    let mut prev_hash: Option<String> = None;
    for parsed in &mut retained_parsed {
        let needs_reparent = match (prev_id, parsed.event.parent_id) {
            (None, None) => false,
            (None, Some(_)) => true,   // first retained event had an archived parent
            (Some(expected), Some(parent)) if parent == expected => false,
            (Some(_), _) => true,      // parent was archived or mismatched
        };

        if needs_reparent {
            parsed.event.parent_id = prev_id;
            // Signature covers parent_id, so re-sign the event.
            if let Some(ref key) = signing_key {
                sign_event(&mut parsed.event, key)?;
            } else {
                // No signing key available — strip the now-invalid signature.
                parsed.event.signer_public_key = None;
                parsed.event.signature = None;
            }
        }

        parsed.event.prev_event_hash = prev_hash;
        prev_hash = Some(compute_event_hash(&parsed.event));
        prev_id = Some(parsed.event.id);
    }

    // Rewrite the event log with the fixed events, preserving original schema versions.
    let mut retained_file = File::create(&event_log_path)
        .with_context(|| format!("failed to rewrite event log {}", event_log_path.display()))?;
    for parsed in &retained_parsed {
        let line = serialize_event(&parsed.event, parsed.schema_version)?;
        writeln!(retained_file, "{}", line)?;
    }

    Ok(CompactionReport {
        total_events: total,
        retained_events: retained_parsed.len(),
        archived_events: archived_lines.len(),
    })
}

/// Parsed event with its original schema version preserved.
struct ParsedCompactEvent {
    event: Event,
    schema_version: Option<u32>,
}

fn parse_event_from_line(line: &str) -> Result<Event> {
    if let Ok(record) = serde_json::from_str::<EventRecord>(line) {
        return Ok(record.event);
    }
    serde_json::from_str::<Event>(line)
        .with_context(|| "failed to parse event line for compaction")
}

fn parse_event_with_version(line: &str) -> Result<ParsedCompactEvent> {
    if let Ok(record) = serde_json::from_str::<EventRecord>(line) {
        return Ok(ParsedCompactEvent {
            event: record.event,
            schema_version: Some(record.schema_version),
        });
    }
    let event = serde_json::from_str::<Event>(line)
        .with_context(|| "failed to parse event line for compaction")?;
    Ok(ParsedCompactEvent {
        event,
        schema_version: None,
    })
}

fn serialize_event(event: &Event, schema_version: Option<u32>) -> Result<String> {
    match schema_version {
        Some(v) => {
            let record = EventRecord {
                schema_version: v,
                event: event.clone(),
            };
            serde_json::to_string(&record).with_context(|| "failed to serialize event record")
        }
        None => {
            // Legacy format — serialize as bare event
            serde_json::to_string(event).with_context(|| "failed to serialize legacy event")
        }
    }
}

fn load_signing_key(root: &Path) -> Result<SigningKey> {
    let key_path = root.join(layout::SIGNING_KEY_FILE);
    let encoded = fs::read_to_string(&key_path)
        .with_context(|| format!("failed to read signing key {}", key_path.display()))?;
    let raw = hex::decode(encoded.trim())
        .with_context(|| "invalid signing key encoding")?;
    let secret = <[u8; 32]>::try_from(raw.as_slice())
        .map_err(|_| anyhow::anyhow!("signing key must be 32 bytes"))?;
    Ok(SigningKey::from_bytes(&secret))
}

fn sign_event(event: &mut Event, key: &SigningKey) -> Result<()> {
    let payload = event_signing_payload(event)?;
    let signature = key.sign(&payload);
    event.signer_public_key = Some(hex::encode(key.verifying_key().to_bytes()));
    event.signature = Some(hex::encode(signature.to_bytes()));
    Ok(())
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
            exploration_id: None,
            session_id: None,
            workspace_name: None,
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

    #[test]
    fn compact_rebuilds_causal_chain() {
        // Regression test for issue #79: compact must produce a valid causal chain
        // even when non-structural events are removed from the middle.
        let dir = tempdir().unwrap();

        let cp1 = make_event(1, None, "1000000000000000000", EventKind::Checkpoint(CheckpointEvent {
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
        let old_session = make_event(2, Some(cp1.id), "1000000000000000001", EventKind::Session(SessionEvent {
            session_id: Uuid::from_u128(200),
            action: SessionAction::Start,
            agent: "agent".to_string(),
            initiator: None,
            task_description: None,
            exploration_id: None,
            result: None,
        }));
        let cp2 = make_event(3, Some(old_session.id), "1000000000000000002", EventKind::Checkpoint(CheckpointEvent {
            label: "cp2".to_string(),
            message: None,
            snapshot_id: Uuid::from_u128(101),
            parent_checkpoint_event: Some(cp1.id),
            snapshot_merkle_root: None,
            ai_intent: None,
            intent_confidence: None,
            files_changed: None,
            category: None,
            scope_label: None,
            structured_description: None,
            git_commit_sha: None,
        }));
        let recent_session = make_event(4, Some(cp2.id), "9000000000000000000", EventKind::Session(SessionEvent {
            session_id: Uuid::from_u128(201),
            action: SessionAction::Start,
            agent: "agent".to_string(),
            initiator: None,
            task_description: None,
            exploration_id: None,
            result: None,
        }));

        write_events_to_log(dir.path(), &[cp1.clone(), old_session, cp2, recent_session]);

        // old_session will be archived (old + non-structural)
        let now = 2000000000000000000u128;
        let cutoff = Duration::from_secs(500);
        let report = compact_event_log(dir.path(), cutoff, now).unwrap();

        assert_eq!(report.total_events, 4);
        assert_eq!(report.retained_events, 3); // cp1, cp2, recent_session
        assert_eq!(report.archived_events, 1); // old_session

        // The retained events should have a valid causal chain:
        // cp1.parent_id = None, cp2.parent_id = cp1.id, recent_session.parent_id = cp2.id
        let log_path = dir.path().join(layout::EVENT_LOG_FILE);
        let content = fs::read_to_string(&log_path).unwrap();
        let events: Vec<Event> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| parse_event_from_line(l).unwrap())
            .collect();

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].parent_id, None);
        assert_eq!(events[0].id, cp1.id);
        assert_eq!(events[1].parent_id, Some(events[0].id));
        assert_eq!(events[2].parent_id, Some(events[1].id));

        // Hash chain should also be valid
        assert!(events[0].prev_event_hash.is_none());
        let expected_hash = compute_event_hash(&events[0]);
        assert_eq!(events[1].prev_event_hash.as_deref(), Some(expected_hash.as_str()));
    }

    #[test]
    fn compact_read_all_succeeds_after_compaction() {
        // Verify that EventLog::read_all works on a compacted log (validates
        // causal chain, hash chain, etc.)
        use crate::event_log::EventLog;

        let dir = tempdir().unwrap();

        let cp1 = make_event(1, None, "1000000000000000000", EventKind::Checkpoint(CheckpointEvent {
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
        let old_session = make_event(2, Some(cp1.id), "1000000000000000001", EventKind::Session(SessionEvent {
            session_id: Uuid::from_u128(200),
            action: SessionAction::Start,
            agent: "agent".to_string(),
            initiator: None,
            task_description: None,
            exploration_id: None,
            result: None,
        }));
        let cp2 = make_event(3, Some(old_session.id), "1000000000000000002", EventKind::Checkpoint(CheckpointEvent {
            label: "cp2".to_string(),
            message: None,
            snapshot_id: Uuid::from_u128(101),
            parent_checkpoint_event: Some(cp1.id),
            snapshot_merkle_root: None,
            ai_intent: None,
            intent_confidence: None,
            files_changed: None,
            category: None,
            scope_label: None,
            structured_description: None,
            git_commit_sha: None,
        }));

        write_events_to_log(dir.path(), &[cp1, old_session, cp2]);

        let now = 2000000000000000000u128;
        let cutoff = Duration::from_secs(500);
        compact_event_log(dir.path(), cutoff, now).unwrap();

        // read_all should succeed — causal chain is valid after compaction
        let event_log = EventLog::for_root(dir.path());
        let events = event_log.read_all().expect("read_all should succeed after compaction");
        assert_eq!(events.len(), 2); // cp1 + cp2
    }
}
