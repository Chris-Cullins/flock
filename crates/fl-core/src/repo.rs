use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::event::{CheckpointEvent, Event, EventKind};
use crate::semantic::{SemanticFileDiff, diff as semantic_diff, supported_source};

const FLOCK_DIR: &str = ".flock";
const EVENT_LOG_DIR: &str = ".flock/event-log";
const EVENT_LOG_FILE: &str = ".flock/event-log/events.jsonl";
const SNAPSHOT_DIR: &str = ".flock/snapshots";
const CONFIG_FILE: &str = ".flock/config.toml";

#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        let start_ref = start.as_ref();
        let start = start_ref.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize path while locating repository: {}",
                start_ref.display()
            )
        })?;

        for ancestor in start.ancestors() {
            if ancestor.join(FLOCK_DIR).is_dir() {
                return Ok(Self::at(ancestor.to_path_buf()));
            }
        }

        bail!(
            "no Flock repository found from {} upward; run `fl init` first",
            start.display()
        );
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(self.root.join(EVENT_LOG_DIR))
            .context("failed to create event-log directory")?;
        fs::create_dir_all(self.root.join(SNAPSHOT_DIR))
            .context("failed to create snapshots directory")?;

        let event_log = self.root.join(EVENT_LOG_FILE);
        if !event_log.exists() {
            File::create(&event_log)
                .with_context(|| format!("failed to create {}", event_log.display()))?;
        }

        let config = self.root.join(CONFIG_FILE);
        if !config.exists() {
            let contents = [
                "mode = \"git-compatible\"",
                "semantic_default = \"typescript\"",
                "analyzers = [\"typescript\", \"javascript\"]",
            ]
            .join("\n");
            fs::write(&config, format!("{}\n", contents))
                .with_context(|| format!("failed to write {}", config.display()))?;
        }

        Ok(())
    }

    pub fn create_checkpoint(&self, message: Option<String>) -> Result<Event> {
        self.assert_initialized()?;

        let snapshot_id = Uuid::new_v4();
        let snapshot_path = self.snapshot_path(snapshot_id);
        fs::create_dir_all(&snapshot_path).with_context(|| {
            format!(
                "failed to create snapshot directory {}",
                snapshot_path.display()
            )
        })?;

        self.copy_workspace_to_snapshot(&snapshot_path)?;

        let label = message
            .as_ref()
            .map(|m| normalize_label(m))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("checkpoint-{}", snapshot_id.simple()));

        let timestamp = unix_timestamp_nanos()?;

        let event = Event {
            id: Uuid::new_v4(),
            timestamp,
            actor: env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
            kind: EventKind::Checkpoint(CheckpointEvent {
                label,
                message,
                snapshot_id,
            }),
        };

        self.append_event(&event)?;
        Ok(event)
    }

    pub fn list_events(&self) -> Result<Vec<Event>> {
        self.assert_initialized()?;

        let path = self.root.join(EVENT_LOG_FILE);
        let file =
            File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);

        let mut events = Vec::new();
        for line in reader.lines() {
            let line =
                line.with_context(|| format!("failed to read line in {}", path.display()))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let event = serde_json::from_str::<Event>(trimmed)
                .with_context(|| format!("failed to parse event JSON: {}", trimmed))?;
            events.push(event);
        }

        Ok(events)
    }

    pub fn semantic_diff_from_latest_checkpoint(&self) -> Result<Vec<SemanticFileDiff>> {
        self.assert_initialized()?;

        let checkpoint = self
            .latest_checkpoint()
            .ok_or_else(|| anyhow!("no checkpoint found; run `fl checkpoint -m \"...\"` first"))?;

        let EventKind::Checkpoint(checkpoint_payload) = checkpoint.kind;
        let snapshot_root = self.snapshot_path(checkpoint_payload.snapshot_id);

        let snapshot_files = collect_source_files(&snapshot_root, false)?;
        let current_files = collect_source_files(self.root(), true)?;

        let mut all_paths: BTreeSet<PathBuf> = snapshot_files;
        all_paths.extend(current_files);

        let mut diffs = Vec::new();
        for rel_path in all_paths {
            let before_path = snapshot_root.join(&rel_path);
            let after_path = self.root.join(&rel_path);

            let before = fs::read(&before_path).ok();
            let after = fs::read(&after_path).ok();

            let diff = semantic_diff(&rel_path, before.as_deref(), after.as_deref())?;
            if let Some(diff) = diff {
                diffs.push(diff);
            }
        }

        diffs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(diffs)
    }

    fn assert_initialized(&self) -> Result<()> {
        if !self.root.join(FLOCK_DIR).is_dir() {
            bail!(
                "{} is not initialized; run `fl init` from repository root",
                self.root.display()
            );
        }
        Ok(())
    }

    fn append_event(&self, event: &Event) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(EVENT_LOG_FILE))
            .context("failed to open event log for append")?;

        let line = serde_json::to_string(event).context("failed to serialize event")?;
        writeln!(file, "{}", line).context("failed to append event to log")?;
        Ok(())
    }

    fn latest_checkpoint(&self) -> Option<Event> {
        let events = self.list_events().ok()?;
        events
            .into_iter()
            .rev()
            .find(|event| matches!(event.kind, EventKind::Checkpoint(_)))
    }

    fn copy_workspace_to_snapshot(&self, snapshot_root: &Path) -> Result<()> {
        let walker = WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|entry| !should_skip_path(&self.root, entry.path()));

        for entry in walker {
            let entry = entry.context("failed while walking repository")?;
            let path = entry.path();

            if path == self.root {
                continue;
            }

            let rel = path
                .strip_prefix(&self.root)
                .context("failed to compute relative snapshot path")?;

            if should_skip_relative(rel) {
                continue;
            }

            let target = snapshot_root.join(rel);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target)
                    .with_context(|| format!("failed to create {}", target.display()))?;
                continue;
            }

            if entry.file_type().is_file() {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create parent directory {}", parent.display())
                    })?;
                }
                fs::copy(path, &target).with_context(|| {
                    format!("failed to copy {} -> {}", path.display(), target.display())
                })?;
            }
        }

        Ok(())
    }

    fn snapshot_path(&self, snapshot_id: Uuid) -> PathBuf {
        self.root.join(SNAPSHOT_DIR).join(snapshot_id.to_string())
    }
}

fn should_skip_path(root: &Path, path: &Path) -> bool {
    match path.strip_prefix(root) {
        Ok(rel) => should_skip_relative(rel),
        Err(_) => false,
    }
}

fn should_skip_relative(path: &Path) -> bool {
    let skip = [".git", ".flock", "target", "node_modules"];

    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        if skip.iter().any(|item| name == *item) {
            return true;
        }
    }

    false
}

fn collect_source_files(root: &Path, apply_skip: bool) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();

    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !apply_skip || !should_skip_path(root, entry.path()));

    for entry in walker {
        let entry = entry.context("failed while scanning source files")?;
        if !entry.file_type().is_file() {
            continue;
        }

        let rel = entry
            .path()
            .strip_prefix(root)
            .context("failed to compute relative source path")?;

        if apply_skip && should_skip_relative(rel) {
            continue;
        }

        if supported_source(rel) {
            files.insert(rel.to_path_buf());
        }
    }

    Ok(files)
}

fn normalize_label(message: &str) -> String {
    message
        .trim()
        .chars()
        .map(|ch| if ch.is_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_ascii_lowercase()
}

fn unix_timestamp_nanos() -> Result<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_nanos();
    Ok(nanos.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn checkpoint_and_semantic_diff_flow() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("sample.ts");
        fs::write(&file, "function add(a, b) { return a + b; }")
            .expect("seed write should succeed");

        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&file, "function add(a, b) { return a - b; }")
            .expect("update write should succeed");

        let diffs = repo
            .semantic_diff_from_latest_checkpoint()
            .expect("semantic diff should succeed");

        assert_eq!(diffs.len(), 1);
        assert!(
            diffs[0]
                .changes
                .iter()
                .any(|change| change.kind == crate::semantic::SemanticChangeKind::Modified)
        );
    }
}
