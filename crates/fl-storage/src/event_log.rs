use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use uuid::Uuid;

use crate::event::{CURRENT_EVENT_SCHEMA_VERSION, Event, EventRecord, event_signing_payload};
use crate::layout::EVENT_LOG_FILE;

#[derive(Debug, Clone)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn for_root(root: &Path) -> Self {
        Self {
            path: root.join(EVENT_LOG_FILE),
        }
    }

    pub fn ensure_exists(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create event-log directory {}", parent.display())
            })?;
        }

        if !self.path.exists() {
            File::create(&self.path)
                .with_context(|| format!("failed to create {}", self.path.display()))?;
        }

        Ok(())
    }

    pub fn read_all(&self) -> Result<Vec<Event>> {
        let file = File::open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        let reader = BufReader::new(file);

        let mut parsed_events = Vec::new();
        for line in reader.lines() {
            let line =
                line.with_context(|| format!("failed to read line in {}", self.path.display()))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed = parse_event_line(trimmed)
                .with_context(|| format!("failed to parse event JSON: {}", trimmed))?;
            parsed_events.push(parsed);
        }

        validate_causal_chain(&parsed_events)?;
        validate_signatures(&parsed_events)?;

        let events = parsed_events
            .into_iter()
            .map(|parsed| parsed.event)
            .collect();
        Ok(events)
    }

    pub fn append(&self, event: &Event) -> Result<()> {
        self.ensure_exists()?;
        let expected_parent = self.read_all()?.last().map(|entry| entry.id);
        validate_next_parent(event, expected_parent)?;
        verify_event_signature(event, true)?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {} for append", self.path.display()))?;

        let record = EventRecord::from_event(event);
        let line = serde_json::to_string(&record).context("failed to serialize event")?;
        writeln!(file, "{}", line).context("failed to append event to log")?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ParsedEvent {
    event: Event,
    schema_version: Option<u32>,
}

fn parse_event_line(line: &str) -> Result<ParsedEvent> {
    if let Ok(record) = serde_json::from_str::<EventRecord>(line) {
        if record.schema_version == 0 || record.schema_version > CURRENT_EVENT_SCHEMA_VERSION {
            bail!(
                "unsupported event schema version {} (supported: {})",
                record.schema_version,
                CURRENT_EVENT_SCHEMA_VERSION
            );
        }
        return Ok(ParsedEvent {
            event: record.event,
            schema_version: Some(record.schema_version),
        });
    }

    let event = serde_json::from_str::<Event>(line)
        .context("event line did not match versioned or legacy event schema")?;
    Ok(ParsedEvent {
        event,
        schema_version: None,
    })
}

fn validate_causal_chain(events: &[ParsedEvent]) -> Result<()> {
    let mut previous_event_id = None;

    for parsed in events {
        match (previous_event_id, parsed.event.parent_id) {
            (None, None) => {}
            (None, Some(parent)) => {
                bail!(
                    "invalid causal chain: first event {} points to parent {}",
                    parsed.event.id,
                    parent
                )
            }
            (Some(expected_parent), Some(parent)) if parent == expected_parent => {}
            (Some(expected_parent), Some(parent)) => {
                bail!(
                    "invalid causal chain: event {} points to {}, expected {}",
                    parsed.event.id,
                    parent,
                    expected_parent
                )
            }
            (Some(expected_parent), None) => {
                if parsed.schema_version.unwrap_or(1) >= 2 {
                    bail!(
                        "invalid causal chain: event {} is missing parent pointer (expected {})",
                        parsed.event.id,
                        expected_parent
                    );
                }
            }
        }

        previous_event_id = Some(parsed.event.id);
    }

    Ok(())
}

fn validate_next_parent(event: &Event, expected_parent: Option<Uuid>) -> Result<()> {
    match (expected_parent, event.parent_id) {
        (None, None) => Ok(()),
        (None, Some(parent)) => bail!(
            "invalid causal chain: first event {} cannot reference parent {}",
            event.id,
            parent
        ),
        (Some(expected_parent), Some(parent)) if parent == expected_parent => Ok(()),
        (Some(expected_parent), Some(parent)) => bail!(
            "invalid causal chain: event {} points to {}, expected {}",
            event.id,
            parent,
            expected_parent
        ),
        (Some(expected_parent), None) => bail!(
            "invalid causal chain: event {} is missing parent pointer (expected {})",
            event.id,
            expected_parent
        ),
    }
}

fn validate_signatures(events: &[ParsedEvent]) -> Result<()> {
    for parsed in events {
        let require_signature = parsed.schema_version.unwrap_or(1) >= 3;
        verify_event_signature(&parsed.event, require_signature)?;
    }
    Ok(())
}

fn verify_event_signature(event: &Event, require_signature: bool) -> Result<()> {
    let (Some(public_key_hex), Some(signature_hex)) = (&event.signer_public_key, &event.signature)
    else {
        if require_signature {
            bail!("event {} is missing ed25519 signature metadata", event.id);
        }
        if event.signer_public_key.is_some() || event.signature.is_some() {
            bail!("event {} has partial signature metadata", event.id);
        }
        return Ok(());
    };

    let public_key = hex::decode(public_key_hex)
        .with_context(|| format!("event {} has invalid signer public key encoding", event.id))?;
    let public_key = <[u8; 32]>::try_from(public_key.as_slice())
        .with_context(|| format!("event {} has invalid signer public key length", event.id))?;

    let signature = hex::decode(signature_hex)
        .with_context(|| format!("event {} has invalid signature encoding", event.id))?;
    let signature = <[u8; 64]>::try_from(signature.as_slice())
        .with_context(|| format!("event {} has invalid signature length", event.id))?;

    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .with_context(|| format!("event {} has invalid signer public key bytes", event.id))?;
    let signature = Signature::from_bytes(&signature);
    let payload = event_signing_payload(event)?;
    verifying_key
        .verify(&payload, &signature)
        .with_context(|| format!("event {} failed ed25519 signature verification", event.id))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::event::{CheckpointEvent, EventKind, event_signing_payload};

    #[test]
    fn append_writes_schema_versioned_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let event = signed_checkpoint_event(None);
        event_log.append(&event).expect("append event");

        let raw = fs::read_to_string(dir.path().join(EVENT_LOG_FILE)).expect("read event log");
        let line = raw.lines().next().expect("one event line");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid json");
        let event_id = event.id.to_string();

        assert_eq!(
            value.get("schema_version").and_then(|v| v.as_u64()),
            Some(CURRENT_EVENT_SCHEMA_VERSION as u64)
        );
        assert_eq!(
            value
                .get("event")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str()),
            Some(event_id.as_str())
        );
    }

    #[test]
    fn read_all_supports_legacy_event_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let first = sample_checkpoint_event(None);
        let second = sample_checkpoint_event(None);
        let first_legacy_line = serde_json::to_string(&first).expect("serialize legacy event");
        let second_legacy_line = serde_json::to_string(&second).expect("serialize legacy event");
        fs::write(
            dir.path().join(EVENT_LOG_FILE),
            format!("{}\n{}\n", first_legacy_line, second_legacy_line),
        )
        .expect("write");

        let events = event_log.read_all().expect("read events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], first);
        assert_eq!(events[1], second);
    }

    #[test]
    fn append_rejects_missing_parent_pointer_after_first_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let first = signed_checkpoint_event(None);
        event_log.append(&first).expect("append first event");

        let second = signed_checkpoint_event(None);
        let err = event_log
            .append(&second)
            .expect_err("should reject missing parent pointer");
        assert!(
            format!("{:#}", err).contains("missing parent pointer"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn append_rejects_incorrect_parent_pointer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let first = signed_checkpoint_event(None);
        event_log.append(&first).expect("append first event");

        let second = signed_checkpoint_event(Some(Uuid::new_v4()));
        let err = event_log
            .append(&second)
            .expect_err("should reject invalid parent pointer");
        assert!(
            format!("{:#}", err).contains("expected"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_all_rejects_broken_parent_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let first = signed_checkpoint_event(None);
        let second = signed_checkpoint_event(Some(Uuid::new_v4()));
        let first_line = serde_json::to_string(&EventRecord::from_event(&first))
            .expect("serialize first versioned event");
        let second_line = serde_json::to_string(&EventRecord::from_event(&second))
            .expect("serialize second versioned event");
        fs::write(
            dir.path().join(EVENT_LOG_FILE),
            format!("{}\n{}\n", first_line, second_line),
        )
        .expect("write");

        let err = event_log
            .read_all()
            .expect_err("should reject causal chain");
        assert!(
            format!("{:#}", err).contains("invalid causal chain"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_all_allows_previous_schema_without_parent_pointers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let first = sample_checkpoint_event(None);
        let second = sample_checkpoint_event(None);
        let first_line = json!({
            "schema_version": 1,
            "event": first,
        });
        let second_line = json!({
            "schema_version": 1,
            "event": second,
        });
        fs::write(
            dir.path().join(EVENT_LOG_FILE),
            format!("{}\n{}\n", first_line, second_line),
        )
        .expect("write");

        let events = event_log.read_all().expect("read events");
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn read_all_rejects_unknown_schema_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let event = signed_checkpoint_event(None);
        let unsupported = json!({
            "schema_version": CURRENT_EVENT_SCHEMA_VERSION + 1,
            "event": event,
        });
        fs::write(
            dir.path().join(EVENT_LOG_FILE),
            format!("{}\n", unsupported),
        )
        .expect("write");

        let err = event_log.read_all().expect_err("should reject schema");
        assert!(
            format!("{:#}", err).contains("unsupported event schema version"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn append_rejects_missing_signature_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let event = sample_checkpoint_event(None);
        let err = event_log
            .append(&event)
            .expect_err("should reject unsigned event");
        assert!(
            format!("{:#}", err).contains("missing ed25519 signature"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_all_rejects_invalid_signature() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let mut event = signed_checkpoint_event(None);
        event.actor = "tampered".to_string();
        let line =
            serde_json::to_string(&EventRecord::from_event(&event)).expect("serialize event");
        fs::write(dir.path().join(EVENT_LOG_FILE), format!("{}\n", line)).expect("write");

        let err = event_log
            .read_all()
            .expect_err("should reject invalid event signature");
        assert!(
            format!("{:#}", err).contains("failed ed25519 signature verification"),
            "unexpected error: {err}"
        );
    }

    fn sample_checkpoint_event(parent_id: Option<Uuid>) -> Event {
        Event {
            id: Uuid::new_v4(),
            timestamp: "1739571600000000000".to_string(),
            actor: "tester".to_string(),
            parent_id,
            signer_public_key: None,
            signature: None,
            kind: EventKind::Checkpoint(CheckpointEvent {
                label: "checkpoint".to_string(),
                message: Some("baseline".to_string()),
                snapshot_id: Uuid::new_v4(),
                parent_checkpoint_event: None,
            }),
        }
    }

    fn signed_checkpoint_event(parent_id: Option<Uuid>) -> Event {
        let mut event = sample_checkpoint_event(parent_id);
        sign_event(&mut event);
        event
    }

    fn sign_event(event: &mut Event) {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let payload = event_signing_payload(event).expect("serialize signing payload");
        let signature = key.sign(&payload);
        event.signer_public_key = Some(hex::encode(key.verifying_key().to_bytes()));
        event.signature = Some(hex::encode(signature.to_bytes()));
    }
}
