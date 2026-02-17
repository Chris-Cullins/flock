//! Segmented event log: splits events into segment files of N events each,
//! with a B-tree index mapping event_id → (segment_number, line_number).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::event::{Event, EventRecord, compute_event_hash};
use crate::layout;

/// Number of events per segment file.
pub const SEGMENT_SIZE: usize = 10_000;

/// Index entry pointing to an event's location.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Segment number (0-based).
    pub segment: u32,
    /// Line number within the segment (0-based).
    pub line: u32,
}

/// On-disk index mapping event IDs to their segment locations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogIndex {
    pub schema_version: u32,
    /// Total number of events across all segments.
    pub event_count: usize,
    /// Number of segments.
    pub segment_count: u32,
    /// Map of event_id -> (segment, line).
    pub entries: BTreeMap<String, IndexEntry>,
    /// ID of the last event (for parent chain validation).
    pub last_event_id: Option<String>,
    /// BLAKE3 hash of the last event (for hash chain).
    #[serde(default)]
    pub last_event_hash: Option<String>,
}

const INDEX_SCHEMA_VERSION: u32 = 1;

impl EventLogIndex {
    fn new() -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            event_count: 0,
            segment_count: 0,
            entries: BTreeMap::new(),
            last_event_id: None,
            last_event_hash: None,
        }
    }
}

/// Segmented event log with O(1) append and O(log n) lookup.
pub struct SegmentedEventLog {
    segments_dir: PathBuf,
    index_path: PathBuf,
}

impl SegmentedEventLog {
    pub fn for_root(root: &Path) -> Self {
        Self {
            segments_dir: root.join(layout::EVENT_LOG_SEGMENTS_DIR),
            index_path: root.join(layout::EVENT_LOG_INDEX_FILE),
        }
    }

    /// Check if a segmented event log exists.
    pub fn exists(&self) -> bool {
        self.index_path.exists()
    }

    pub fn ensure_exists(&self) -> Result<()> {
        fs::create_dir_all(&self.segments_dir)
            .with_context(|| format!("failed to create segments directory {}", self.segments_dir.display()))?;
        if !self.index_path.exists() {
            self.write_index(&EventLogIndex::new())?;
        }
        Ok(())
    }

    /// Read all events across all segments.
    pub fn read_all(&self) -> Result<Vec<Event>> {
        let index = self.read_index()?;
        let mut events = Vec::with_capacity(index.event_count);

        for seg_num in 0..index.segment_count {
            let seg_path = self.segment_path(seg_num);
            if !seg_path.exists() {
                continue;
            }
            let file = File::open(&seg_path)
                .with_context(|| format!("failed to open segment {}", seg_path.display()))?;
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = line.with_context(|| format!("failed to read line in segment {}", seg_path.display()))?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let record: EventRecord = serde_json::from_str(trimmed)
                    .with_context(|| format!("failed to parse event in segment {}", seg_path.display()))?;
                events.push(record.event);
            }
        }

        // Validate causal chain: each event's parent_id must match the
        // previous event's id.
        let mut prev_id: Option<Uuid> = None;
        for event in &events {
            match (prev_id, event.parent_id) {
                (None, None) => {}
                (None, Some(parent)) => bail!(
                    "invalid causal chain: first event {} points to parent {}",
                    event.id, parent
                ),
                (Some(expected), Some(parent)) if parent == expected => {}
                (Some(expected), Some(parent)) => bail!(
                    "invalid causal chain: event {} points to {}, expected {}",
                    event.id, parent, expected
                ),
                (Some(expected), None) => bail!(
                    "invalid causal chain: event {} is missing parent pointer (expected {})",
                    event.id, expected
                ),
            }
            prev_id = Some(event.id);
        }

        // Validate hash chain integrity.
        let mut prev_hash: Option<String> = None;
        for event in &events {
            if let Some(ref expected_hash) = event.prev_event_hash {
                match &prev_hash {
                    Some(actual) if actual == expected_hash => {}
                    Some(actual) => bail!(
                        "hash chain broken at event {}: expected {}, got {}",
                        event.id, expected_hash, actual
                    ),
                    None => bail!(
                        "hash chain broken at event {}: has prev_event_hash but is the first event",
                        event.id
                    ),
                }
            }
            prev_hash = Some(compute_event_hash(event));
        }

        Ok(events)
    }

    /// Read events starting from a given index (0-based event number).
    pub fn read_from(&self, start: usize) -> Result<Vec<Event>> {
        let index = self.read_index()?;
        if start >= index.event_count {
            return Ok(Vec::new());
        }

        let start_segment = (start / SEGMENT_SIZE) as u32;
        let start_line = start % SEGMENT_SIZE;
        let mut events = Vec::new();

        for seg_num in start_segment..index.segment_count {
            let seg_path = self.segment_path(seg_num);
            if !seg_path.exists() {
                continue;
            }
            let file = File::open(&seg_path)
                .with_context(|| format!("failed to open segment {}", seg_path.display()))?;
            let reader = BufReader::new(file);
            for (line_num, line) in reader.lines().enumerate() {
                let line = line?;
                // Skip lines before start position in the first segment
                if seg_num == start_segment && line_num < start_line {
                    continue;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let record: EventRecord = serde_json::from_str(trimmed)
                    .with_context(|| "failed to parse event")?;
                events.push(record.event);
            }
        }

        Ok(events)
    }

    /// Append a batch of events from a remote pull.
    ///
    /// The first event's `parent_id` may not match the local log's tail because
    /// local-only events (e.g. RemoteSync) may have been appended after the
    /// last synced event. The internal chain of pulled events is still validated.
    pub fn append_batch_for_pull(&self, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        // Validate internal chain of pulled events (skip first event's parent check).
        let mut prev_id: Option<Uuid> = None;
        for (i, event) in events.iter().enumerate() {
            if i > 0 {
                match (prev_id, event.parent_id) {
                    (Some(expected), Some(parent)) if parent == expected => {}
                    (Some(expected), Some(parent)) => bail!(
                        "invalid causal chain in pulled events: event {} points to {}, expected {}",
                        event.id, parent, expected
                    ),
                    (Some(expected), None) => bail!(
                        "invalid causal chain in pulled events: event {} is missing parent (expected {})",
                        event.id, expected
                    ),
                    _ => {}
                }
            }
            prev_id = Some(event.id);
        }

        // Append each event, skipping parent_id validation against local tail.
        for event in events {
            self.append_relaxed(event)?;
        }
        Ok(())
    }

    /// Append a single event without validating parent_id against the local
    /// log's last event. Used during pull when local-only events create a gap.
    fn append_relaxed(&self, event: &Event) -> Result<()> {
        let mut index = self.read_index()?;

        // Determine which segment to write to
        let segment_num = (index.event_count / SEGMENT_SIZE) as u32;
        let line_num = (index.event_count % SEGMENT_SIZE) as u32;

        // Create new segment if needed
        if segment_num >= index.segment_count {
            index.segment_count = segment_num + 1;
        }

        // Set hash chain link
        let mut event = event.clone();
        event.prev_event_hash = index.last_event_hash.clone();

        // Write event to segment file
        let seg_path = self.segment_path(segment_num);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&seg_path)
            .with_context(|| format!("failed to open segment {} for append", seg_path.display()))?;

        let record = EventRecord::from_event(&event);
        let line = serde_json::to_string(&record).context("failed to serialize event")?;
        writeln!(file, "{}", line).context("failed to append event to segment")?;

        // Update index
        index.event_count += 1;
        index.last_event_id = Some(event.id.to_string());
        index.last_event_hash = Some(compute_event_hash(&event));
        if line_num == 0 && segment_num > 0 {
            // New segment boundary
        }
        self.write_index(&index)?;

        Ok(())
    }

    /// Return the ID of the last event via the index (O(1)), or `None` if empty.
    pub fn latest_event_id(&self) -> Result<Option<Uuid>> {
        let index = self.read_index()?;
        match index.last_event_id {
            Some(id_str) => Ok(Some(id_str.parse().context("invalid UUID in segmented index")?)),
            None => Ok(None),
        }
    }

    /// Append an event. Validates parent chain via index (O(1)).
    pub fn append(&self, event: &Event) -> Result<()> {
        let mut index = self.read_index()?;

        // Validate parent chain
        match (index.last_event_id.as_deref(), event.parent_id) {
            (None, None) => {}
            (None, Some(parent)) => bail!(
                "invalid causal chain: first event {} cannot reference parent {}",
                event.id, parent
            ),
            (Some(expected), Some(parent)) if parent.to_string() == expected => {}
            (Some(expected), Some(parent)) => bail!(
                "invalid causal chain: event {} points to {}, expected {}",
                event.id, parent, expected
            ),
            (Some(expected), None) => bail!(
                "invalid causal chain: event {} is missing parent pointer (expected {})",
                event.id, expected
            ),
        }

        // Determine which segment to write to
        let segment_num = (index.event_count / SEGMENT_SIZE) as u32;
        let line_num = (index.event_count % SEGMENT_SIZE) as u32;

        // Create new segment if needed
        if segment_num >= index.segment_count {
            index.segment_count = segment_num + 1;
        }

        // Set hash chain link
        let mut event = event.clone();
        event.prev_event_hash = index.last_event_hash.clone();

        // Write event to segment file
        let seg_path = self.segment_path(segment_num);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&seg_path)
            .with_context(|| format!("failed to open segment {} for append", seg_path.display()))?;

        let record = EventRecord::from_event(&event);
        let line = serde_json::to_string(&record).context("failed to serialize event")?;
        writeln!(file, "{}", line).context("failed to append event to segment")?;

        // Update index
        index.entries.insert(
            event.id.to_string(),
            IndexEntry {
                segment: segment_num,
                line: line_num,
            },
        );
        index.event_count += 1;
        index.last_event_id = Some(event.id.to_string());
        index.last_event_hash = Some(compute_event_hash(&event));

        self.write_index(&index)?;

        Ok(())
    }

    /// Look up an event by ID.
    pub fn lookup(&self, event_id: Uuid) -> Result<Option<Event>> {
        let index = self.read_index()?;
        let entry = match index.entries.get(&event_id.to_string()) {
            Some(e) => e,
            None => return Ok(None),
        };

        let seg_path = self.segment_path(entry.segment);
        let file = File::open(&seg_path)
            .with_context(|| format!("failed to open segment {}", seg_path.display()))?;
        let reader = BufReader::new(file);

        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            if line_num as u32 == entry.line {
                let record: EventRecord = serde_json::from_str(line.trim())
                    .with_context(|| "failed to parse event")?;
                return Ok(Some(record.event));
            }
        }

        Ok(None)
    }

    /// Get total event count from the index (O(1)).
    pub fn event_count(&self) -> Result<usize> {
        let index = self.read_index()?;
        Ok(index.event_count)
    }

    /// Get the last event ID from the index (O(1)).
    pub fn last_event_id(&self) -> Result<Option<Uuid>> {
        let index = self.read_index()?;
        match index.last_event_id {
            Some(id) => Ok(Some(Uuid::parse_str(&id).context("invalid UUID in index")?)),
            None => Ok(None),
        }
    }

    fn read_index(&self) -> Result<EventLogIndex> {
        if !self.index_path.exists() {
            return Ok(EventLogIndex::new());
        }
        let json = fs::read_to_string(&self.index_path)
            .with_context(|| format!("failed to read event log index {}", self.index_path.display()))?;
        serde_json::from_str(&json)
            .with_context(|| format!("failed to parse event log index {}", self.index_path.display()))
    }

    fn write_index(&self, index: &EventLogIndex) -> Result<()> {
        let json = serde_json::to_string_pretty(index)
            .context("failed to serialize event log index")?;
        // Write to temp file then rename for atomicity
        let tmp_path = self.index_path.with_extension("json.tmp");
        fs::write(&tmp_path, &json)
            .with_context(|| format!("failed to write temp index {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &self.index_path)
            .with_context(|| format!("failed to rename temp index to {}", self.index_path.display()))
    }

    fn segment_path(&self, segment_num: u32) -> PathBuf {
        self.segments_dir.join(format!("{:06}.jsonl", segment_num))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CheckpointEvent, EventKind};
    use tempfile::tempdir;

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

    #[test]
    fn append_and_read_all() {
        let dir = tempdir().unwrap();
        let log = SegmentedEventLog::for_root(dir.path());
        log.ensure_exists().unwrap();

        let e1 = make_event(1, None);
        let e2 = make_event(2, Some(e1.id));
        let e3 = make_event(3, Some(e2.id));

        log.append(&e1).unwrap();
        log.append(&e2).unwrap();
        log.append(&e3).unwrap();

        let events = log.read_all().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].id, e1.id);
        assert_eq!(events[2].id, e3.id);
    }

    #[test]
    fn append_validates_parent_chain() {
        let dir = tempdir().unwrap();
        let log = SegmentedEventLog::for_root(dir.path());
        log.ensure_exists().unwrap();

        let e1 = make_event(1, None);
        log.append(&e1).unwrap();

        // Wrong parent
        let e2 = make_event(2, Some(Uuid::from_u128(999)));
        assert!(log.append(&e2).is_err());
    }

    #[test]
    fn lookup_by_id() {
        let dir = tempdir().unwrap();
        let log = SegmentedEventLog::for_root(dir.path());
        log.ensure_exists().unwrap();

        let e1 = make_event(1, None);
        let e2 = make_event(2, Some(e1.id));
        log.append(&e1).unwrap();
        log.append(&e2).unwrap();

        let found = log.lookup(e2.id).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, e2.id);

        assert!(log.lookup(Uuid::from_u128(999)).unwrap().is_none());
    }

    #[test]
    fn read_from_offset() {
        let dir = tempdir().unwrap();
        let log = SegmentedEventLog::for_root(dir.path());
        log.ensure_exists().unwrap();

        let e1 = make_event(1, None);
        let e2 = make_event(2, Some(e1.id));
        let e3 = make_event(3, Some(e2.id));
        log.append(&e1).unwrap();
        log.append(&e2).unwrap();
        log.append(&e3).unwrap();

        let events = log.read_from(1).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, e2.id);
    }

    #[test]
    fn read_all_detects_broken_causal_chain() {
        let dir = tempdir().unwrap();
        let log = SegmentedEventLog::for_root(dir.path());
        log.ensure_exists().unwrap();

        // Write two valid events
        let e1 = make_event(1, None);
        let e2 = make_event(2, Some(e1.id));
        log.append(&e1).unwrap();
        log.append(&e2).unwrap();

        // Manually append a broken event (wrong parent) directly to the
        // segment file, bypassing validation in append().
        let e3_broken = make_event(3, Some(Uuid::from_u128(999)));
        let seg_path = log.segment_path(0);
        let record = EventRecord::from_event(&e3_broken);
        let line = serde_json::to_string(&record).unwrap();
        let mut file = OpenOptions::new().append(true).open(&seg_path).unwrap();
        writeln!(file, "{}", line).unwrap();

        let err = log.read_all().expect_err("should detect broken chain");
        let msg = format!("{}", err);
        assert!(msg.contains("invalid causal chain"), "unexpected error: {}", msg);
    }

    #[test]
    fn event_count_and_last_id() {
        let dir = tempdir().unwrap();
        let log = SegmentedEventLog::for_root(dir.path());
        log.ensure_exists().unwrap();

        assert_eq!(log.event_count().unwrap(), 0);
        assert!(log.last_event_id().unwrap().is_none());

        let e1 = make_event(1, None);
        log.append(&e1).unwrap();

        assert_eq!(log.event_count().unwrap(), 1);
        assert_eq!(log.last_event_id().unwrap(), Some(e1.id));
    }
}
