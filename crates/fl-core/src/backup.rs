use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, Component};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use uuid::Uuid;
use walkdir::WalkDir;

use fl_storage::{AutoEventLog, EventKind, FLOCK_DIR, SNAPSHOT_DIR};

/// Collect snapshot IDs referenced by checkpoint events in the event log.
fn referenced_snapshot_ids(repo_root: &Path) -> BTreeSet<Uuid> {
    let event_log = AutoEventLog::for_root(repo_root);
    let events = match event_log.read_all() {
        Ok(events) => events,
        Err(_) => return BTreeSet::new(),
    };
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Checkpoint(cp) => Some(cp.snapshot_id),
            _ => None,
        })
        .collect()
}

/// Check if a path is inside the snapshots directory and, if so, whether
/// the snapshot UUID is in the referenced set.  Returns true if the path
/// should be *skipped* (i.e. it is an orphaned snapshot).
fn is_orphaned_snapshot(path: &Path, flock_dir: &Path, referenced: &BTreeSet<Uuid>) -> bool {
    let snapshot_dir = flock_dir.join("snapshots");
    let relative = match path.strip_prefix(&snapshot_dir) {
        Ok(r) => r,
        Err(_) => return false, // not under snapshots/
    };

    // Get the first component (the snapshot UUID directory name)
    let first = match relative.components().next() {
        Some(Component::Normal(name)) => name.to_string_lossy(),
        _ => return false,
    };

    match Uuid::parse_str(&first) {
        Ok(id) => !referenced.contains(&id),
        Err(_) => false, // not a UUID directory, keep it
    }
}

/// Create a gzipped tar backup of the .flock directory.
pub fn create_backup(repo_root: &Path, output_path: &Path) -> Result<()> {
    let flock_dir = repo_root.join(FLOCK_DIR);
    if !flock_dir.is_dir() {
        bail!("no .flock directory found at {}", flock_dir.display());
    }

    // Collect referenced snapshot IDs so we skip orphaned snapshots
    let referenced = referenced_snapshot_ids(repo_root);

    let file = File::create(output_path)
        .with_context(|| format!("failed to create backup file {}", output_path.display()))?;
    let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut archive = tar::Builder::new(encoder);

    // Walk the .flock directory, skipping orphaned snapshot directories
    for entry in WalkDir::new(&flock_dir) {
        let entry = entry.with_context(|| "failed to walk .flock directory")?;
        let path = entry.path();

        if is_orphaned_snapshot(path, &flock_dir, &referenced) {
            continue;
        }

        let relative = path.strip_prefix(repo_root)
            .with_context(|| format!("failed to strip prefix from {}", path.display()))?;

        if path.is_file() {
            archive.append_path_with_name(path, relative)
                .with_context(|| format!("failed to add {} to backup", path.display()))?;
        } else if path.is_dir() && path != flock_dir {
            archive.append_dir(relative, path)
                .with_context(|| format!("failed to add dir {} to backup", path.display()))?;
        }
    }

    archive.finish().context("failed to finalize backup archive")?;
    Ok(())
}

/// Restore a backup archive to a target directory.
pub fn restore_backup(archive_path: &Path, target_dir: &Path) -> Result<()> {
    if !archive_path.exists() {
        bail!("backup archive not found: {}", archive_path.display());
    }

    let file = File::open(archive_path)
        .with_context(|| format!("failed to open backup {}", archive_path.display()))?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut archive = tar::Archive::new(decoder);

    fs::create_dir_all(target_dir)
        .with_context(|| format!("failed to create target directory {}", target_dir.display()))?;

    archive.unpack(target_dir)
        .with_context(|| format!("failed to extract backup to {}", target_dir.display()))?;

    // Verify the restored directory has a .flock dir
    let flock_dir = target_dir.join(FLOCK_DIR);
    if !flock_dir.is_dir() {
        bail!("restored backup does not contain a .flock directory");
    }

    // Remove any orphaned snapshots that aren't referenced by checkpoint events
    remove_orphaned_snapshots(target_dir)?;

    Ok(())
}

/// Remove snapshot directories not referenced by any checkpoint event.
fn remove_orphaned_snapshots(repo_root: &Path) -> Result<()> {
    let snapshot_root = repo_root.join(SNAPSHOT_DIR);
    if !snapshot_root.is_dir() {
        return Ok(());
    }

    let referenced = referenced_snapshot_ids(repo_root);

    for entry in fs::read_dir(&snapshot_root)
        .with_context(|| format!("failed to read {}", snapshot_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let snapshot_id = match Uuid::parse_str(name) {
            Ok(id) => id,
            Err(_) => continue,
        };
        if !referenced.contains(&snapshot_id) {
            fs::remove_dir_all(&path).with_context(|| {
                format!("failed to remove orphaned snapshot {}", path.display())
            })?;
        }
    }

    Ok(())
}

/// Verify a backup archive contains the expected structure.
pub fn verify_backup(archive_path: &Path) -> Result<BackupVerification> {
    if !archive_path.exists() {
        bail!("backup archive not found: {}", archive_path.display());
    }

    let file = File::open(archive_path)
        .with_context(|| format!("failed to open backup {}", archive_path.display()))?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut archive = tar::Archive::new(decoder);

    let mut has_event_log = false;
    let mut has_refs = false;
    let mut has_config = false;
    let mut file_count = 0usize;
    let mut total_size = 0u64;

    for entry in archive.entries().context("failed to read archive entries")? {
        let entry = entry.context("failed to read archive entry")?;
        let path = entry.path().context("failed to read entry path")?;
        let path_str = path.to_string_lossy();

        if path_str.contains("event-log/") || path_str.contains("event-log-segments/") {
            has_event_log = true;
        }
        if path_str.contains("refs/") {
            has_refs = true;
        }
        if path_str.ends_with("config.toml") {
            has_config = true;
        }

        file_count += 1;
        total_size += entry.size();
    }

    Ok(BackupVerification {
        file_count,
        total_size,
        has_event_log,
        has_refs,
        has_config,
    })
}

#[derive(Debug, Clone)]
pub struct BackupVerification {
    pub file_count: usize,
    pub total_size: u64,
    pub has_event_log: bool,
    pub has_refs: bool,
    pub has_config: bool,
}

impl BackupVerification {
    pub fn is_complete(&self) -> bool {
        self.has_event_log && self.has_refs && self.has_config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_and_restore_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();

        // Create minimal .flock structure
        let flock = repo_root.join(FLOCK_DIR);
        fs::create_dir_all(flock.join("event-log")).expect("mkdir event-log");
        fs::create_dir_all(flock.join("refs")).expect("mkdir refs");
        fs::write(flock.join("config.toml"), "mode = \"GitCompatible\"\n").expect("write config");
        fs::write(flock.join("event-log/events.jsonl"), "").expect("write events");
        fs::write(flock.join("refs/refs.json"), "[]").expect("write refs");

        // Create backup
        let backup_path = dir.path().join("backup.tar.gz");
        create_backup(repo_root, &backup_path).expect("create backup");
        assert!(backup_path.exists());

        // Verify backup
        let verification = verify_backup(&backup_path).expect("verify");
        assert!(verification.has_config);
        assert!(verification.has_event_log);
        assert!(verification.has_refs);
        assert!(verification.is_complete());

        // Restore to new location
        let restore_dir = tempfile::tempdir().expect("restore tempdir");
        restore_backup(&backup_path, restore_dir.path()).expect("restore");
        assert!(restore_dir.path().join(FLOCK_DIR).join("config.toml").exists());
        assert!(restore_dir.path().join(FLOCK_DIR).join("event-log/events.jsonl").exists());
        assert!(restore_dir.path().join(FLOCK_DIR).join("refs/refs.json").exists());
    }

    #[test]
    fn verify_detects_missing_components() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();

        // Create incomplete .flock structure (no refs)
        let flock = repo_root.join(FLOCK_DIR);
        fs::create_dir_all(flock.join("event-log")).expect("mkdir");
        fs::write(flock.join("config.toml"), "mode = \"GitCompatible\"\n").expect("write");
        fs::write(flock.join("event-log/events.jsonl"), "").expect("write events");

        let backup_path = dir.path().join("backup.tar.gz");
        create_backup(repo_root, &backup_path).expect("backup");

        let verification = verify_backup(&backup_path).expect("verify");
        assert!(!verification.has_refs);
        assert!(!verification.is_complete());
    }

    /// Helper: write a legacy-format checkpoint event to events.jsonl.
    /// Legacy events (no EventRecord wrapper) skip signature/merkle validation.
    fn write_legacy_checkpoint_event(flock: &Path, snapshot_id: Uuid) {
        use fl_storage::{CheckpointEvent, Event, EventKind};

        let event = Event {
            id: Uuid::new_v4(),
            timestamp: "1739571600000000000".to_string(),
            actor: "tester".to_string(),
            parent_id: None,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            exploration_id: None,
            session_id: None,
            workspace_name: None,
            kind: EventKind::Checkpoint(CheckpointEvent {
                label: "test".to_string(),
                message: None,
                snapshot_id,
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

        let line = serde_json::to_string(&event).expect("serialize event");
        fs::write(flock.join("event-log/events.jsonl"), format!("{}\n", line))
            .expect("write events");
    }

    #[test]
    fn backup_excludes_orphaned_snapshots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();
        let flock = repo_root.join(FLOCK_DIR);

        // Create .flock structure with event log referencing snapshot_a
        let snapshot_a = Uuid::new_v4();
        let snapshot_b = Uuid::new_v4(); // orphan — not in any event

        fs::create_dir_all(flock.join("event-log")).expect("mkdir event-log");
        fs::create_dir_all(flock.join("refs")).expect("mkdir refs");
        fs::write(flock.join("config.toml"), "mode = \"GitCompatible\"\n").expect("write config");
        fs::write(flock.join("refs/refs.json"), "[]").expect("write refs");
        write_legacy_checkpoint_event(&flock, snapshot_a);

        // Create both snapshot directories
        let snap_a_dir = flock.join("snapshots").join(snapshot_a.to_string());
        let snap_b_dir = flock.join("snapshots").join(snapshot_b.to_string());
        fs::create_dir_all(&snap_a_dir).expect("mkdir snapshot_a");
        fs::create_dir_all(&snap_b_dir).expect("mkdir snapshot_b");
        fs::write(snap_a_dir.join("file.txt"), "referenced").expect("write snap_a file");
        fs::write(snap_b_dir.join("file.txt"), "orphaned").expect("write snap_b file");

        // Create backup — should exclude orphaned snapshot_b
        let backup_path = dir.path().join("backup.tar.gz");
        create_backup(repo_root, &backup_path).expect("create backup");

        // Restore and verify orphaned snapshot is not present
        let restore_dir = tempfile::tempdir().expect("restore tempdir");
        restore_backup(&backup_path, restore_dir.path()).expect("restore");

        let restored_snap_a = restore_dir.path().join(FLOCK_DIR).join("snapshots").join(snapshot_a.to_string());
        let restored_snap_b = restore_dir.path().join(FLOCK_DIR).join("snapshots").join(snapshot_b.to_string());

        assert!(restored_snap_a.exists(), "referenced snapshot should be in backup");
        assert!(!restored_snap_b.exists(), "orphaned snapshot should NOT be in backup");
    }

    #[test]
    fn restore_removes_orphaned_snapshots() {
        // Test that restore cleans up orphaned snapshots even if they somehow
        // exist in the archive (e.g. from an older backup tool version)
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();
        let flock = repo_root.join(FLOCK_DIR);

        let snapshot_a = Uuid::new_v4();
        let snapshot_orphan = Uuid::new_v4();

        fs::create_dir_all(flock.join("event-log")).expect("mkdir event-log");
        fs::create_dir_all(flock.join("refs")).expect("mkdir refs");
        fs::write(flock.join("config.toml"), "mode = \"GitCompatible\"\n").expect("write config");
        fs::write(flock.join("refs/refs.json"), "[]").expect("write refs");
        write_legacy_checkpoint_event(&flock, snapshot_a);

        // Create only the referenced snapshot (no orphan) and backup
        let snap_a_dir = flock.join("snapshots").join(snapshot_a.to_string());
        fs::create_dir_all(&snap_a_dir).expect("mkdir snapshot_a");
        fs::write(snap_a_dir.join("file.txt"), "data").expect("write");

        let backup_path = dir.path().join("backup.tar.gz");
        create_backup(repo_root, &backup_path).expect("create backup");

        // Restore to a fresh dir, then manually add an orphaned snapshot
        let restore_dir = tempfile::tempdir().expect("restore tempdir");
        restore_backup(&backup_path, restore_dir.path()).expect("restore");

        // Simulate orphan by creating an unreferenced snapshot dir after restore
        let orphan_dir = restore_dir.path().join(FLOCK_DIR).join("snapshots").join(snapshot_orphan.to_string());
        fs::create_dir_all(&orphan_dir).expect("mkdir orphan");
        fs::write(orphan_dir.join("stale.txt"), "stale").expect("write stale");

        // Call remove_orphaned_snapshots directly — it should clean up
        remove_orphaned_snapshots(restore_dir.path()).expect("remove orphans");
        assert!(!orphan_dir.exists(), "orphaned snapshot should be removed");
        assert!(
            restore_dir.path().join(FLOCK_DIR).join("snapshots").join(snapshot_a.to_string()).exists(),
            "referenced snapshot should remain"
        );
    }
}
