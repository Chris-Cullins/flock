use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use uuid::Uuid;

use crate::event::{CURRENT_EVENT_SCHEMA_VERSION, Event, EventRecord, compute_event_hash, event_signing_payload};
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
        validate_checkpoint_metadata(&parsed_events)?;
        validate_hash_chain(&parsed_events)?;

        let events = parsed_events
            .into_iter()
            .map(|parsed| parsed.event)
            .collect();
        Ok(events)
    }

    pub fn append(&self, event: &Event) -> Result<()> {
        self.ensure_exists()?;
        let existing = self.read_all()?;
        let expected_parent = existing.last().map(|entry| entry.id);
        validate_next_parent(event, expected_parent)?;
        verify_event_signature(event, true)?;
        verify_checkpoint_merkle_root(event, true)?;

        // Compute hash chain link
        let mut event = event.clone();
        event.prev_event_hash = existing.last().map(|prev| compute_event_hash(prev));

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {} for append", self.path.display()))?;

        let record = EventRecord::from_event(&event);
        let line = serde_json::to_string(&record).context("failed to serialize event")?;
        writeln!(file, "{}", line).context("failed to append event to log")?;
        Ok(())
    }

    /// Append a batch of events from a remote. Validates causal chain and
    /// signatures for the entire batch before writing any events.
    pub fn append_batch(&self, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        self.ensure_exists()?;

        let existing = self.read_all()?;
        let mut expected_parent = existing.last().map(|e| e.id);

        // Pre-validate the full batch before writing anything.
        for event in events {
            validate_next_parent(event, expected_parent)?;
            verify_event_signature(event, true)?;
            verify_checkpoint_merkle_root(event, true)?;
            expected_parent = Some(event.id);
        }

        // All validated — append with hash chain links.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {} for batch append", self.path.display()))?;

        let mut prev_hash: Option<String> = existing.last().map(|e| compute_event_hash(e));
        for event in events {
            let mut event = event.clone();
            event.prev_event_hash = prev_hash;
            prev_hash = Some(compute_event_hash(&event));
            let record = EventRecord::from_event(&event);
            let line = serde_json::to_string(&record).context("failed to serialize event")?;
            writeln!(file, "{}", line).context("failed to append event to log")?;
        }

        Ok(())
    }

    /// Append a batch of events from a remote pull. Signatures are not
    /// required — graft events from shallow clones may have their signature
    /// stripped when the parent_id is cleared.
    pub fn append_batch_from_pull(&self, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        self.ensure_exists()?;

        let existing = self.read_all()?;
        let mut expected_parent = existing.last().map(|e| e.id);

        // Pre-validate with relaxed signature checking.
        for event in events {
            validate_next_parent(event, expected_parent)?;
            verify_event_signature(event, false)?; // signatures optional
            verify_checkpoint_merkle_root(event, true)?;
            expected_parent = Some(event.id);
        }

        // Append with hash chain links.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {} for batch append", self.path.display()))?;

        let mut prev_hash: Option<String> = existing.last().map(|e| compute_event_hash(e));
        for event in events {
            let mut event = event.clone();
            event.prev_event_hash = prev_hash;
            prev_hash = Some(compute_event_hash(&event));
            let record = EventRecord::from_event(&event);
            let line = serde_json::to_string(&record).context("failed to serialize event")?;
            writeln!(file, "{}", line).context("failed to append event to log")?;
        }

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

fn validate_checkpoint_metadata(events: &[ParsedEvent]) -> Result<()> {
    for parsed in events {
        let require_merkle_root = parsed.schema_version.unwrap_or(1) >= 5;
        verify_checkpoint_merkle_root(&parsed.event, require_merkle_root)?;
    }
    Ok(())
}

fn validate_hash_chain(events: &[ParsedEvent]) -> Result<()> {
    let mut prev_hash: Option<String> = None;
    for parsed in events {
        let require_hash = parsed.schema_version.unwrap_or(1) >= 13;
        match (&prev_hash, &parsed.event.prev_event_hash) {
            (None, None) => {}
            (None, Some(h)) if !h.is_empty() => {
                bail!(
                    "hash chain error: first event {} has prev_event_hash but no predecessor",
                    parsed.event.id
                );
            }
            (Some(expected), Some(actual)) if actual == expected => {}
            (Some(expected), Some(actual)) => {
                bail!(
                    "hash chain error: event {} prev_event_hash mismatch (expected {}, got {})",
                    parsed.event.id,
                    expected,
                    actual
                );
            }
            (Some(_), None) if require_hash => {
                bail!(
                    "hash chain error: event {} is missing prev_event_hash (schema >= 13)",
                    parsed.event.id
                );
            }
            _ => {}
        }
        prev_hash = Some(compute_event_hash(&parsed.event));
    }
    Ok(())
}

fn verify_checkpoint_merkle_root(event: &Event, require_merkle_root: bool) -> Result<()> {
    let crate::event::EventKind::Checkpoint(checkpoint) = &event.kind else {
        return Ok(());
    };

    match checkpoint.snapshot_merkle_root.as_deref() {
        Some(root) => {
            if root.len() != 64 {
                bail!(
                    "event {} has invalid checkpoint merkle root length (expected 64 hex chars)",
                    event.id
                );
            }
            let decoded = hex::decode(root).with_context(|| {
                format!(
                    "event {} has invalid checkpoint merkle root encoding",
                    event.id
                )
            })?;
            if decoded.len() != 32 {
                bail!(
                    "event {} has invalid checkpoint merkle root byte length (expected 32)",
                    event.id
                );
            }
            Ok(())
        }
        None if require_merkle_root => bail!(
            "event {} is missing checkpoint merkle root metadata",
            event.id
        ),
        None => Ok(()),
    }
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
    fn append_rejects_missing_checkpoint_merkle_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let mut event = sample_checkpoint_event(None);
        sign_event(&mut event);
        let err = event_log
            .append(&event)
            .expect_err("should reject checkpoint without merkle root");
        assert!(
            format!("{:#}", err).contains("missing checkpoint merkle root"),
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
            prev_event_hash: None,
            kind: EventKind::Checkpoint(CheckpointEvent {
                label: "checkpoint".to_string(),
                message: Some("baseline".to_string()),
                snapshot_id: Uuid::new_v4(),
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

    fn signed_checkpoint_event(parent_id: Option<Uuid>) -> Event {
        let mut event = sample_checkpoint_event(parent_id);
        if let EventKind::Checkpoint(checkpoint) = &mut event.kind {
            checkpoint.snapshot_merkle_root = Some("0".repeat(64));
        }
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

    #[test]
    fn append_sets_hash_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let first = signed_checkpoint_event(None);
        event_log.append(&first).expect("append first");

        let second = signed_checkpoint_event(Some(first.id));
        event_log.append(&second).expect("append second");

        // Read back and verify hash chain is set
        let events = event_log.read_all().expect("read all");
        assert_eq!(events.len(), 2);
        assert!(events[0].prev_event_hash.is_none());
        assert!(events[1].prev_event_hash.is_some());

        // Verify the hash matches
        let expected_hash = crate::event::compute_event_hash(&events[0]);
        assert_eq!(events[1].prev_event_hash.as_deref(), Some(expected_hash.as_str()));
    }

    #[test]
    fn read_all_detects_tampered_hash_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let event_log = EventLog::for_root(dir.path());
        event_log.ensure_exists().expect("ensure event log");

        let first = signed_checkpoint_event(None);
        event_log.append(&first).expect("append first");
        let second = signed_checkpoint_event(Some(first.id));
        event_log.append(&second).expect("append second");

        // Tamper with the hash chain by writing a bad prev_event_hash
        let events = event_log.read_all().expect("read all");
        let mut tampered = events[1].clone();
        tampered.prev_event_hash = Some("bad_hash".to_string());
        let first_record = EventRecord::from_event(&events[0]);
        let tampered_record = EventRecord::from_event(&tampered);
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&first_record).unwrap(),
            serde_json::to_string(&tampered_record).unwrap()
        );
        fs::write(dir.path().join(EVENT_LOG_FILE), content).expect("write tampered log");

        let err = event_log
            .read_all()
            .expect_err("should detect hash chain tamper");
        assert!(
            format!("{:#}", err).contains("hash chain error"),
            "unexpected error: {err}"
        );
    }
}
