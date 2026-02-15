use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::event::{
    CheckpointEvent, Event, EventKind, ExplorationAction, ExplorationEvent, GitBridgeAction,
    GitBridgeEvent, UndoEvent, UndoMode,
};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplorationStatus {
    Active,
    Promoted,
    Abandoned,
}

impl fmt::Display for ExplorationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Promoted => f.write_str("promoted"),
            Self::Abandoned => f.write_str("abandoned"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExplorationSummary {
    pub id: Uuid,
    pub title: String,
    pub status: ExplorationStatus,
    pub base_checkpoint_event: Option<Uuid>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub enum UndoRequest {
    Last,
    N(usize),
    To(String),
    Since(Duration),
}

#[derive(Debug, Clone)]
pub struct UndoResult {
    pub target_event_id: Uuid,
    pub restored_checkpoint_event: Option<Uuid>,
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
        let label = message
            .as_deref()
            .map(normalize_label)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("checkpoint-{}", Uuid::new_v4().simple()));

        self.create_checkpoint_with_label(label, message)
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

        let EventKind::Checkpoint(checkpoint_payload) = checkpoint.kind else {
            bail!("latest checkpoint event had unexpected kind")
        };

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

    pub fn start_exploration(&self, title: String) -> Result<ExplorationSummary> {
        self.assert_initialized()?;

        let id = Uuid::new_v4();
        let timestamp = unix_timestamp_nanos()?;
        let base_checkpoint_event = self.latest_checkpoint().map(|event| event.id);

        let event = Event {
            id: Uuid::new_v4(),
            timestamp: timestamp.clone(),
            actor: current_actor(),
            kind: EventKind::Exploration(ExplorationEvent {
                exploration_id: id,
                title: title.clone(),
                base_checkpoint_event,
                action: ExplorationAction::Start,
            }),
        };

        self.append_event(&event)?;

        Ok(ExplorationSummary {
            id,
            title,
            status: ExplorationStatus::Active,
            base_checkpoint_event,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        })
    }

    pub fn list_explorations(&self) -> Result<Vec<ExplorationSummary>> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let mut entries: Vec<ExplorationSummary> =
            replay_explorations(&events).into_values().collect();

        entries.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.title.cmp(&b.title))
        });

        Ok(entries)
    }

    pub fn promote_exploration(&self, id: Uuid) -> Result<ExplorationSummary> {
        self.assert_initialized()?;

        let existing = self
            .list_explorations()?
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow!("exploration {} not found", id))?;

        if existing.status != ExplorationStatus::Active {
            bail!(
                "exploration {} is not active (current status: {})",
                id,
                existing.status
            );
        }

        let message = format!("promote exploration {}", existing.title);
        self.create_checkpoint_with_label(
            format!("promote-{}", normalize_label(&existing.title)),
            Some(message),
        )?;

        self.append_event(&Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            kind: EventKind::Exploration(ExplorationEvent {
                exploration_id: id,
                title: existing.title.clone(),
                base_checkpoint_event: existing.base_checkpoint_event,
                action: ExplorationAction::Promote,
            }),
        })?;

        self.list_explorations()?
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow!("failed to reload exploration {}", id))
    }

    pub fn abandon_exploration(&self, id: Uuid) -> Result<ExplorationSummary> {
        self.assert_initialized()?;

        let existing = self
            .list_explorations()?
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow!("exploration {} not found", id))?;

        if existing.status != ExplorationStatus::Active {
            bail!(
                "exploration {} is not active (current status: {})",
                id,
                existing.status
            );
        }

        self.append_event(&Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            kind: EventKind::Exploration(ExplorationEvent {
                exploration_id: id,
                title: existing.title.clone(),
                base_checkpoint_event: existing.base_checkpoint_event,
                action: ExplorationAction::Abandon,
            }),
        })?;

        self.list_explorations()?
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow!("failed to reload exploration {}", id))
    }

    pub fn undo(&self, request: UndoRequest) -> Result<UndoResult> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        if events.is_empty() {
            bail!("cannot undo: event log is empty")
        }

        let target = resolve_target_event(&events, &request)?;
        let mode = to_undo_mode(&request, target.id);

        let mut restored_checkpoint_event = None;

        if matches!(target.kind, EventKind::Checkpoint(_)) {
            let Some(previous_checkpoint) = previous_checkpoint_before(&events, target.id) else {
                bail!(
                    "cannot undo checkpoint {}: no earlier checkpoint exists",
                    target.id
                )
            };

            let EventKind::Checkpoint(payload) = previous_checkpoint.kind else {
                bail!("expected checkpoint payload")
            };

            self.restore_workspace_from_snapshot(payload.snapshot_id)?;

            let checkpoint_event = self.create_checkpoint_with_label(
                format!("undo-{}", target.id.simple()),
                Some(format!("undo target {}", target.id)),
            )?;
            restored_checkpoint_event = Some(checkpoint_event.id);
        }

        self.append_event(&Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            kind: EventKind::Undo(UndoEvent {
                target_event_id: target.id,
                mode,
                restored_checkpoint_event,
            }),
        })?;

        Ok(UndoResult {
            target_event_id: target.id,
            restored_checkpoint_event,
        })
    }

    pub fn git_commit_stub(&self, message: String) -> Result<String> {
        self.assert_initialized()?;
        self.assert_git_initialized()?;

        self.run_git(&["add", "-A"])?;
        let detail = self.run_git(&["commit", "-m", &message])?;

        self.append_event(&Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            kind: EventKind::GitBridge(GitBridgeEvent {
                action: GitBridgeAction::Commit,
                success: true,
                detail: detail.clone(),
            }),
        })?;

        Ok(detail)
    }

    pub fn git_push_stub(&self, remote: Option<String>, branch: Option<String>) -> Result<String> {
        self.assert_initialized()?;
        self.assert_git_initialized()?;

        let mut args = vec!["push".to_string()];
        if let Some(remote) = remote {
            args.push(remote);
        }
        if let Some(branch) = branch {
            args.push(branch);
        }

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let detail = self.run_git(&arg_refs)?;

        self.append_event(&Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            kind: EventKind::GitBridge(GitBridgeEvent {
                action: GitBridgeAction::Push,
                success: true,
                detail: detail.clone(),
            }),
        })?;

        Ok(detail)
    }

    pub fn git_pull_stub(&self, remote: Option<String>, branch: Option<String>) -> Result<String> {
        self.assert_initialized()?;
        self.assert_git_initialized()?;

        let mut args = vec!["pull".to_string()];
        if let Some(remote) = remote {
            args.push(remote);
        }
        if let Some(branch) = branch {
            args.push(branch);
        }

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let detail = self.run_git(&arg_refs)?;

        self.append_event(&Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            kind: EventKind::GitBridge(GitBridgeEvent {
                action: GitBridgeAction::Pull,
                success: true,
                detail: detail.clone(),
            }),
        })?;

        Ok(detail)
    }

    fn create_checkpoint_with_label(
        &self,
        label: String,
        message: Option<String>,
    ) -> Result<Event> {
        let snapshot_id = Uuid::new_v4();
        let snapshot_path = self.snapshot_path(snapshot_id);
        fs::create_dir_all(&snapshot_path).with_context(|| {
            format!(
                "failed to create snapshot directory {}",
                snapshot_path.display()
            )
        })?;

        self.copy_workspace_to_snapshot(&snapshot_path)?;

        let event = Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            kind: EventKind::Checkpoint(CheckpointEvent {
                label,
                message,
                snapshot_id,
            }),
        };

        self.append_event(&event)?;
        Ok(event)
    }

    fn restore_workspace_from_snapshot(&self, snapshot_id: Uuid) -> Result<()> {
        let snapshot_root = self.snapshot_path(snapshot_id);
        if !snapshot_root.is_dir() {
            bail!("snapshot {} not found", snapshot_id)
        }

        self.clear_workspace_files()?;

        let walker = WalkDir::new(&snapshot_root)
            .into_iter()
            .filter_entry(|entry| !should_skip_path(&snapshot_root, entry.path()));

        for entry in walker {
            let entry = entry.context("failed while restoring workspace")?;
            if entry.path() == snapshot_root {
                continue;
            }

            let rel = entry
                .path()
                .strip_prefix(&snapshot_root)
                .context("failed to compute restored relative path")?;

            if should_skip_relative(rel) {
                continue;
            }

            let target = self.root.join(rel);
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
                fs::copy(entry.path(), &target).with_context(|| {
                    format!(
                        "failed to restore {} -> {}",
                        entry.path().display(),
                        target.display()
                    )
                })?;
            }
        }

        Ok(())
    }

    fn clear_workspace_files(&self) -> Result<()> {
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("failed to read root directory {}", self.root.display()))?
        {
            let entry = entry.context("failed to read workspace entry")?;
            let path = entry.path();
            let rel = path
                .strip_prefix(&self.root)
                .context("failed to compute relative workspace path")?;

            if should_skip_relative(rel) {
                continue;
            }

            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to stat {}", path.display()))?;
            if metadata.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to remove directory {}", path.display()))?;
            } else {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove file {}", path.display()))?;
            }
        }
        Ok(())
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

    fn assert_git_initialized(&self) -> Result<()> {
        if !self.root.join(".git").is_dir() {
            bail!(
                "git bridge stub requires an existing .git directory in {}",
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

    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .with_context(|| format!("failed to run git {}", args.join(" ")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stdout.is_empty() {
            stderr.clone()
        } else if stderr.is_empty() {
            stdout.clone()
        } else {
            format!("{}\n{}", stdout, stderr)
        };

        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                if detail.is_empty() {
                    "(no output)"
                } else {
                    detail.as_str()
                }
            )
        }

        Ok(detail)
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

fn parse_nanos(value: &str) -> Result<u128> {
    value
        .parse::<u128>()
        .with_context(|| format!("invalid nanosecond timestamp: {}", value))
}

fn current_actor() -> String {
    env::var("USER").unwrap_or_else(|_| "unknown".to_string())
}

fn replay_explorations(events: &[Event]) -> BTreeMap<Uuid, ExplorationSummary> {
    let mut map = BTreeMap::new();

    for event in events {
        let EventKind::Exploration(exploration) = &event.kind else {
            continue;
        };

        match exploration.action {
            ExplorationAction::Start => {
                map.insert(
                    exploration.exploration_id,
                    ExplorationSummary {
                        id: exploration.exploration_id,
                        title: exploration.title.clone(),
                        status: ExplorationStatus::Active,
                        base_checkpoint_event: exploration.base_checkpoint_event,
                        created_at: event.timestamp.clone(),
                        updated_at: event.timestamp.clone(),
                    },
                );
            }
            ExplorationAction::Promote => {
                if let Some(entry) = map.get_mut(&exploration.exploration_id) {
                    entry.status = ExplorationStatus::Promoted;
                    entry.updated_at = event.timestamp.clone();
                }
            }
            ExplorationAction::Abandon => {
                if let Some(entry) = map.get_mut(&exploration.exploration_id) {
                    entry.status = ExplorationStatus::Abandoned;
                    entry.updated_at = event.timestamp.clone();
                }
            }
        }
    }

    map
}

fn resolve_target_event<'a>(events: &'a [Event], request: &UndoRequest) -> Result<&'a Event> {
    match request {
        UndoRequest::Last => events.last().ok_or_else(|| anyhow!("event log is empty")),
        UndoRequest::N(n) => {
            if *n == 0 {
                bail!("--n must be >= 1")
            }
            if *n > events.len() {
                bail!(
                    "cannot undo {} events: only {} events exist",
                    n,
                    events.len()
                )
            }
            let idx = events.len() - *n;
            Ok(&events[idx])
        }
        UndoRequest::To(raw_id) => {
            let matches: Vec<&Event> = events
                .iter()
                .filter(|event| event.id.to_string().starts_with(raw_id))
                .collect();

            match matches.as_slice() {
                [] => bail!("no event id matches `{}`", raw_id),
                [event] => Ok(*event),
                _ => bail!("event id prefix `{}` is ambiguous", raw_id),
            }
        }
        UndoRequest::Since(duration) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock is before unix epoch")?
                .as_nanos();
            let cutoff = now.saturating_sub(duration.as_nanos());

            events
                .iter()
                .find(|event| {
                    parse_nanos(&event.timestamp)
                        .map(|ts| ts >= cutoff)
                        .unwrap_or(false)
                })
                .ok_or_else(|| {
                    anyhow!(
                        "no events found in the last {} seconds",
                        duration.as_secs_f64()
                    )
                })
        }
    }
}

fn to_undo_mode(request: &UndoRequest, resolved_target_id: Uuid) -> UndoMode {
    match request {
        UndoRequest::Last => UndoMode::Last,
        UndoRequest::N(n) => UndoMode::N(*n),
        UndoRequest::To(_) => UndoMode::To(resolved_target_id),
        UndoRequest::Since(duration) => UndoMode::SinceNanos(duration.as_nanos()),
    }
}

fn previous_checkpoint_before(events: &[Event], target_event_id: Uuid) -> Option<Event> {
    let mut previous = None;

    for event in events {
        if event.id == target_event_id {
            break;
        }

        if matches!(event.kind, EventKind::Checkpoint(_)) {
            previous = Some(event.clone());
        }
    }

    previous
}

pub fn parse_duration_spec(input: &str) -> Result<Duration> {
    let value = input.trim();
    if value.is_empty() {
        bail!("duration cannot be empty")
    }

    let split_at = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    let (digits, unit) = value.split_at(split_at);

    if digits.is_empty() {
        bail!("invalid duration `{}`", input)
    }

    let amount = digits
        .parse::<u64>()
        .with_context(|| format!("invalid duration amount `{}`", digits))?;

    let duration = match unit {
        "" | "s" => Duration::from_secs(amount),
        "m" => Duration::from_secs(amount.saturating_mul(60)),
        "h" => Duration::from_secs(amount.saturating_mul(60 * 60)),
        "d" => Duration::from_secs(amount.saturating_mul(60 * 60 * 24)),
        _ => bail!("unsupported duration unit `{}` (use s, m, h, d)", unit),
    };

    Ok(duration)
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

    #[test]
    fn exploration_lifecycle_flow() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let started = repo
            .start_exploration("new-parser".to_string())
            .expect("exploration should start");
        assert_eq!(started.status, ExplorationStatus::Active);

        let promoted = repo
            .promote_exploration(started.id)
            .expect("exploration should promote");
        assert_eq!(promoted.status, ExplorationStatus::Promoted);
    }

    #[test]
    fn undo_last_checkpoint_restores_previous_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("undo.ts");
        fs::write(&file, "const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&file, "const value = 2;").expect("write should succeed");
        repo.create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");

        repo.undo(UndoRequest::Last).expect("undo should succeed");

        let current = fs::read_to_string(&file).expect("read should succeed");
        assert!(current.contains("const value = 1"));
    }

    #[test]
    fn parse_duration_specs() {
        assert_eq!(parse_duration_spec("5m").expect("duration").as_secs(), 300);
        assert_eq!(parse_duration_spec("30").expect("duration").as_secs(), 30);
        assert!(parse_duration_spec("1w").is_err());
    }
}
