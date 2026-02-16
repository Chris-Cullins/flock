use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::Path;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use walkdir::WalkDir;

use fl_storage::FLOCK_DIR;

/// Create a gzipped tar backup of the .flock directory.
pub fn create_backup(repo_root: &Path, output_path: &Path) -> Result<()> {
    let flock_dir = repo_root.join(FLOCK_DIR);
    if !flock_dir.is_dir() {
        bail!("no .flock directory found at {}", flock_dir.display());
    }

    let file = File::create(output_path)
        .with_context(|| format!("failed to create backup file {}", output_path.display()))?;
    let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut archive = tar::Builder::new(encoder);

    // Walk the .flock directory and add all files
    for entry in WalkDir::new(&flock_dir) {
        let entry = entry.with_context(|| "failed to walk .flock directory")?;
        let path = entry.path();
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
}
