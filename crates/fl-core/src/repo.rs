use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::{Signer, SigningKey};
use fl_storage::{
    CONFIG_FILE, FLOCK_DIR, KEY_DIR, RefKind, RefStore, RepoRef, SIGNING_KEY_FILE, SNAPSHOT_DIR,
    WorkspaceRefConfig,
};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::event::{
    CheckpointEvent, Event, EventKind, ExplorationAction, ExplorationEvent, GitBridgeAction,
    GitBridgeEvent, UndoEvent,
};
use crate::semantic::{SemanticFileDiff, diff as semantic_diff, supported_source};
use fl_workflow::{previous_checkpoint_before, replay_state, resolve_target_event, to_undo_mode};

pub use fl_workflow::parse_duration_spec;
pub use fl_workflow::{
    ExplorationStatus, ExplorationSummary, ReplayedState, UndoRequest, UndoResult,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckReport {
    pub event_count: usize,
    pub checkpoint_count: usize,
    pub snapshot_count: usize,
    pub ref_count: usize,
}

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
        fs::create_dir_all(self.root.join(SNAPSHOT_DIR))
            .context("failed to create snapshots directory")?;

        fl_storage::EventLog::for_root(self.root()).ensure_exists()?;
        RefStore::for_root(self.root()).ensure_exists()?;
        self.ensure_signing_key()?;

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

        self.create_checkpoint_with_lineage(label, message, None)
    }

    pub fn list_events(&self) -> Result<Vec<Event>> {
        self.assert_initialized()?;
        fl_storage::EventLog::for_root(self.root()).read_all()
    }

    pub fn list_refs(&self) -> Result<Vec<RepoRef>> {
        self.assert_initialized()?;

        let store = RefStore::for_root(self.root());
        store.ensure_exists()?;
        let mut refs = store.read_all()?;
        refs.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
        Ok(refs)
    }

    pub fn upsert_ref(
        &self,
        kind: RefKind,
        name: String,
        target_event_id_prefix: String,
        auto_rebase: Option<bool>,
    ) -> Result<RepoRef> {
        self.assert_initialized()?;

        let normalized_name = normalize_ref_name(&name)?;
        let resolved_event_id = self.resolve_event_id_by_prefix(&target_event_id_prefix)?;
        let target_event = self.event_by_id(resolved_event_id)?;

        if kind == RefKind::Tag && !matches!(target_event.kind, EventKind::Checkpoint(_)) {
            bail!(
                "tag refs must target checkpoint events; event {} is not a checkpoint",
                resolved_event_id
            );
        }

        let workspace = match kind {
            RefKind::Workspace => Some(WorkspaceRefConfig {
                auto_rebase: auto_rebase.unwrap_or(false),
            }),
            RefKind::Branch | RefKind::Tag => {
                if auto_rebase.is_some() {
                    bail!("--auto-rebase is only valid for workspace refs");
                }
                None
            }
        };

        let reference = RepoRef {
            kind,
            name: normalized_name,
            target_event_id: resolved_event_id,
            workspace,
        };

        let store = RefStore::for_root(self.root());
        store.ensure_exists()?;
        store.upsert(reference.clone())?;
        Ok(reference)
    }

    pub fn delete_ref(&self, kind: RefKind, name: &str) -> Result<bool> {
        self.assert_initialized()?;

        let normalized_name = normalize_ref_name(name)?;
        let store = RefStore::for_root(self.root());
        store.ensure_exists()?;
        store.delete(kind, &normalized_name)
    }

    pub fn replay_state(&self) -> Result<ReplayedState> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        replay_state(&events)
    }

    pub fn fsck(&self) -> Result<FsckReport> {
        self.assert_initialized()?;

        let events = fl_storage::EventLog::for_root(self.root())
            .read_all()
            .context("event log integrity check failed")?;
        let event_count = events.len();

        let event_kinds: HashMap<Uuid, EventKind> = events
            .iter()
            .map(|event| (event.id, event.kind.clone()))
            .collect();

        let mut seen_checkpoints = BTreeSet::new();
        let mut checkpoint_snapshot_ids = BTreeSet::new();
        for event in &events {
            match &event.kind {
                EventKind::Checkpoint(checkpoint) => {
                    if let Some(parent) = checkpoint.parent_checkpoint_event {
                        if !seen_checkpoints.contains(&parent) {
                            bail!(
                                "checkpoint {} references unknown or non-ancestor parent checkpoint {}",
                                event.id,
                                parent
                            );
                        }
                    }

                    let snapshot_path = self.snapshot_path(checkpoint.snapshot_id);
                    let metadata = fs::metadata(&snapshot_path).with_context(|| {
                        format!(
                            "checkpoint {} references missing snapshot {}",
                            event.id,
                            snapshot_path.display()
                        )
                    })?;
                    if !metadata.is_dir() {
                        bail!(
                            "checkpoint {} snapshot path is not a directory: {}",
                            event.id,
                            snapshot_path.display()
                        );
                    }

                    let expected_merkle =
                        checkpoint.snapshot_merkle_root.as_ref().ok_or_else(|| {
                            anyhow!(
                                "checkpoint {} is missing snapshot merkle root metadata",
                                event.id
                            )
                        })?;
                    let actual_merkle =
                        compute_snapshot_merkle_root(&snapshot_path).with_context(|| {
                            format!(
                                "failed to compute merkle root for snapshot {}",
                                checkpoint.snapshot_id
                            )
                        })?;
                    if actual_merkle != *expected_merkle {
                        bail!(
                            "checkpoint {} snapshot merkle root mismatch: expected {}, actual {}",
                            event.id,
                            expected_merkle,
                            actual_merkle
                        );
                    }

                    seen_checkpoints.insert(event.id);
                    checkpoint_snapshot_ids.insert(checkpoint.snapshot_id);
                }
                EventKind::Exploration(exploration) => {
                    if let Some(base_checkpoint) = exploration.base_checkpoint_event {
                        let Some(base_kind) = event_kinds.get(&base_checkpoint) else {
                            bail!(
                                "exploration {} references missing base checkpoint {}",
                                event.id,
                                base_checkpoint
                            );
                        };
                        if !matches!(base_kind, EventKind::Checkpoint(_)) {
                            bail!(
                                "exploration {} base checkpoint {} is not a checkpoint event",
                                event.id,
                                base_checkpoint
                            );
                        }
                    }
                }
                EventKind::Undo(undo) => {
                    let Some(_target_kind) = event_kinds.get(&undo.target_event_id) else {
                        bail!(
                            "undo event {} references missing target event {}",
                            event.id,
                            undo.target_event_id
                        );
                    };

                    if let Some(restored_checkpoint) = undo.restored_checkpoint_event {
                        let Some(restored_kind) = event_kinds.get(&restored_checkpoint) else {
                            bail!(
                                "undo event {} references missing restored checkpoint {}",
                                event.id,
                                restored_checkpoint
                            );
                        };
                        if !matches!(restored_kind, EventKind::Checkpoint(_)) {
                            bail!(
                                "undo event {} restored checkpoint {} is not a checkpoint event",
                                event.id,
                                restored_checkpoint
                            );
                        }
                    }
                }
                EventKind::GitBridge(_) => {}
            }
        }

        let snapshot_root = self.root.join(SNAPSHOT_DIR);
        if !snapshot_root.is_dir() {
            bail!("snapshots directory missing: {}", snapshot_root.display());
        }

        let mut snapshot_count = 0usize;
        for entry in fs::read_dir(&snapshot_root)
            .with_context(|| format!("failed to read {}", snapshot_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", snapshot_root.display()))?;
            let path = entry.path();
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to stat {}", path.display()))?;

            if !metadata.is_dir() {
                bail!(
                    "unexpected non-directory entry in snapshots directory: {}",
                    path.display()
                );
            }

            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow!("invalid snapshot directory name: {}", path.display()))?;
            let snapshot_id = Uuid::parse_str(name).with_context(|| {
                format!("snapshot directory name is not a UUID: {}", path.display())
            })?;

            if !checkpoint_snapshot_ids.contains(&snapshot_id) {
                bail!(
                    "snapshot {} is not referenced by any checkpoint event",
                    snapshot_id
                );
            }

            snapshot_count += 1;
        }

        let refs = RefStore::for_root(self.root())
            .read_all()
            .context("refs integrity check failed")?;
        for reference in &refs {
            let Some(target_kind) = event_kinds.get(&reference.target_event_id) else {
                bail!(
                    "ref {}:{:?} points to missing event {}",
                    reference.name,
                    reference.kind,
                    reference.target_event_id
                );
            };

            if reference.kind == RefKind::Tag && !matches!(target_kind, EventKind::Checkpoint(_)) {
                bail!(
                    "tag ref {} points to non-checkpoint event {}",
                    reference.name,
                    reference.target_event_id
                );
            }
        }

        Ok(FsckReport {
            event_count,
            checkpoint_count: seen_checkpoints.len(),
            snapshot_count,
            ref_count: refs.len(),
        })
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
        let base_checkpoint_event = self.latest_checkpoint().map(|event| event.id);
        let event = self.append_event(EventKind::Exploration(ExplorationEvent {
            exploration_id: id,
            title: title.clone(),
            base_checkpoint_event,
            action: ExplorationAction::Start,
        }))?;

        Ok(ExplorationSummary {
            id,
            title,
            status: ExplorationStatus::Active,
            base_checkpoint_event,
            created_at: event.timestamp.clone(),
            updated_at: event.timestamp,
        })
    }

    pub fn list_explorations(&self) -> Result<Vec<ExplorationSummary>> {
        self.assert_initialized()?;

        let mut entries: Vec<ExplorationSummary> =
            self.replay_state()?.explorations.into_values().collect();

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
        self.create_checkpoint_with_lineage(
            format!("promote-{}", normalize_label(&existing.title)),
            Some(message),
            None,
        )?;

        self.append_event(EventKind::Exploration(ExplorationEvent {
            exploration_id: id,
            title: existing.title.clone(),
            base_checkpoint_event: existing.base_checkpoint_event,
            action: ExplorationAction::Promote,
        }))?;

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

        self.append_event(EventKind::Exploration(ExplorationEvent {
            exploration_id: id,
            title: existing.title.clone(),
            base_checkpoint_event: existing.base_checkpoint_event,
            action: ExplorationAction::Abandon,
        }))?;

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

            let checkpoint_event = self.create_checkpoint_with_lineage(
                format!("undo-{}", target.id.simple()),
                Some(format!("undo target {}", target.id)),
                Some(previous_checkpoint.id),
            )?;
            restored_checkpoint_event = Some(checkpoint_event.id);
        }

        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id: target.id,
            mode,
            restored_checkpoint_event,
            file_scope: None,
        }))?;

        Ok(UndoResult {
            target_event_id: target.id,
            restored_checkpoint_event,
        })
    }

    pub fn undo_file(
        &self,
        request: UndoRequest,
        file_path: impl AsRef<Path>,
    ) -> Result<UndoResult> {
        self.assert_initialized()?;

        let scoped_file = self.normalize_scoped_file_path(file_path.as_ref())?;
        let scoped_file_display = scoped_file.to_string_lossy().to_string();

        let events = self.list_events()?;
        if events.is_empty() {
            bail!("cannot undo: event log is empty")
        }

        let target = resolve_target_event(&events, &request)?;
        let mode = to_undo_mode(&request, target.id);

        if !matches!(&target.kind, EventKind::Checkpoint(_)) {
            bail!(
                "file-scoped undo in git-compatible mode supports checkpoint targets only; resolved target {} is not a checkpoint",
                target.id
            );
        }

        let Some(previous_checkpoint) = previous_checkpoint_before(&events, target.id) else {
            bail!(
                "cannot undo checkpoint {} for file {}: no earlier checkpoint exists",
                target.id,
                scoped_file_display
            )
        };

        let EventKind::Checkpoint(payload) = previous_checkpoint.kind else {
            bail!("expected checkpoint payload")
        };

        self.restore_workspace_file_from_snapshot(payload.snapshot_id, &scoped_file)?;

        let checkpoint_event = self.create_checkpoint_with_lineage(
            format!("undo-file-{}", normalize_label(&scoped_file_display)),
            Some(format!(
                "undo file {} from checkpoint {}",
                scoped_file_display, target.id
            )),
            None,
        )?;
        let restored_checkpoint_event = Some(checkpoint_event.id);

        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id: target.id,
            mode,
            restored_checkpoint_event,
            file_scope: Some(scoped_file_display),
        }))?;

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

        self.append_event(EventKind::GitBridge(GitBridgeEvent {
            action: GitBridgeAction::Commit,
            success: true,
            detail: detail.clone(),
        }))?;

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

        self.append_event(EventKind::GitBridge(GitBridgeEvent {
            action: GitBridgeAction::Push,
            success: true,
            detail: detail.clone(),
        }))?;

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

        self.append_event(EventKind::GitBridge(GitBridgeEvent {
            action: GitBridgeAction::Pull,
            success: true,
            detail: detail.clone(),
        }))?;

        Ok(detail)
    }

    fn create_checkpoint_with_lineage(
        &self,
        label: String,
        message: Option<String>,
        parent_checkpoint_event: Option<Uuid>,
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
        let snapshot_merkle_root = compute_snapshot_merkle_root(&snapshot_path)?;

        let parent_checkpoint_event =
            parent_checkpoint_event.or_else(|| self.latest_checkpoint().map(|event| event.id));

        let event = self.append_event(EventKind::Checkpoint(CheckpointEvent {
            label,
            message,
            snapshot_id,
            parent_checkpoint_event,
            snapshot_merkle_root: Some(snapshot_merkle_root),
        }))?;
        self.advance_main_ref(event.id)?;
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

    fn restore_workspace_file_from_snapshot(
        &self,
        snapshot_id: Uuid,
        rel_path: &Path,
    ) -> Result<()> {
        let snapshot_root = self.snapshot_path(snapshot_id);
        if !snapshot_root.is_dir() {
            bail!("snapshot {} not found", snapshot_id)
        }

        let snapshot_file = snapshot_root.join(rel_path);
        let workspace_file = self.root.join(rel_path);

        if snapshot_file.exists() {
            let metadata = fs::metadata(&snapshot_file).with_context(|| {
                format!(
                    "failed to inspect snapshot file {}",
                    snapshot_file.display()
                )
            })?;
            if metadata.is_dir() {
                bail!(
                    "scoped undo target {} resolves to a directory in snapshot {}",
                    rel_path.display(),
                    snapshot_id
                );
            }

            if let Some(parent) = workspace_file.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create parent directory {}", parent.display())
                })?;
            }

            fs::copy(&snapshot_file, &workspace_file).with_context(|| {
                format!(
                    "failed to restore {} -> {}",
                    snapshot_file.display(),
                    workspace_file.display()
                )
            })?;
            return Ok(());
        }

        if workspace_file.exists() {
            let metadata = fs::metadata(&workspace_file).with_context(|| {
                format!(
                    "failed to inspect workspace path {}",
                    workspace_file.display()
                )
            })?;
            if metadata.is_dir() {
                fs::remove_dir_all(&workspace_file).with_context(|| {
                    format!("failed to remove directory {}", workspace_file.display())
                })?;
            } else {
                fs::remove_file(&workspace_file).with_context(|| {
                    format!("failed to remove file {}", workspace_file.display())
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

    fn append_event(&self, kind: EventKind) -> Result<Event> {
        self.ensure_signing_key()?;

        let mut event = Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            parent_id: self.latest_event_id()?,
            signer_public_key: None,
            signature: None,
            kind,
        };
        self.sign_event(&mut event)?;
        fl_storage::EventLog::for_root(self.root()).append(&event)?;
        Ok(event)
    }

    fn latest_event_id(&self) -> Result<Option<Uuid>> {
        Ok(self.list_events()?.last().map(|event| event.id))
    }

    fn latest_checkpoint(&self) -> Option<Event> {
        let events = self.list_events().ok()?;
        let state = replay_state(&events).ok()?;
        let checkpoint_id = state.latest_checkpoint_event_id?;
        events.into_iter().find(|event| event.id == checkpoint_id)
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

    fn normalize_scoped_file_path(&self, file_path: &Path) -> Result<PathBuf> {
        let rel_path = if file_path.is_absolute() {
            file_path.strip_prefix(&self.root).with_context(|| {
                format!(
                    "scoped undo path {} is outside repository root {}",
                    file_path.display(),
                    self.root.display()
                )
            })?
        } else {
            file_path
        };

        let mut normalized = PathBuf::new();
        for component in rel_path.components() {
            match component {
                Component::Normal(part) => normalized.push(part),
                Component::CurDir => {}
                _ => {
                    bail!(
                        "scoped undo path {} must stay within repository root",
                        file_path.display()
                    )
                }
            }
        }

        if normalized.as_os_str().is_empty() {
            bail!("scoped undo path cannot be empty");
        }

        if should_skip_relative(&normalized) {
            bail!(
                "scoped undo path {} is reserved and cannot be targeted",
                normalized.display()
            );
        }

        Ok(normalized)
    }

    fn run_git(&self, args: &[&str]) -> Result<String> {
        fl_bridge_git::run_git(&self.root, args)
    }

    fn event_by_id(&self, event_id: Uuid) -> Result<Event> {
        self.list_events()?
            .into_iter()
            .find(|event| event.id == event_id)
            .ok_or_else(|| anyhow!("event {} not found", event_id))
    }

    fn resolve_event_id_by_prefix(&self, prefix: &str) -> Result<Uuid> {
        let trimmed = prefix.trim();
        if trimmed.is_empty() {
            bail!("target event id cannot be empty");
        }

        let events = self.list_events()?;
        if events.is_empty() {
            bail!("cannot create refs: event log is empty");
        }

        let matches: Vec<Uuid> = events
            .iter()
            .filter(|event| event.id.to_string().starts_with(trimmed))
            .map(|event| event.id)
            .collect();

        match matches.as_slice() {
            [] => bail!("no event id matches `{}`", trimmed),
            [event_id] => Ok(*event_id),
            _ => bail!("event id prefix `{}` is ambiguous", trimmed),
        }
    }

    fn advance_main_ref(&self, checkpoint_event_id: Uuid) -> Result<()> {
        let store = RefStore::for_root(self.root());
        store.ensure_exists()?;
        store.upsert(RepoRef {
            kind: RefKind::Branch,
            name: "main".to_string(),
            target_event_id: checkpoint_event_id,
            workspace: None,
        })
    }

    fn ensure_signing_key(&self) -> Result<()> {
        let key_dir = self.root.join(KEY_DIR);
        fs::create_dir_all(&key_dir)
            .with_context(|| format!("failed to create key directory {}", key_dir.display()))?;

        let key_path = self.root.join(SIGNING_KEY_FILE);
        if key_path.exists() {
            return Ok(());
        }

        let mut secret = [0u8; 32];
        fill_random(&mut secret)?;
        let encoded = format!("{}\n", hex::encode(secret));
        fs::write(&key_path, encoded)
            .with_context(|| format!("failed to write signing key {}", key_path.display()))
    }

    fn sign_event(&self, event: &mut Event) -> Result<()> {
        let signing_key = self.load_signing_key()?;
        let payload = fl_storage::event_signing_payload(event)?;
        let signature = signing_key.sign(&payload);
        event.signer_public_key = Some(hex::encode(signing_key.verifying_key().to_bytes()));
        event.signature = Some(hex::encode(signature.to_bytes()));
        Ok(())
    }

    fn load_signing_key(&self) -> Result<SigningKey> {
        let key_path = self.root.join(SIGNING_KEY_FILE);
        let encoded = fs::read_to_string(&key_path)
            .with_context(|| format!("failed to read signing key {}", key_path.display()))?;
        let raw = hex::decode(encoded.trim())
            .with_context(|| format!("invalid signing key encoding in {}", key_path.display()))?;
        let secret = <[u8; 32]>::try_from(raw.as_slice())
            .with_context(|| format!("invalid signing key length in {}", key_path.display()))?;
        Ok(SigningKey::from_bytes(&secret))
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

fn compute_snapshot_merkle_root(snapshot_root: &Path) -> Result<String> {
    let mut leaves: Vec<(String, [u8; 32])> = Vec::new();

    for entry in WalkDir::new(snapshot_root) {
        let entry = entry.context("failed while walking snapshot for merkle hashing")?;
        if !entry.file_type().is_file() {
            continue;
        }

        let rel = entry
            .path()
            .strip_prefix(snapshot_root)
            .context("failed to compute relative snapshot path for merkle hashing")?;
        let rel_key = merkle_path_key(rel)?;

        let contents = fs::read(entry.path()).with_context(|| {
            format!(
                "failed to read snapshot file for merkle hashing: {}",
                entry.path().display()
            )
        })?;

        let mut leaf_hasher = blake3::Hasher::new();
        leaf_hasher.update(b"flock:merkle:leaf:v1");
        leaf_hasher.update(&(rel_key.len() as u64).to_le_bytes());
        leaf_hasher.update(rel_key.as_bytes());
        leaf_hasher.update(blake3::hash(&contents).as_bytes());
        leaves.push((rel_key, *leaf_hasher.finalize().as_bytes()));
    }

    leaves.sort_by(|a, b| a.0.cmp(&b.0));

    let mut nodes: Vec<[u8; 32]> = leaves.into_iter().map(|(_, hash)| hash).collect();
    if nodes.is_empty() {
        return Ok(hex::encode(
            blake3::hash(b"flock:merkle:empty:v1").as_bytes(),
        ));
    }

    while nodes.len() > 1 {
        let mut next = Vec::with_capacity(nodes.len().div_ceil(2));
        for chunk in nodes.chunks(2) {
            let left = chunk[0];
            let right = if chunk.len() == 2 { chunk[1] } else { chunk[0] };

            let mut node_hasher = blake3::Hasher::new();
            node_hasher.update(b"flock:merkle:node:v1");
            node_hasher.update(&left);
            node_hasher.update(&right);
            next.push(*node_hasher.finalize().as_bytes());
        }
        nodes = next;
    }

    Ok(hex::encode(nodes[0]))
}

fn merkle_path_key(rel_path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in rel_path.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str().ok_or_else(|| {
                    anyhow!("snapshot path {} is not valid UTF-8", rel_path.display())
                })?;
                parts.push(part);
            }
            Component::CurDir => {}
            _ => {
                bail!(
                    "snapshot path {} has unsupported path components",
                    rel_path.display()
                )
            }
        }
    }

    if parts.is_empty() {
        bail!("snapshot path cannot be empty");
    }

    Ok(parts.join("/"))
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

fn normalize_ref_name(raw_name: &str) -> Result<String> {
    let name = raw_name.trim();
    if name.is_empty() {
        bail!("ref name cannot be empty");
    }

    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        bail!("ref name `{}` has invalid slash placement", name);
    }

    let valid = name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' || ch == '/');
    if !valid {
        bail!(
            "ref name `{}` contains unsupported characters (allowed: A-Z, a-z, 0-9, -, _, ., /)",
            name
        );
    }

    Ok(name.to_string())
}

fn unix_timestamp_nanos() -> Result<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_nanos();
    Ok(nanos.to_string())
}

fn current_actor() -> String {
    env::var("USER").unwrap_or_else(|_| "unknown".to_string())
}

fn fill_random(buffer: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buffer).map_err(|err| anyhow!("failed to generate random bytes: {err}"))
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
    fn events_are_linked_by_parent_pointers() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        repo.start_exploration("causal-chain".to_string())
            .expect("exploration should start");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint should succeed");

        let events = repo.list_events().expect("list events should succeed");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].parent_id, None);
        assert_eq!(events[1].parent_id, Some(events[0].id));
        assert!(events[0].signer_public_key.is_some());
        assert!(events[0].signature.is_some());
        assert!(events[1].signer_public_key.is_some());
        assert!(events[1].signature.is_some());
    }

    #[test]
    fn checkpoints_record_parent_checkpoint_lineage() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let cp1 = repo
            .create_checkpoint(Some("cp1".to_string()))
            .expect("first checkpoint should succeed");
        let cp2 = repo
            .create_checkpoint(Some("cp2".to_string()))
            .expect("second checkpoint should succeed");

        let EventKind::Checkpoint(cp1_payload) = cp1.kind else {
            panic!("first event should be checkpoint");
        };
        let EventKind::Checkpoint(cp2_payload) = cp2.kind else {
            panic!("second event should be checkpoint");
        };

        assert_eq!(cp1_payload.parent_checkpoint_event, None);
        assert_eq!(cp2_payload.parent_checkpoint_event, Some(cp1.id));
    }

    #[test]
    fn checkpoints_include_snapshot_merkle_root() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("hash.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");

        let checkpoint = repo
            .create_checkpoint(Some("cp".to_string()))
            .expect("checkpoint should succeed");
        let EventKind::Checkpoint(payload) = checkpoint.kind else {
            panic!("checkpoint payload expected");
        };
        let root = payload
            .snapshot_merkle_root
            .expect("checkpoint should include snapshot merkle root");
        assert_eq!(root.len(), 64);
        assert!(root.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn snapshot_merkle_root_is_content_deterministic() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("stable.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");

        let first = repo
            .create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");
        let EventKind::Checkpoint(first_payload) = first.kind else {
            panic!("checkpoint payload expected");
        };
        let first_root = first_payload
            .snapshot_merkle_root
            .expect("checkpoint should include snapshot merkle root");

        let second = repo
            .create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");
        let EventKind::Checkpoint(second_payload) = second.kind else {
            panic!("checkpoint payload expected");
        };
        let second_root = second_payload
            .snapshot_merkle_root
            .expect("checkpoint should include snapshot merkle root");

        assert_eq!(first_root, second_root);

        fs::write(&file, "export const value = 2;").expect("write should succeed");
        let changed = repo
            .create_checkpoint(Some("cp3".to_string()))
            .expect("checkpoint should succeed");
        let EventKind::Checkpoint(changed_payload) = changed.kind else {
            panic!("checkpoint payload expected");
        };
        let changed_root = changed_payload
            .snapshot_merkle_root
            .expect("checkpoint should include snapshot merkle root");

        assert_ne!(second_root, changed_root);
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
    fn undo_generated_checkpoint_uses_rewound_checkpoint_as_parent() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("undo-lineage.ts");
        fs::write(&file, "const value = 1;").expect("write should succeed");
        let cp1 = repo
            .create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&file, "const value = 2;").expect("write should succeed");
        repo.create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");

        let undo_result = repo.undo(UndoRequest::Last).expect("undo should succeed");
        let restored_checkpoint_id = undo_result
            .restored_checkpoint_event
            .expect("undo should emit restored checkpoint");

        let events = repo.list_events().expect("list events should succeed");
        let restored = events
            .into_iter()
            .find(|event| event.id == restored_checkpoint_id)
            .expect("restored checkpoint event should exist");
        let EventKind::Checkpoint(restored_payload) = restored.kind else {
            panic!("restored event should be checkpoint");
        };

        assert_eq!(restored_payload.parent_checkpoint_event, Some(cp1.id));
    }

    #[test]
    fn undo_last_exploration_event_is_reflected_in_replayed_state() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        repo.start_exploration("ephemeral".to_string())
            .expect("start exploration should succeed");
        repo.undo(UndoRequest::Last).expect("undo should succeed");

        let explorations = repo
            .list_explorations()
            .expect("exploration replay should succeed");
        assert!(explorations.is_empty());
    }

    #[test]
    fn replay_state_prefers_restored_checkpoint_after_undo() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("checkpoint-undo.ts");
        fs::write(&file, "const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&file, "const value = 2;").expect("write should succeed");
        repo.create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");

        let result = repo.undo(UndoRequest::Last).expect("undo should succeed");
        let replayed = repo.replay_state().expect("replay should succeed");

        assert_eq!(
            replayed.latest_checkpoint_event_id,
            result.restored_checkpoint_event
        );
    }

    #[test]
    fn undo_file_restores_target_path_only() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let target = dir.path().join("target.ts");
        let untouched = dir.path().join("untouched.ts");

        fs::write(&target, "const value = 1;").expect("write should succeed");
        fs::write(&untouched, "const other = 10;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&target, "const value = 2;").expect("write should succeed");
        fs::write(&untouched, "const other = 20;").expect("write should succeed");
        repo.create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");

        repo.undo_file(UndoRequest::Last, Path::new("target.ts"))
            .expect("file-scoped undo should succeed");

        let target_contents = fs::read_to_string(&target).expect("read should succeed");
        let untouched_contents = fs::read_to_string(&untouched).expect("read should succeed");
        assert!(target_contents.contains("const value = 1;"));
        assert!(untouched_contents.contains("const other = 20;"));
    }

    #[test]
    fn undo_file_records_scope_and_preserves_unrelated_state() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let target = dir.path().join("scoped.ts");
        fs::write(&target, "const value = 1;").expect("write should succeed");
        let cp1 = repo
            .create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&target, "const value = 2;").expect("write should succeed");
        let cp2 = repo
            .create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");

        let exploration = repo
            .start_exploration("keep-state".to_string())
            .expect("exploration should start");

        repo.undo_file(UndoRequest::To(cp2.id.to_string()), Path::new("scoped.ts"))
            .expect("file-scoped undo should succeed");

        let events = repo.list_events().expect("events should load");
        let last = events.last().expect("event should exist");
        let EventKind::Undo(undo) = &last.kind else {
            panic!("last event should be undo");
        };
        assert_eq!(undo.file_scope.as_deref(), Some("scoped.ts"));
        assert_eq!(undo.target_event_id, cp2.id);

        let explorations = repo.list_explorations().expect("explorations should load");
        assert!(
            explorations.iter().any(
                |entry| entry.id == exploration.id && entry.status == ExplorationStatus::Active
            )
        );

        let EventKind::Checkpoint(cp1_payload) = cp1.kind else {
            panic!("checkpoint payload expected");
        };
        let replayed = repo.replay_state().expect("replay should succeed");
        assert_ne!(
            replayed.latest_checkpoint_snapshot_id,
            Some(cp1_payload.snapshot_id)
        );
    }

    #[test]
    fn parse_duration_specs() {
        assert_eq!(parse_duration_spec("5m").expect("duration").as_secs(), 300);
        assert_eq!(parse_duration_spec("30").expect("duration").as_secs(), 30);
        assert!(parse_duration_spec("1w").is_err());
    }

    #[test]
    fn init_creates_signing_key_file() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        assert!(dir.path().join(SIGNING_KEY_FILE).is_file());
    }

    #[test]
    fn checkpoints_advance_main_branch_ref() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let cp1 = repo
            .create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");
        let cp2 = repo
            .create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");

        let refs = repo.list_refs().expect("refs should load");
        let main = refs
            .into_iter()
            .find(|entry| entry.kind == RefKind::Branch && entry.name == "main")
            .expect("main branch ref should exist");

        assert_ne!(main.target_event_id, cp1.id);
        assert_eq!(main.target_event_id, cp2.id);
    }

    #[test]
    fn tag_refs_require_checkpoint_target() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        repo.start_exploration("not-a-checkpoint".to_string())
            .expect("exploration should start");
        let exploration_event_id = repo
            .list_events()
            .expect("events should load")
            .last()
            .expect("exploration event should exist")
            .id;

        let err = repo
            .upsert_ref(
                RefKind::Tag,
                "v0".to_string(),
                exploration_event_id.to_string(),
                None,
            )
            .expect_err("tag should reject non-checkpoint targets");

        assert!(
            format!("{:#}", err).contains("must target checkpoint"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn workspace_ref_persists_auto_rebase_and_delete() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let checkpoint = repo
            .create_checkpoint(Some("cp".to_string()))
            .expect("checkpoint should succeed");
        let workspace_ref = repo
            .upsert_ref(
                RefKind::Workspace,
                "agent/a".to_string(),
                checkpoint.id.to_string(),
                Some(true),
            )
            .expect("workspace ref should be created");

        assert_eq!(workspace_ref.kind, RefKind::Workspace);
        assert!(
            workspace_ref
                .workspace
                .as_ref()
                .expect("workspace config should exist")
                .auto_rebase
        );

        let removed = repo
            .delete_ref(RefKind::Workspace, "agent/a")
            .expect("delete should succeed");
        assert!(removed);
        assert!(
            repo.list_refs()
                .expect("refs should load")
                .into_iter()
                .all(|entry| !(entry.kind == RefKind::Workspace && entry.name == "agent/a"))
        );
    }

    #[test]
    fn fsck_passes_for_valid_repo() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("healthy.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");
        fs::write(&file, "export const value = 2;").expect("write should succeed");
        repo.create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");
        repo.undo(UndoRequest::Last).expect("undo should succeed");

        let report = repo.fsck().expect("fsck should pass");
        assert_eq!(report.event_count, 4);
        assert_eq!(report.checkpoint_count, 3);
        assert_eq!(report.snapshot_count, 3);
        assert_eq!(report.ref_count, 1);
    }

    #[test]
    fn fsck_detects_tampered_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("tamper.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        let checkpoint = repo
            .create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");
        let EventKind::Checkpoint(payload) = checkpoint.kind else {
            panic!("checkpoint payload expected");
        };

        let snapshot_file = repo.snapshot_path(payload.snapshot_id).join("tamper.ts");
        fs::write(&snapshot_file, "export const value = 999;").expect("tamper write should work");

        let err = repo.fsck().expect_err("fsck should fail");
        assert!(
            format!("{:#}", err).contains("merkle root mismatch"),
            "unexpected error: {err}"
        );
    }
}
