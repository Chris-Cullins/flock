use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
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
use crate::semantic::{
    SemanticFileDiff, SemanticMergeResult, diff as semantic_diff, supported_source,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowSafetyReport {
    pub mode: String,
    pub clean: bool,
    pub checks: Vec<ShadowSafetyCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowSafetyCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub recovery: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactReport {
    pub target: String,
    pub direct_dependents: Vec<String>,
    pub transitive_dependents: Vec<String>,
    pub symbols: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewStats {
    pub files_changed: usize,
    pub symbols_added: usize,
    pub symbols_removed: usize,
    pub symbols_modified: usize,
    pub high_risk_count: usize,
    pub breaking_count: usize,
}

pub struct ReviewSummary {
    pub exploration: ExplorationSummary,
    pub diffs: Vec<SemanticFileDiff>,
    pub stats: ReviewStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub workspace: RepoRef,
    pub event_count: usize,
    pub checkpoint_count: usize,
    pub snapshot_count: usize,
    pub limits_exceeded: Vec<String>,
}

fn check_workspace_limits(
    config: &WorkspaceRefConfig,
    event_count: usize,
    snapshot_count: usize,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(max) = config.max_snapshots {
        if snapshot_count > max {
            warnings.push(format!(
                "snapshot limit exceeded: {} > {}",
                snapshot_count, max
            ));
        }
    }
    if let Some(max) = config.max_events {
        if event_count > max {
            warnings.push(format!(
                "event limit exceeded: {} > {}",
                event_count, max
            ));
        }
    }
    warnings
}

#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoMode {
    GitCompatible,
    GitColocated,
}

impl RepoMode {
    fn as_str(self) -> &'static str {
        match self {
            RepoMode::GitCompatible => "git-compatible",
            RepoMode::GitColocated => "git-colocated",
        }
    }
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
        self.init_with_mode(RepoMode::GitCompatible)
    }

    pub fn init_colocated(&self) -> Result<()> {
        self.init_with_mode(RepoMode::GitColocated)
    }

    fn init_with_mode(&self, mode: RepoMode) -> Result<()> {
        fs::create_dir_all(self.root.join(SNAPSHOT_DIR))
            .context("failed to create snapshots directory")?;

        fl_storage::EventLog::for_root(self.root()).ensure_exists()?;
        RefStore::for_root(self.root()).ensure_exists()?;
        self.ensure_signing_key()?;

        let config = self.root.join(CONFIG_FILE);
        if !config.exists() {
            let contents = [
                format!("mode = \"{}\"", mode.as_str()),
                "semantic_default = \"typescript\"".to_string(),
                "analyzers = [\"typescript\", \"javascript\"]".to_string(),
            ]
            .join("\n");
            fs::write(&config, format!("{}\n", contents))
                .with_context(|| format!("failed to write {}", config.display()))?;
        }

        if mode == RepoMode::GitColocated {
            self.ensure_git_repository()?;
            self.ensure_git_exclude_entry(".flock/")?;
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
                base_snapshot_id: None,
                max_snapshots: None,
                max_events: None,
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
        self.sync_ref_to_git_if_colocated(&reference)?;
        Ok(reference)
    }

    pub fn delete_ref(&self, kind: RefKind, name: &str) -> Result<bool> {
        self.assert_initialized()?;

        let normalized_name = normalize_ref_name(name)?;
        let store = RefStore::for_root(self.root());
        store.ensure_exists()?;
        let removed = store.delete(kind, &normalized_name)?;
        if removed {
            self.delete_git_ref_if_colocated(kind, &normalized_name)?;
        }
        Ok(removed)
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
        all_paths.extend(current_files.iter().cloned());

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

        enrich_semantic_impacts(self.root(), &current_files, &mut diffs)?;
        diffs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(diffs)
    }

    /// Groups semantic changes by inferred intent category.
    pub fn semantic_diff_with_intents(
        &self,
    ) -> Result<Vec<(String, Vec<SemanticFileDiff>)>> {
        let diffs = self.semantic_diff_from_latest_checkpoint()?;
        Ok(classify_diffs_by_intent(diffs))
    }

    /// Computes transitive impact of a file path within the repository.
    pub fn impact_analysis(&self, path: &str) -> Result<ImpactReport> {
        self.assert_initialized()?;

        let current_files = collect_source_files(self.root(), true)?;
        let reverse_dependencies = build_reverse_dependency_index(self.root(), &current_files)?;

        let target_path = PathBuf::from(path);
        let all_impacted = collect_impacted_files(&target_path, &reverse_dependencies);

        // Separate direct vs transitive dependents
        let direct: Vec<String> = reverse_dependencies
            .get(&target_path)
            .map(|deps| {
                deps.iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();

        let transitive: Vec<String> = all_impacted
            .iter()
            .filter(|p| *p != &target_path)
            .filter(|p| !direct.contains(&p.to_string_lossy().to_string()))
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        // Collect symbols from the target file if it exists
        let symbols = self.extract_symbols_for_path(path)?;

        Ok(ImpactReport {
            target: path.to_string(),
            direct_dependents: direct,
            transitive_dependents: transitive,
            symbols,
        })
    }

    /// Previews semantic merge of three file versions without modifying the workspace.
    pub fn semantic_merge_preview(
        &self,
        base_path: &Path,
        left_path: &Path,
        right_path: &Path,
    ) -> Result<SemanticMergeResult> {
        let base = std::fs::read(base_path)
            .with_context(|| format!("failed to read base file: {}", base_path.display()))?;
        let left = std::fs::read(left_path)
            .with_context(|| format!("failed to read left file: {}", left_path.display()))?;
        let right = std::fs::read(right_path)
            .with_context(|| format!("failed to read right file: {}", right_path.display()))?;

        // Use the left path as the representative for language detection
        let result = fl_semantic::merge(left_path, Some(&base), Some(&left), Some(&right))?;
        result.ok_or_else(|| {
            anyhow!(
                "unsupported file type for semantic merge: {}",
                left_path.display()
            )
        })
    }

    /// Reviews an exploration by diffing its base checkpoint against current working directory.
    pub fn review_exploration(&self, id: Uuid) -> Result<ReviewSummary> {
        self.assert_initialized()?;

        let exploration = self
            .list_explorations()?
            .into_iter()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow!("exploration {} not found", id))?;

        let diffs = if let Some(base_checkpoint_id) = exploration.base_checkpoint_event {
            let event = self.event_by_id(base_checkpoint_id)?;
            let EventKind::Checkpoint(payload) = event.kind else {
                bail!(
                    "base checkpoint event {} is not a checkpoint",
                    base_checkpoint_id
                )
            };
            self.diff_snapshot_against_working_dir(payload.snapshot_id)?
        } else {
            // No base checkpoint — diff against empty
            self.semantic_diff_from_latest_checkpoint()?
        };

        let stats = compute_review_stats(&diffs);

        Ok(ReviewSummary {
            exploration,
            diffs,
            stats,
        })
    }

    fn diff_snapshot_against_working_dir(
        &self,
        snapshot_id: Uuid,
    ) -> Result<Vec<SemanticFileDiff>> {
        let snapshot_root = self.snapshot_path(snapshot_id);
        let snapshot_files = collect_source_files(&snapshot_root, false)?;
        let current_files = collect_source_files(self.root(), true)?;

        let mut all_paths: BTreeSet<PathBuf> = snapshot_files;
        all_paths.extend(current_files.iter().cloned());

        let mut diffs = Vec::new();
        for rel_path in &all_paths {
            let before_path = snapshot_root.join(rel_path);
            let after_path = self.root.join(rel_path);

            let before = fs::read(&before_path).ok();
            let after = fs::read(&after_path).ok();

            let diff = semantic_diff(rel_path, before.as_deref(), after.as_deref())?;
            if let Some(diff) = diff {
                diffs.push(diff);
            }
        }

        enrich_semantic_impacts(self.root(), &current_files, &mut diffs)?;
        diffs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(diffs)
    }

    fn extract_symbols_for_path(&self, path: &str) -> Result<Vec<String>> {
        let file_path = self.root.join(path);
        if !file_path.is_file() {
            return Ok(Vec::new());
        }
        let rel = PathBuf::from(path);
        if !supported_source(&rel) {
            return Ok(Vec::new());
        }
        let content = fs::read(&file_path)
            .with_context(|| format!("failed to read {}", file_path.display()))?;
        // Diff against empty to get all symbols as "Added"
        let diff = semantic_diff(&rel, None, Some(&content))?;
        Ok(diff
            .map(|d| d.changes.into_iter().map(|c| c.symbol).collect())
            .unwrap_or_default())
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

    /// Compare two explorations by diffing their base checkpoint snapshots.
    /// If `right_id` is None, compares the left exploration's base against current working dir.
    pub fn compare_explorations(
        &self,
        left_id: Uuid,
        right_id: Option<Uuid>,
    ) -> Result<Vec<SemanticFileDiff>> {
        self.assert_initialized()?;

        let explorations = self.list_explorations()?;

        let left = explorations
            .iter()
            .find(|e| e.id == left_id)
            .ok_or_else(|| anyhow!("exploration {} not found", left_id))?;

        let left_snapshot_id = self.exploration_snapshot_id(left)?;

        if let Some(right_id) = right_id {
            let right = explorations
                .iter()
                .find(|e| e.id == right_id)
                .ok_or_else(|| anyhow!("exploration {} not found", right_id))?;

            let right_snapshot_id = self.exploration_snapshot_id(right)?;

            self.diff_two_snapshots(left_snapshot_id, right_snapshot_id)
        } else {
            self.diff_snapshot_against_working_dir(left_snapshot_id)
        }
    }

    /// Diff two snapshots against each other.
    fn diff_two_snapshots(
        &self,
        left_snapshot_id: Uuid,
        right_snapshot_id: Uuid,
    ) -> Result<Vec<SemanticFileDiff>> {
        let left_root = self.snapshot_path(left_snapshot_id);
        let right_root = self.snapshot_path(right_snapshot_id);

        let left_files = collect_source_files(&left_root, false)?;
        let right_files = collect_source_files(&right_root, false)?;

        let mut all_paths: BTreeSet<PathBuf> = left_files;
        all_paths.extend(right_files.iter().cloned());

        let mut diffs = Vec::new();
        for rel_path in &all_paths {
            let left_path = left_root.join(rel_path);
            let right_path = right_root.join(rel_path);

            let before = fs::read(&left_path).ok();
            let after = fs::read(&right_path).ok();

            let diff = semantic_diff(rel_path, before.as_deref(), after.as_deref())?;
            if let Some(diff) = diff {
                diffs.push(diff);
            }
        }

        diffs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(diffs)
    }

    /// Get the snapshot ID for an exploration's base checkpoint.
    fn exploration_snapshot_id(&self, exploration: &ExplorationSummary) -> Result<Uuid> {
        let base_id = exploration.base_checkpoint_event.ok_or_else(|| {
            anyhow!(
                "exploration {} has no base checkpoint",
                exploration.id
            )
        })?;
        let event = self.event_by_id(base_id)?;
        let EventKind::Checkpoint(payload) = event.kind else {
            bail!(
                "base checkpoint event {} is not a checkpoint",
                base_id
            )
        };
        Ok(payload.snapshot_id)
    }

    /// Prune abandoned explorations older than the given TTL duration.
    /// Returns the number of explorations pruned.
    pub fn prune_explorations(&self, max_age: std::time::Duration) -> Result<usize> {
        self.assert_initialized()?;

        let now_nanos: u128 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before unix epoch")?
            .as_nanos();

        let explorations = self.list_explorations()?;
        let mut pruned = 0;

        for exploration in &explorations {
            if exploration.status != ExplorationStatus::Abandoned {
                continue;
            }

            let updated_nanos: u128 = exploration
                .updated_at
                .parse()
                .unwrap_or(0);
            let age_nanos = now_nanos.saturating_sub(updated_nanos);

            if age_nanos >= max_age.as_nanos() {
                // Clean up snapshot if exploration had a base checkpoint
                if let Some(base_id) = exploration.base_checkpoint_event {
                    if let Ok(event) = self.event_by_id(base_id) {
                        if let EventKind::Checkpoint(payload) = event.kind {
                            let snapshot_path = self.snapshot_path(payload.snapshot_id);
                            if snapshot_path.is_dir() {
                                let _ = fs::remove_dir_all(&snapshot_path);
                            }
                        }
                    }
                }
                pruned += 1;
            }
        }

        Ok(pruned)
    }

    /// Create an isolated workspace with its own base snapshot.
    pub fn create_workspace(
        &self,
        name: String,
        auto_rebase: bool,
    ) -> Result<RepoRef> {
        self.assert_initialized()?;

        // Capture current state as the workspace base
        let checkpoint = self
            .latest_checkpoint()
            .ok_or_else(|| anyhow!("cannot create workspace: no checkpoint exists; run `fl checkpoint` first"))?;
        let EventKind::Checkpoint(payload) = checkpoint.kind else {
            bail!("latest checkpoint event is malformed")
        };

        let workspace_ref = RepoRef {
            kind: RefKind::Workspace,
            name: name.clone(),
            target_event_id: checkpoint.id,
            workspace: Some(WorkspaceRefConfig {
                auto_rebase,
                base_snapshot_id: Some(payload.snapshot_id),
                max_snapshots: None,
                max_events: None,
            }),
        };

        let store = RefStore::for_root(self.root());
        store.upsert(workspace_ref.clone())?;

        Ok(workspace_ref)
    }

    /// List all workspaces.
    pub fn list_workspaces(&self) -> Result<Vec<RepoRef>> {
        self.assert_initialized()?;

        let refs = self.list_refs()?;
        Ok(refs
            .into_iter()
            .filter(|r| r.kind == RefKind::Workspace)
            .collect())
    }

    /// Get workspace info including resource usage.
    pub fn workspace_info(&self, name: &str) -> Result<WorkspaceInfo> {
        self.assert_initialized()?;

        let refs = self.list_refs()?;
        let ws_ref = refs
            .iter()
            .find(|r| r.kind == RefKind::Workspace && r.name == name)
            .ok_or_else(|| anyhow!("workspace `{}` not found", name))?
            .clone();

        if ws_ref.workspace.is_none() {
            bail!("workspace `{}` has no config", name);
        }

        let events = self.list_events()?;
        let checkpoints = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Checkpoint(_)))
            .count();

        // Count snapshots on disk
        let snapshot_dir = self.root.join(SNAPSHOT_DIR);
        let snapshot_count = if snapshot_dir.is_dir() {
            fs::read_dir(&snapshot_dir)
                .map(|entries| entries.filter_map(|e| e.ok()).count())
                .unwrap_or(0)
        } else {
            0
        };

        let limits_exceeded = check_workspace_limits(
            ws_ref.workspace.as_ref().unwrap(),
            events.len(),
            snapshot_count,
        );

        Ok(WorkspaceInfo {
            workspace: ws_ref,
            event_count: events.len(),
            checkpoint_count: checkpoints,
            snapshot_count,
            limits_exceeded,
        })
    }

    /// Set resource limits on a workspace.
    pub fn set_workspace_limits(
        &self,
        name: &str,
        max_snapshots: Option<usize>,
        max_events: Option<usize>,
    ) -> Result<RepoRef> {
        self.assert_initialized()?;

        let store = RefStore::for_root(self.root());
        let refs = store.read_all()?;
        let mut ws_ref = refs
            .iter()
            .find(|r| r.kind == RefKind::Workspace && r.name == name)
            .ok_or_else(|| anyhow!("workspace `{}` not found", name))?
            .clone();

        let config = ws_ref
            .workspace
            .as_mut()
            .ok_or_else(|| anyhow!("workspace `{}` has no config", name))?;

        if let Some(ms) = max_snapshots {
            config.max_snapshots = Some(ms);
        }
        if let Some(me) = max_events {
            config.max_events = Some(me);
        }

        store.upsert(ws_ref.clone())?;
        Ok(ws_ref)
    }

    /// Quick save: create a checkpoint with auto-generated label, optimized for agent use.
    pub fn quick_save(&self, tag: Option<String>) -> Result<Event> {
        self.assert_initialized()?;
        let label = tag.unwrap_or_else(|| format!("quick-{}", Uuid::new_v4().simple()));
        self.create_checkpoint_with_lineage(label, Some("quick save".to_string()), None)
    }

    /// Quick restore: undo to the last checkpoint, optimized for agent use.
    pub fn quick_restore(&self) -> Result<UndoResult> {
        self.undo(UndoRequest::Last)
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

    pub fn git_commit(&self, message: String) -> Result<String> {
        self.run_git_bridge_action(GitBridgeAction::Commit, || {
            self.run_git(&["add", "-A"])?;
            self.run_git(&["commit", "-m", &message])
        })
    }

    pub fn git_shadow_status(&self) -> Result<ShadowSafetyReport> {
        self.assert_initialized()?;
        self.assert_git_initialized()?;

        let mode = self.repo_mode()?;
        let mut checks = Vec::new();

        if mode != RepoMode::GitColocated {
            checks.push(ShadowSafetyCheck {
                name: "mode".to_string(),
                ok: true,
                detail: "repository is not in git-colocated mode; shadow mode checks are inactive"
                    .to_string(),
                recovery: Some(
                    "re-initialize with `fl init --colocated` to enable shadow mode safeguards"
                        .to_string(),
                ),
            });
            return Ok(ShadowSafetyReport {
                mode: mode.as_str().to_string(),
                clean: true,
                checks,
            });
        }

        let exclude_ok = self.git_exclude_has_entry(".flock/")?;
        checks.push(ShadowSafetyCheck {
            name: "git exclude".to_string(),
            ok: exclude_ok,
            detail: if exclude_ok {
                "`.flock/` is excluded from git tracking".to_string()
            } else {
                "`.flock/` is not excluded in `.git/info/exclude`".to_string()
            },
            recovery: (!exclude_ok)
                .then_some("append `.flock/` to `.git/info/exclude`".to_string()),
        });

        let worktree_dirty = self.git_has_worktree_changes()?;
        checks.push(ShadowSafetyCheck {
            name: "working tree".to_string(),
            ok: !worktree_dirty,
            detail: if worktree_dirty {
                "working tree has pending changes".to_string()
            } else {
                "working tree is clean".to_string()
            },
            recovery: worktree_dirty.then_some(
                "commit, stash, or discard local changes before bridge operations".to_string(),
            ),
        });

        let checkpoint_count = self.list_checkpoints_with_payload()?.len();
        if checkpoint_count == 0 {
            checks.push(ShadowSafetyCheck {
                name: "head/ref alignment".to_string(),
                ok: true,
                detail: "no checkpoints yet; alignment check deferred".to_string(),
                recovery: Some("create a checkpoint to start ref alignment tracking".to_string()),
            });
        } else {
            let head = self.resolve_git_revision_if_exists("HEAD")?;
            let flock_main = self.resolve_git_revision_if_exists("refs/flock/branches/main")?;

            match (head.as_deref(), flock_main.as_deref()) {
                (Some(head), Some(main_ref)) if head == main_ref => checks.push(ShadowSafetyCheck {
                    name: "head/ref alignment".to_string(),
                    ok: true,
                    detail: format!(
                        "HEAD matches refs/flock/branches/main at {}",
                        short_sha(head)
                    ),
                    recovery: None,
                }),
                (Some(head), Some(main_ref)) => checks.push(ShadowSafetyCheck {
                    name: "head/ref alignment".to_string(),
                    ok: false,
                    detail: format!(
                        "HEAD ({}) diverges from refs/flock/branches/main ({})",
                        short_sha(head),
                        short_sha(main_ref)
                    ),
                    recovery: Some(
                        "run `fl git import` to map git commits to checkpoints, then create a new checkpoint if needed"
                            .to_string(),
                    ),
                }),
                (Some(_), None) => checks.push(ShadowSafetyCheck {
                    name: "head/ref alignment".to_string(),
                    ok: false,
                    detail: "refs/flock/branches/main is missing".to_string(),
                    recovery: Some(
                        "run `fl checkpoint -m \"sync\"` to recreate flock main ref mapping"
                            .to_string(),
                    ),
                }),
                (None, Some(_)) => checks.push(ShadowSafetyCheck {
                    name: "head/ref alignment".to_string(),
                    ok: false,
                    detail: "HEAD commit is missing while refs/flock/branches/main exists".to_string(),
                    recovery: Some(
                        "repair git history state, then run `fl git import` to rebuild mappings"
                            .to_string(),
                    ),
                }),
                (None, None) => checks.push(ShadowSafetyCheck {
                    name: "head/ref alignment".to_string(),
                    ok: false,
                    detail: "both HEAD and refs/flock/branches/main are missing".to_string(),
                    recovery: Some(
                        "create a checkpoint (`fl checkpoint -m \"bootstrap\"`) to establish initial mappings"
                            .to_string(),
                    ),
                }),
            }
        }

        let clean = checks.iter().all(|check| check.ok);
        Ok(ShadowSafetyReport {
            mode: mode.as_str().to_string(),
            clean,
            checks,
        })
    }

    pub fn git_push(&self, remote: Option<String>, branch: Option<String>) -> Result<String> {
        self.run_git_bridge_action(GitBridgeAction::Push, || {
            let remote = self.resolve_git_remote_name(remote.as_deref())?;
            let branch = self.resolve_git_branch_name(branch.as_deref())?;

            let mut details = Vec::new();
            let branch_push = self
                .run_git(&["push", &remote, &branch])
                .with_context(|| format!("failed to push branch `{branch}` to `{remote}`"))?;
            details.push(format!("push {remote} {branch}"));
            details.push(branch_push);

            if self.repo_mode()? == RepoMode::GitColocated {
                if let Some(flock_push) = self.push_colocated_refs_to_remote(&remote)? {
                    details.push(flock_push);
                }
            }

            Ok(join_non_empty_lines(details))
        })
    }

    pub fn git_pull(&self, remote: Option<String>, branch: Option<String>) -> Result<String> {
        self.run_git_bridge_action(GitBridgeAction::Pull, || {
            let remote = self.resolve_git_remote_name(remote.as_deref())?;
            let branch = self.resolve_git_branch_name(branch.as_deref())?;

            let mut details = Vec::new();
            let branch_pull = self
                .run_git(&["pull", "--ff-only", &remote, &branch])
                .with_context(|| format!("failed to pull branch `{branch}` from `{remote}`"))?;
            details.push(format!("pull --ff-only {remote} {branch}"));
            details.push(branch_pull);

            if self.repo_mode()? == RepoMode::GitColocated {
                if let Some(flock_fetch) = self.fetch_colocated_refs_from_remote(&remote)? {
                    details.push(flock_fetch);
                }
            }

            Ok(join_non_empty_lines(details))
        })
    }

    pub fn git_import(&self, git_ref: Option<String>) -> Result<String> {
        self.run_git_bridge_action(GitBridgeAction::Import, || {
            let git_ref = git_ref
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("HEAD")
                .to_string();

            let output = self
                .run_git(&["rev-list", "--reverse", &git_ref])
                .with_context(|| format!("failed to enumerate commits for `{git_ref}`"))?;
            let commits: Vec<String> = output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect();

            if commits.is_empty() {
                bail!("no commits found for git ref `{git_ref}`");
            }

            let mut known_commits = self.known_git_commit_mappings()?;
            let mut parent_checkpoint = self.latest_checkpoint().map(|event| event.id);
            let mut imported = Vec::new();

            for commit in commits {
                if known_commits.contains(&commit) {
                    continue;
                }

                let subject = self.run_git(&["show", "-s", "--format=%s", &commit])?;
                let message = self.run_git(&["show", "-s", "--format=%B", &commit])?;
                let label = normalize_label(subject.trim());
                let label = if label.is_empty() {
                    format!("git-import-{}", short_sha(&commit))
                } else {
                    label
                };
                let message = if message.trim().is_empty() {
                    Some(format!("git import {}", commit))
                } else {
                    Some(message.trim().to_string())
                };

                let event = self.create_checkpoint_from_git_commit(
                    &commit,
                    label,
                    message,
                    parent_checkpoint,
                )?;
                parent_checkpoint = Some(event.id);
                known_commits.insert(commit.clone());
                imported.push((commit, event.id));
            }

            if imported.is_empty() {
                return Ok(format!(
                    "import {git_ref}: no new commits (all commits are already mapped)"
                ));
            }

            let mut details = Vec::new();
            details.push(format!(
                "import {git_ref}: imported {} commits",
                imported.len()
            ));
            details.extend(imported.iter().map(|(commit, checkpoint)| {
                format!("git_commit={commit} checkpoint={checkpoint}")
            }));
            Ok(join_non_empty_lines(details))
        })
    }

    pub fn git_export(&self, branch: Option<String>) -> Result<String> {
        self.run_git_bridge_action(GitBridgeAction::Export, || {
            let branch = branch
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("flock/export")
                .to_string();
            let checkpoints = self.list_checkpoints_with_payload()?;
            if checkpoints.is_empty() {
                bail!("no checkpoints available to export");
            }

            let temp_repo =
                tempfile::tempdir().context("failed to create temporary git export repository")?;
            fl_bridge_git::run_git(temp_repo.path(), &["init"])
                .context("failed to initialize temporary git repository for export")?;

            let mut mapping = Vec::new();
            for (event, checkpoint) in &checkpoints {
                clear_directory_except(temp_repo.path(), &[".git"])?;
                let snapshot_root = self.snapshot_path(checkpoint.snapshot_id);
                copy_tree(snapshot_root.as_path(), temp_repo.path(), false)?;

                fl_bridge_git::run_git(temp_repo.path(), &["add", "-A"])
                    .context("failed to stage exported checkpoint contents")?;
                let message = checkpoint
                    .message
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(&checkpoint.label);
                fl_bridge_git::run_git(
                    temp_repo.path(),
                    &[
                        "-c",
                        "user.name=Flock",
                        "-c",
                        "user.email=flock@local",
                        "commit",
                        "--allow-empty",
                        "-m",
                        message,
                    ],
                )
                .with_context(|| {
                    format!(
                        "failed to commit exported checkpoint {} in temporary git repository",
                        event.id
                    )
                })?;

                let sha = fl_bridge_git::run_git(temp_repo.path(), &["rev-parse", "HEAD"])
                    .context("failed to resolve exported commit sha")?;
                mapping.push((event.id, sha.trim().to_string()));
            }

            let source = temp_repo.path().to_string_lossy().to_string();
            let refspec = format!("+HEAD:refs/heads/{branch}");
            let fetch = self
                .run_git(&["fetch", &source, &refspec])
                .with_context(|| format!("failed to import exported history into `{branch}`"))?;

            let mut details = Vec::new();
            details.push(format!(
                "exported {} checkpoints to refs/heads/{}",
                mapping.len(),
                branch
            ));
            if !fetch.trim().is_empty() {
                details.push(fetch);
            }
            details.extend(mapping.iter().map(|(checkpoint, commit)| {
                format!("checkpoint={checkpoint} git_commit={commit}")
            }));
            Ok(join_non_empty_lines(details))
        })
    }

    fn create_checkpoint_with_lineage(
        &self,
        label: String,
        message: Option<String>,
        parent_checkpoint_event: Option<Uuid>,
    ) -> Result<Event> {
        let git_commit_mapping = if self.repo_mode()? == RepoMode::GitColocated {
            Some(self.commit_checkpoint_to_git(message.as_deref(), &label)?)
        } else {
            None
        };

        self.create_checkpoint_from_source_with_lineage(
            &self.root,
            true,
            label,
            message,
            parent_checkpoint_event,
            git_commit_mapping,
        )
    }

    fn create_checkpoint_from_git_commit(
        &self,
        git_commit: &str,
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
        self.extract_git_commit_tree_to_directory(git_commit, &snapshot_path)?;

        self.create_checkpoint_from_existing_snapshot_with_lineage(
            snapshot_id,
            label,
            message,
            parent_checkpoint_event,
            Some(git_commit.to_string()),
        )
    }

    fn create_checkpoint_from_source_with_lineage(
        &self,
        source_root: &Path,
        apply_skip: bool,
        label: String,
        message: Option<String>,
        parent_checkpoint_event: Option<Uuid>,
        git_commit_mapping: Option<String>,
    ) -> Result<Event> {
        let snapshot_id = Uuid::new_v4();
        let snapshot_path = self.snapshot_path(snapshot_id);
        fs::create_dir_all(&snapshot_path).with_context(|| {
            format!(
                "failed to create snapshot directory {}",
                snapshot_path.display()
            )
        })?;

        copy_tree(source_root, &snapshot_path, apply_skip)?;

        self.create_checkpoint_from_existing_snapshot_with_lineage(
            snapshot_id,
            label,
            message,
            parent_checkpoint_event,
            git_commit_mapping,
        )
    }

    fn create_checkpoint_from_existing_snapshot_with_lineage(
        &self,
        snapshot_id: Uuid,
        label: String,
        message: Option<String>,
        parent_checkpoint_event: Option<Uuid>,
        git_commit_mapping: Option<String>,
    ) -> Result<Event> {
        let snapshot_path = self.snapshot_path(snapshot_id);
        let snapshot_merkle_root = compute_snapshot_merkle_root(&snapshot_path)?;
        let parent_checkpoint_event =
            parent_checkpoint_event.or_else(|| self.latest_checkpoint().map(|event| event.id));
        let event = self.append_event(EventKind::Checkpoint(CheckpointEvent {
            label,
            message: message.clone(),
            snapshot_id,
            parent_checkpoint_event,
            snapshot_merkle_root: Some(snapshot_merkle_root),
        }))?;

        if let Some(git_commit_sha) = git_commit_mapping {
            self.append_event(EventKind::GitBridge(GitBridgeEvent {
                action: GitBridgeAction::Commit,
                success: true,
                detail: format!("checkpoint={} git_commit={}", event.id, git_commit_sha),
            }))?;
        }

        self.advance_main_ref(event.id)?;
        Ok(event)
    }

    fn extract_git_commit_tree_to_directory(
        &self,
        git_commit: &str,
        destination: &Path,
    ) -> Result<()> {
        let output = Command::new("git")
            .args(["archive", "--format=tar", git_commit])
            .current_dir(&self.root)
            .output()
            .with_context(|| format!("failed to run git archive for commit `{git_commit}`"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "git archive failed for commit `{}`: {}",
                git_commit,
                if stderr.is_empty() {
                    "(no output)"
                } else {
                    stderr.as_str()
                }
            );
        }

        let cursor = Cursor::new(output.stdout);
        let mut archive = tar::Archive::new(cursor);
        archive
            .unpack(destination)
            .with_context(|| format!("failed to extract git archive for `{git_commit}`"))?;
        Ok(())
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
                "git bridge operations require an existing .git directory in {}",
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

    fn list_checkpoints_with_payload(&self) -> Result<Vec<(Event, CheckpointEvent)>> {
        let mut checkpoints = Vec::new();
        for event in self.list_events()? {
            let EventKind::Checkpoint(payload) = event.kind.clone() else {
                continue;
            };
            checkpoints.push((event, payload));
        }
        Ok(checkpoints)
    }

    fn known_git_commit_mappings(&self) -> Result<BTreeSet<String>> {
        let mut commits = BTreeSet::new();
        for event in self.list_events()? {
            let EventKind::GitBridge(bridge) = event.kind else {
                continue;
            };
            if bridge.action != GitBridgeAction::Commit || !bridge.success {
                continue;
            }

            if let Some(commit) = parse_git_commit_from_detail(&bridge.detail) {
                commits.insert(commit);
            }
        }
        Ok(commits)
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

    fn assert_shadow_mode_safe(&self, action: &GitBridgeAction) -> Result<()> {
        if self.repo_mode()? != RepoMode::GitColocated {
            return Ok(());
        }

        if !self.git_exclude_has_entry(".flock/")? {
            bail!(
                "shadow mode safety check failed: `.flock/` is not excluded from git. Recovery: append `.flock/` to `.git/info/exclude`"
            );
        }

        if matches!(
            action,
            GitBridgeAction::Pull | GitBridgeAction::Import | GitBridgeAction::Export
        ) && self.git_has_worktree_changes()?
        {
            bail!(
                "shadow mode safety check failed: working tree has uncommitted changes. Recovery: commit, stash, or discard local changes before running `fl git {}`",
                git_action_name(action)
            );
        }

        if matches!(
            action,
            GitBridgeAction::Push | GitBridgeAction::Pull | GitBridgeAction::Export
        ) {
            self.assert_shadow_main_ref_aligned()?;
        }

        Ok(())
    }

    fn assert_shadow_main_ref_aligned(&self) -> Result<()> {
        let checkpoint_count = self.list_checkpoints_with_payload()?.len();
        if checkpoint_count == 0 {
            return Ok(());
        }

        let head = self.resolve_git_revision_if_exists("HEAD")?.ok_or_else(|| {
            anyhow!(
                "shadow mode safety check failed: git HEAD is missing while checkpoints exist. Recovery: restore git history, then run `fl git import`"
            )
        })?;
        let flock_main = self
            .resolve_git_revision_if_exists("refs/flock/branches/main")?
            .ok_or_else(|| {
                anyhow!(
                    "shadow mode safety check failed: refs/flock/branches/main is missing. Recovery: run `fl checkpoint -m \"sync\"`"
                )
            })?;

        if head != flock_main {
            bail!(
                "shadow mode safety check failed: git HEAD ({}) diverges from refs/flock/branches/main ({}). Recovery: run `fl git import` to map git commits, then checkpoint if needed",
                short_sha(&head),
                short_sha(&flock_main)
            );
        }

        Ok(())
    }

    fn git_exclude_has_entry(&self, entry: &str) -> Result<bool> {
        let exclude_path = self.root.join(".git").join("info").join("exclude");
        let raw = fs::read_to_string(&exclude_path).unwrap_or_default();
        let expected = entry.trim_end_matches('/');
        Ok(raw.lines().map(str::trim).any(|line| {
            let normalized = line.trim_end_matches('/');
            normalized == expected
        }))
    }

    fn git_has_worktree_changes(&self) -> Result<bool> {
        let output = self.run_git(&["status", "--porcelain"])?;
        Ok(!output.trim().is_empty())
    }

    fn resolve_git_revision_if_exists(&self, revision: &str) -> Result<Option<String>> {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", revision])
            .current_dir(&self.root)
            .output()
            .with_context(|| format!("failed to run git rev-parse for `{revision}`"))?;
        if !output.status.success() {
            return Ok(None);
        }

        let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if resolved.is_empty() {
            return Ok(None);
        }

        Ok(Some(resolved))
    }

    fn run_git_bridge_action<F>(&self, action: GitBridgeAction, operation: F) -> Result<String>
    where
        F: FnOnce() -> Result<String>,
    {
        self.assert_initialized()?;
        self.assert_git_initialized()?;

        match self
            .assert_shadow_mode_safe(&action)
            .and_then(|_| operation())
        {
            Ok(detail) => {
                self.append_git_bridge_event(action, true, detail.clone())?;
                Ok(detail)
            }
            Err(err) => {
                let failure_detail = format!("{:#}", err);
                if let Err(log_err) =
                    self.append_git_bridge_event(action.clone(), false, failure_detail)
                {
                    return Err(err.context(format!(
                        "failed to record git bridge failure event: {log_err:#}"
                    )));
                }
                Err(err)
            }
        }
    }

    fn append_git_bridge_event(
        &self,
        action: GitBridgeAction,
        success: bool,
        detail: String,
    ) -> Result<()> {
        self.append_event(EventKind::GitBridge(GitBridgeEvent {
            action,
            success,
            detail,
        }))?;
        Ok(())
    }

    fn resolve_git_remote_name(&self, remote: Option<&str>) -> Result<String> {
        if let Some(remote) = remote.map(str::trim).filter(|value| !value.is_empty()) {
            return Ok(remote.to_string());
        }

        let output = self.run_git(&["remote"])?;
        let remotes: Vec<&str> = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();

        if remotes.is_empty() {
            bail!("no git remotes configured; pass a remote name explicitly");
        }

        if remotes.len() == 1 {
            return Ok(remotes[0].to_string());
        }

        if remotes.iter().any(|remote| *remote == "origin") {
            return Ok("origin".to_string());
        }

        bail!(
            "multiple git remotes found ({}); pass a remote name explicitly",
            remotes.join(", ")
        )
    }

    fn resolve_git_branch_name(&self, branch: Option<&str>) -> Result<String> {
        if let Some(branch) = branch.map(str::trim).filter(|value| !value.is_empty()) {
            return Ok(branch.to_string());
        }

        let current = self.run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        let current = current.trim();
        if current.is_empty() || current == "HEAD" {
            bail!("branch not provided and repository is in detached HEAD state");
        }

        Ok(current.to_string())
    }

    fn push_colocated_refs_to_remote(&self, remote: &str) -> Result<Option<String>> {
        let refs = self.list_local_colocated_refs()?;
        if refs.is_empty() {
            return Ok(None);
        }

        let mut args = vec!["push".to_string(), remote.to_string()];
        args.extend(refs.iter().map(|git_ref| format!("{git_ref}:{git_ref}")));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let detail = self.run_git(&arg_refs)?;
        Ok(Some(format!(
            "push {remote} refs/flock/*\n{}",
            detail.trim()
        )))
    }

    fn fetch_colocated_refs_from_remote(&self, remote: &str) -> Result<Option<String>> {
        let refs = self.list_remote_colocated_refs(remote)?;
        if refs.is_empty() {
            return Ok(None);
        }

        let mut args = vec!["fetch".to_string(), remote.to_string()];
        args.extend(refs.iter().map(|git_ref| format!("{git_ref}:{git_ref}")));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let detail = self.run_git(&arg_refs)?;
        Ok(Some(format!(
            "fetch {remote} refs/flock/*\n{}",
            detail.trim()
        )))
    }

    fn list_local_colocated_refs(&self) -> Result<Vec<String>> {
        let output = self.run_git(&["for-each-ref", "--format=%(refname)", "refs/flock/"])?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    fn list_remote_colocated_refs(&self, remote: &str) -> Result<Vec<String>> {
        let output = self.run_git(&[
            "ls-remote",
            "--refs",
            remote,
            "refs/flock/branches/*",
            "refs/flock/tags/*",
            "refs/flock/workspaces/*",
        ])?;

        let mut refs = BTreeSet::new();
        for line in output.lines() {
            let Some((_, ref_name)) = line.split_once('\t') else {
                continue;
            };
            let ref_name = ref_name.trim();
            if !ref_name.is_empty() {
                refs.insert(ref_name.to_string());
            }
        }

        Ok(refs.into_iter().collect())
    }

    fn ensure_git_repository(&self) -> Result<()> {
        if self.root.join(".git").is_dir() {
            return Ok(());
        }

        self.run_git(&["init"])?;
        Ok(())
    }

    fn repo_mode(&self) -> Result<RepoMode> {
        let config_path = self.root.join(CONFIG_FILE);
        if !config_path.is_file() {
            return Ok(RepoMode::GitCompatible);
        }

        let raw = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        Ok(parse_repo_mode(&raw))
    }

    fn commit_checkpoint_to_git(&self, message: Option<&str>, label: &str) -> Result<String> {
        self.assert_git_initialized()?;
        self.run_git(&["add", "-A"])?;

        let commit_message = message
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("fl checkpoint {}", label));
        self.run_git(&[
            "-c",
            "user.name=Flock",
            "-c",
            "user.email=flock@local",
            "commit",
            "--allow-empty",
            "-m",
            &commit_message,
        ])?;

        let commit_sha = self.run_git(&["rev-parse", "HEAD"])?;
        Ok(commit_sha.trim().to_string())
    }

    fn ensure_git_exclude_entry(&self, entry: &str) -> Result<()> {
        let exclude_path = self.root.join(".git").join("info").join("exclude");
        let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
        if existing
            .lines()
            .map(str::trim)
            .any(|line| line == entry || line == entry.trim_end_matches('/'))
        {
            return Ok(());
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclude_path)
            .with_context(|| format!("failed to open {}", exclude_path.display()))?;
        if !existing.is_empty() && !existing.ends_with('\n') {
            file.write_all(b"\n")
                .with_context(|| format!("failed to write {}", exclude_path.display()))?;
        }
        file.write_all(format!("{entry}\n").as_bytes())
            .with_context(|| format!("failed to write {}", exclude_path.display()))?;
        Ok(())
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
        let reference = RepoRef {
            kind: RefKind::Branch,
            name: "main".to_string(),
            target_event_id: checkpoint_event_id,
            workspace: None,
        };
        store.upsert(reference.clone())?;
        self.sync_ref_to_git_if_colocated(&reference)
    }

    fn sync_ref_to_git_if_colocated(&self, reference: &RepoRef) -> Result<()> {
        if self.repo_mode()? != RepoMode::GitColocated {
            return Ok(());
        }

        self.assert_git_initialized()?;
        let git_ref = git_ref_name(reference.kind, &reference.name);
        let commit = self.resolve_git_commit_for_event(reference.target_event_id)?;
        self.run_git(&["update-ref", &git_ref, &commit])?;
        Ok(())
    }

    fn delete_git_ref_if_colocated(&self, kind: RefKind, name: &str) -> Result<()> {
        if self.repo_mode()? != RepoMode::GitColocated {
            return Ok(());
        }

        self.assert_git_initialized()?;
        let git_ref = git_ref_name(kind, name);

        // Ignore missing refs so Flock metadata remains the source of truth.
        let _ = self.run_git(&["update-ref", "-d", &git_ref]);
        Ok(())
    }

    fn resolve_git_commit_for_event(&self, event_id: Uuid) -> Result<String> {
        let events = self.list_events()?;
        let target_index = events
            .iter()
            .position(|event| event.id == event_id)
            .ok_or_else(|| anyhow!("event {} not found", event_id))?;

        let checkpoint_id = events[..=target_index]
            .iter()
            .rev()
            .find_map(|event| match event.kind {
                EventKind::Checkpoint(_) => Some(event.id),
                _ => None,
            })
            .ok_or_else(|| {
                anyhow!(
                    "cannot map ref target {} to git: no checkpoint exists at or before the target event",
                    event_id
                )
            })?;

        self.git_commit_for_checkpoint(checkpoint_id, &events)
    }

    fn git_commit_for_checkpoint(
        &self,
        checkpoint_event_id: Uuid,
        events: &[Event],
    ) -> Result<String> {
        for event in events {
            let EventKind::GitBridge(bridge) = &event.kind else {
                continue;
            };
            if bridge.action != GitBridgeAction::Commit {
                continue;
            }

            if let Some(commit) = parse_checkpoint_git_mapping(&bridge.detail, checkpoint_event_id)
            {
                return Ok(commit);
            }
        }

        bail!(
            "checkpoint {} has no git commit mapping event; cannot sync git refs",
            checkpoint_event_id
        )
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

fn copy_tree(source_root: &Path, destination_root: &Path, apply_skip: bool) -> Result<()> {
    let walker = WalkDir::new(source_root)
        .into_iter()
        .filter_entry(|entry| !apply_skip || !should_skip_path(source_root, entry.path()));

    for entry in walker {
        let entry = entry.context("failed while walking source tree for copy")?;
        let path = entry.path();
        if path == source_root {
            continue;
        }

        let rel = path
            .strip_prefix(source_root)
            .context("failed to compute relative path while copying tree")?;
        if apply_skip && should_skip_relative(rel) {
            continue;
        }

        let target = destination_root.join(rel);
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

fn clear_directory_except(root: &Path, keep_names: &[&str]) -> Result<()> {
    for entry in fs::read_dir(root)
        .with_context(|| format!("failed to read directory {}", root.display()))?
    {
        let entry = entry.context("failed to read directory entry")?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if keep_names.iter().any(|keep| *keep == file_name) {
            continue;
        }

        let path = entry.path();
        if entry
            .metadata()
            .with_context(|| format!("failed to stat {}", path.display()))?
            .is_dir()
        {
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove directory {}", path.display()))?;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove file {}", path.display()))?;
        }
    }
    Ok(())
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

fn enrich_semantic_impacts(
    repo_root: &Path,
    current_files: &BTreeSet<PathBuf>,
    diffs: &mut [SemanticFileDiff],
) -> Result<()> {
    if diffs.is_empty() {
        return Ok(());
    }

    let reverse_dependencies = build_reverse_dependency_index(repo_root, current_files)?;
    for diff in diffs {
        let changed_path = PathBuf::from(&diff.path);
        let impacted_files = collect_impacted_files(&changed_path, &reverse_dependencies);
        let impacted_file_values: Vec<String> = impacted_files
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect();
        let impacted_module_values: Vec<String> = impacted_files
            .iter()
            .map(|path| module_name_for_path(path))
            .collect();

        for change in &mut diff.changes {
            extend_unique(
                &mut change.impact.files,
                impacted_file_values.iter().cloned(),
            );
            extend_unique(
                &mut change.impact.modules,
                impacted_module_values.iter().cloned(),
            );
        }
    }

    Ok(())
}

fn extend_unique(target: &mut Vec<String>, values: impl IntoIterator<Item = String>) {
    let mut combined = BTreeSet::new();
    for value in target.drain(..) {
        if !value.trim().is_empty() {
            combined.insert(value);
        }
    }
    for value in values {
        if !value.trim().is_empty() {
            combined.insert(value);
        }
    }
    *target = combined.into_iter().collect();
}

fn collect_impacted_files(
    changed_file: &Path,
    reverse_dependencies: &HashMap<PathBuf, BTreeSet<PathBuf>>,
) -> BTreeSet<PathBuf> {
    let mut impacted = BTreeSet::new();
    let mut stack = vec![changed_file.to_path_buf()];
    impacted.insert(changed_file.to_path_buf());

    while let Some(current) = stack.pop() {
        let Some(dependents) = reverse_dependencies.get(&current) else {
            continue;
        };

        for dependent in dependents {
            if impacted.insert(dependent.clone()) {
                stack.push(dependent.clone());
            }
        }
    }

    impacted
}

fn build_reverse_dependency_index(
    repo_root: &Path,
    current_files: &BTreeSet<PathBuf>,
) -> Result<HashMap<PathBuf, BTreeSet<PathBuf>>> {
    let mut reverse = HashMap::<PathBuf, BTreeSet<PathBuf>>::new();

    for importer in current_files {
        let source_path = repo_root.join(importer);
        let source = fs::read_to_string(&source_path).with_context(|| {
            format!(
                "failed to read source file while building semantic impact graph: {}",
                source_path.display()
            )
        })?;

        for specifier in extract_local_import_specifiers(&source) {
            for target in resolve_import_targets(importer, &specifier, current_files) {
                reverse.entry(target).or_default().insert(importer.clone());
            }
        }
    }

    Ok(reverse)
}

fn extract_local_import_specifiers(source: &str) -> Vec<String> {
    let mut specifiers = BTreeSet::new();
    collect_specifiers_for_token(
        source,
        "from",
        |source, token_end| parse_quoted_literal(source, token_end).map(|(value, _)| value),
        &mut specifiers,
    );
    collect_specifiers_for_token(
        source,
        "require(",
        |source, token_end| parse_quoted_literal(source, token_end).map(|(value, _)| value),
        &mut specifiers,
    );
    collect_specifiers_for_token(
        source,
        "import(",
        |source, token_end| parse_quoted_literal(source, token_end).map(|(value, _)| value),
        &mut specifiers,
    );

    specifiers
        .into_iter()
        .filter(|value| {
            value.starts_with("./") || value.starts_with("../") || value.starts_with('/')
        })
        .collect()
}

fn collect_specifiers_for_token(
    source: &str,
    token: &str,
    parser: impl Fn(&str, usize) -> Option<String>,
    output: &mut BTreeSet<String>,
) {
    let bytes = source.as_bytes();
    let mut start = 0usize;

    while let Some(offset) = source[start..].find(token) {
        let token_start = start + offset;
        let token_end = token_start + token.len();

        if token == "from"
            && (is_identifier_byte(bytes.get(token_start.wrapping_sub(1)).copied())
                || is_identifier_byte(bytes.get(token_end).copied()))
        {
            start = token_end;
            continue;
        }

        if let Some(value) = parser(source, token_end) {
            let normalized = value
                .split(['?', '#'])
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            if !normalized.is_empty() {
                output.insert(normalized);
            }
        }

        start = token_end;
    }
}

fn parse_quoted_literal(source: &str, mut index: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    while matches!(bytes.get(index), Some(ch) if ch.is_ascii_whitespace()) {
        index += 1;
    }

    let quote = match bytes.get(index).copied() {
        Some(b'"') | Some(b'\'') => bytes[index],
        _ => return None,
    };

    let mut cursor = index + 1;
    while let Some(ch) = bytes.get(cursor).copied() {
        if ch == quote && bytes.get(cursor.wrapping_sub(1)).copied() != Some(b'\\') {
            let value = source[index + 1..cursor].to_string();
            return Some((value, cursor + 1));
        }
        cursor += 1;
    }

    None
}

fn is_identifier_byte(byte: Option<u8>) -> bool {
    matches!(byte, Some(ch) if ch.is_ascii_alphanumeric() || ch == b'_')
}

fn resolve_import_targets(
    importer: &Path,
    specifier: &str,
    current_files: &BTreeSet<PathBuf>,
) -> Vec<PathBuf> {
    let base = if specifier.starts_with('/') {
        PathBuf::from(specifier.trim_start_matches('/'))
    } else {
        importer
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(specifier)
    };
    let base = normalize_relative_path(&base);

    let mut candidates = Vec::new();
    if base.extension().is_some() {
        candidates.push(base);
    } else {
        for ext in ["ts", "tsx", "js", "jsx"] {
            candidates.push(base.with_extension(ext));
            candidates.push(base.join(format!("index.{ext}")));
        }
    }

    let mut resolved = BTreeSet::new();
    for candidate in candidates {
        if current_files.contains(&candidate) {
            resolved.insert(candidate);
        }
    }

    resolved.into_iter().collect()
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    normalized
}

fn module_name_for_path(path: &Path) -> String {
    let Some(parent) = path.parent() else {
        return "(root)".to_string();
    };
    if parent.as_os_str().is_empty() {
        "(root)".to_string()
    } else {
        parent.to_string_lossy().to_string()
    }
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

fn join_non_empty_lines(lines: Vec<String>) -> String {
    lines
        .into_iter()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<String>>()
        .join("\n")
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

fn git_ref_name(kind: RefKind, name: &str) -> String {
    match kind {
        RefKind::Branch => format!("refs/flock/branches/{name}"),
        RefKind::Tag => format!("refs/flock/tags/{name}"),
        RefKind::Workspace => format!("refs/flock/workspaces/{name}"),
    }
}

fn parse_checkpoint_git_mapping(detail: &str, checkpoint_id: Uuid) -> Option<String> {
    let mut mapped_checkpoint = None;
    let mut mapped_commit = None;
    let checkpoint_id = checkpoint_id.to_string();

    for token in detail.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        match key {
            "checkpoint" => mapped_checkpoint = Some(value),
            "git_commit" => mapped_commit = Some(value),
            _ => {}
        }
    }

    if mapped_checkpoint == Some(checkpoint_id.as_str()) {
        return mapped_commit.map(ToOwned::to_owned);
    }
    None
}

fn parse_git_commit_from_detail(detail: &str) -> Option<String> {
    for token in detail.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        if key == "git_commit" {
            return Some(value.to_string());
        }
    }
    None
}

fn short_sha(sha: &str) -> &str {
    let end = std::cmp::min(8, sha.len());
    &sha[..end]
}

fn git_action_name(action: &GitBridgeAction) -> &'static str {
    match action {
        GitBridgeAction::Commit => "commit",
        GitBridgeAction::Push => "push",
        GitBridgeAction::Pull => "pull",
        GitBridgeAction::Import => "import",
        GitBridgeAction::Export => "export",
    }
}

fn parse_repo_mode(raw_config: &str) -> RepoMode {
    for line in raw_config.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim() != "mode" {
            continue;
        }

        let value = value.trim().trim_matches('"');
        return match value {
            "git-colocated" => RepoMode::GitColocated,
            _ => RepoMode::GitCompatible,
        };
    }

    RepoMode::GitCompatible
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

fn classify_diffs_by_intent(
    diffs: Vec<SemanticFileDiff>,
) -> Vec<(String, Vec<SemanticFileDiff>)> {
    let mut groups: HashMap<String, Vec<SemanticFileDiff>> = HashMap::new();

    for diff in diffs {
        let intent = classify_single_diff_intent(&diff);
        groups.entry(intent).or_default().push(diff);
    }

    let order = [
        "Breaking change",
        "New feature",
        "Bug fix",
        "Refactor",
        "Removal",
        "Mixed",
    ];
    let mut result = Vec::new();
    for label in order {
        if let Some(files) = groups.remove(label) {
            result.push((label.to_string(), files));
        }
    }
    // Any remaining categories
    for (label, files) in groups {
        result.push((label, files));
    }
    result
}

fn classify_single_diff_intent(diff: &SemanticFileDiff) -> String {
    use crate::semantic::{SemanticChangeKind, SemanticCompatibilityStatus};

    if diff.changes.is_empty() {
        return "Mixed".to_string();
    }

    // Check for breaking changes first
    let has_breaking = diff.changes.iter().any(|c| {
        matches!(
            c.compatibility.status,
            SemanticCompatibilityStatus::Breaking | SemanticCompatibilityStatus::PotentiallyBreaking
        )
    });
    if has_breaking {
        return "Breaking change".to_string();
    }

    let all_added = diff
        .changes
        .iter()
        .all(|c| matches!(c.kind, SemanticChangeKind::Added));
    if all_added {
        return "New feature".to_string();
    }

    let all_removed = diff
        .changes
        .iter()
        .all(|c| matches!(c.kind, SemanticChangeKind::Removed));
    if all_removed {
        return "Removal".to_string();
    }

    let all_refactor = diff.changes.iter().all(|c| {
        matches!(
            c.kind,
            SemanticChangeKind::Renamed | SemanticChangeKind::Moved | SemanticChangeKind::StyleOnly
        )
    });
    if all_refactor {
        return "Refactor".to_string();
    }

    let all_low_risk_modified = diff.changes.iter().all(|c| {
        matches!(c.kind, SemanticChangeKind::Modified)
            && matches!(c.risk, crate::semantic::SemanticRisk::Low)
    });
    if all_low_risk_modified {
        return "Bug fix".to_string();
    }

    "Mixed".to_string()
}

fn compute_review_stats(diffs: &[SemanticFileDiff]) -> ReviewStats {
    use crate::semantic::{SemanticChangeKind, SemanticCompatibilityStatus, SemanticRisk};

    let mut stats = ReviewStats {
        files_changed: diffs.len(),
        symbols_added: 0,
        symbols_removed: 0,
        symbols_modified: 0,
        high_risk_count: 0,
        breaking_count: 0,
    };

    for diff in diffs {
        for change in &diff.changes {
            match change.kind {
                SemanticChangeKind::Added => stats.symbols_added += 1,
                SemanticChangeKind::Removed => stats.symbols_removed += 1,
                _ => stats.symbols_modified += 1,
            }
            if matches!(change.risk, SemanticRisk::High) {
                stats.high_risk_count += 1;
            }
            if matches!(
                change.compatibility.status,
                SemanticCompatibilityStatus::Breaking
                    | SemanticCompatibilityStatus::PotentiallyBreaking
            ) {
                stats.breaking_count += 1;
            }
        }
    }

    stats
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
    fn semantic_diff_tracks_impacted_files_and_modules() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let source_dir = dir.path().join("src");
        fs::create_dir_all(&source_dir).expect("source dir should be created");
        let lib_file = source_dir.join("lib.ts");
        let app_file = source_dir.join("app.ts");

        fs::write(&lib_file, "export function add(a, b) { return a + b; }")
            .expect("lib seed write should succeed");
        fs::write(
            &app_file,
            "import { add } from './lib'; export const total = add(1, 2);",
        )
        .expect("app seed write should succeed");

        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&lib_file, "export function add(a, b) { return a - b; }")
            .expect("lib update should succeed");

        let diffs = repo
            .semantic_diff_from_latest_checkpoint()
            .expect("semantic diff should succeed");
        let lib_diff = diffs
            .iter()
            .find(|diff| diff.path == "src/lib.ts")
            .expect("lib diff should exist");
        let modified = lib_diff
            .changes
            .iter()
            .find(|change| change.kind == crate::semantic::SemanticChangeKind::Modified)
            .expect("modified semantic change should exist");

        assert!(
            modified
                .impact
                .files
                .iter()
                .any(|path| path == "src/lib.ts")
        );
        assert!(
            modified
                .impact
                .files
                .iter()
                .any(|path| path == "src/app.ts")
        );
        assert!(modified.impact.modules.iter().any(|module| module == "src"));
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
    fn init_colocated_creates_git_repository() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        assert!(dir.path().join(".git").is_dir());
        let config = fs::read_to_string(dir.path().join(CONFIG_FILE)).expect("config should read");
        assert!(config.contains("mode = \"git-colocated\""));
        let exclude =
            fs::read_to_string(dir.path().join(".git/info/exclude")).expect("exclude should read");
        assert!(exclude.lines().any(|line| line.trim() == ".flock/"));
    }

    #[test]
    fn colocated_checkpoint_creates_git_commit_and_mapping_event() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        let file = dir.path().join("colocated.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        let checkpoint = repo
            .create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");
        let head = repo
            .run_git(&["rev-parse", "HEAD"])
            .expect("head should resolve");
        let head = head.trim().to_string();

        let events = repo.list_events().expect("events should load");
        let mapping = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::GitBridge(bridge) if bridge.action == GitBridgeAction::Commit => {
                    Some(bridge.detail.clone())
                }
                _ => None,
            })
            .expect("commit mapping event should exist");
        assert!(mapping.contains(&checkpoint.id.to_string()));
        assert!(mapping.contains(&head));
    }

    #[test]
    fn colocated_checkpoint_updates_main_git_ref_mapping() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        let file = dir.path().join("main-ref.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        let head = repo
            .run_git(&["rev-parse", "HEAD"])
            .expect("head should resolve");
        let mapped = repo
            .run_git(&["rev-parse", "refs/flock/branches/main"])
            .expect("main mapping should resolve");

        assert_eq!(mapped.trim(), head.trim());
    }

    #[test]
    fn colocated_refs_set_and_delete_sync_git_ref_namespace() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        let file = dir.path().join("refs-sync.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        let checkpoint = repo
            .create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        let head = repo
            .run_git(&["rev-parse", "HEAD"])
            .expect("head should resolve");
        let head = head.trim().to_string();

        repo.upsert_ref(
            RefKind::Branch,
            "release/1".to_string(),
            checkpoint.id.to_string(),
            None,
        )
        .expect("branch ref should sync");
        repo.upsert_ref(
            RefKind::Tag,
            "v1.0.0".to_string(),
            checkpoint.id.to_string(),
            None,
        )
        .expect("tag ref should sync");
        repo.upsert_ref(
            RefKind::Workspace,
            "agent/a".to_string(),
            checkpoint.id.to_string(),
            Some(true),
        )
        .expect("workspace ref should sync");

        let branch_sha = repo
            .run_git(&["rev-parse", &git_ref_name(RefKind::Branch, "release/1")])
            .expect("branch git ref should resolve");
        let tag_sha = repo
            .run_git(&["rev-parse", &git_ref_name(RefKind::Tag, "v1.0.0")])
            .expect("tag git ref should resolve");
        let workspace_sha = repo
            .run_git(&["rev-parse", &git_ref_name(RefKind::Workspace, "agent/a")])
            .expect("workspace git ref should resolve");

        assert_eq!(branch_sha.trim(), head);
        assert_eq!(tag_sha.trim(), head);
        assert_eq!(workspace_sha.trim(), head);

        repo.delete_ref(RefKind::Workspace, "agent/a")
            .expect("workspace should delete");
        assert!(
            repo.run_git(&["rev-parse", &git_ref_name(RefKind::Workspace, "agent/a")])
                .is_err()
        );
    }

    #[test]
    fn colocated_ref_target_non_checkpoint_maps_to_previous_checkpoint_commit() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        let file = dir.path().join("workspace-map.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");
        let head = repo
            .run_git(&["rev-parse", "HEAD"])
            .expect("head should resolve");
        let head = head.trim().to_string();

        repo.start_exploration("scratch".to_string())
            .expect("exploration should start");
        let exploration_event_id = repo
            .list_events()
            .expect("events should load")
            .last()
            .expect("exploration event should exist")
            .id;

        repo.upsert_ref(
            RefKind::Workspace,
            "agent/b".to_string(),
            exploration_event_id.to_string(),
            Some(false),
        )
        .expect("workspace ref should map");

        let mapped = repo
            .run_git(&["rev-parse", &git_ref_name(RefKind::Workspace, "agent/b")])
            .expect("workspace git ref should resolve");
        assert_eq!(mapped.trim(), head);
    }

    #[test]
    fn git_shadow_status_reports_head_ref_drift() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        let file = dir.path().join("shadow-drift.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&file, "export const value = 2;").expect("write should succeed");
        repo.run_git(&["add", "-A"])
            .expect("git add should succeed");
        repo.run_git(&[
            "-c",
            "user.name=Tester",
            "-c",
            "user.email=tester@example.com",
            "commit",
            "-m",
            "manual git commit",
        ])
        .expect("manual git commit should succeed");

        let report = repo.git_shadow_status().expect("status should succeed");
        assert!(!report.clean);
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "head/ref alignment" && !check.ok),
            "expected head/ref alignment failure: {:?}",
            report.checks
        );
    }

    #[test]
    fn git_push_blocks_when_shadow_mode_is_out_of_sync() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        let file = dir.path().join("shadow-block.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&file, "export const value = 2;").expect("write should succeed");
        repo.run_git(&["add", "-A"])
            .expect("git add should succeed");
        repo.run_git(&[
            "-c",
            "user.name=Tester",
            "-c",
            "user.email=tester@example.com",
            "commit",
            "-m",
            "manual git commit",
        ])
        .expect("manual git commit should succeed");

        let err = repo
            .git_push(None, None)
            .expect_err("push should fail shadow mode preflight");
        assert!(
            format!("{:#}", err).contains("shadow mode safety check failed"),
            "unexpected error: {err}"
        );

        let events = repo.list_events().expect("events should load");
        assert!(events.iter().any(|event| {
            matches!(
                event.kind,
                EventKind::GitBridge(GitBridgeEvent {
                    action: GitBridgeAction::Push,
                    success: false,
                    ..
                })
            )
        }));
    }

    #[test]
    fn git_push_syncs_branch_and_flock_refs_to_remote() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir should be created");
        fl_bridge_git::run_git(remote_dir.path(), &["init", "--bare"])
            .expect("remote bare repo should initialize");

        let local_dir = tempfile::tempdir().expect("local tempdir should be created");
        let repo = Repo::at(local_dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        let file = local_dir.path().join("push-sync.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");
        let remote_path = remote_dir.path().to_string_lossy().to_string();

        repo.run_git(&["remote", "add", "origin", &remote_path])
            .expect("origin remote should be added");
        let branch = repo
            .run_git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .expect("branch should resolve")
            .trim()
            .to_string();
        let head = repo
            .run_git(&["rev-parse", "HEAD"])
            .expect("head should resolve")
            .trim()
            .to_string();

        repo.git_push(Some("origin".to_string()), Some(branch.clone()))
            .expect("git push should succeed");

        let remote_branch = fl_bridge_git::run_git(
            remote_dir.path(),
            &["rev-parse", &format!("refs/heads/{branch}")],
        )
        .expect("remote branch should resolve");
        let remote_flock_main = fl_bridge_git::run_git(
            remote_dir.path(),
            &["rev-parse", "refs/flock/branches/main"],
        )
        .expect("remote flock main ref should resolve");

        assert_eq!(remote_branch.trim(), head);
        assert_eq!(remote_flock_main.trim(), head);

        let events = repo.list_events().expect("events should load");
        assert!(events.iter().any(|event| {
            matches!(
                event.kind,
                EventKind::GitBridge(GitBridgeEvent {
                    action: GitBridgeAction::Push,
                    success: true,
                    ..
                })
            )
        }));
    }

    #[test]
    fn git_pull_fetches_remote_branch_and_flock_refs() {
        let remote_dir = tempfile::tempdir().expect("remote tempdir should be created");
        fl_bridge_git::run_git(remote_dir.path(), &["init", "--bare"])
            .expect("remote bare repo should initialize");

        let source_dir = tempfile::tempdir().expect("source tempdir should be created");
        let source_repo = Repo::at(source_dir.path());
        source_repo
            .init_colocated()
            .expect("source colocated init should succeed");

        let source_file = source_dir.path().join("pull-sync.ts");
        fs::write(&source_file, "export const value = 1;").expect("write should succeed");
        source_repo
            .create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");
        let remote_path = remote_dir.path().to_string_lossy().to_string();
        source_repo
            .run_git(&["remote", "add", "origin", &remote_path])
            .expect("origin remote should be added");
        let branch = source_repo
            .run_git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .expect("branch should resolve")
            .trim()
            .to_string();
        source_repo
            .git_push(Some("origin".to_string()), Some(branch.clone()))
            .expect("initial push should succeed");

        let clone_parent = tempfile::tempdir().expect("clone parent tempdir should be created");
        let clone_path = clone_parent.path().join("clone");
        let clone_path_string = clone_path.to_string_lossy().to_string();
        fl_bridge_git::run_git(
            clone_parent.path(),
            &["clone", &remote_path, &clone_path_string],
        )
        .expect("clone should succeed");

        let clone_repo = Repo::at(&clone_path);
        clone_repo
            .init_colocated()
            .expect("clone colocated init should succeed");

        fs::write(&source_file, "export const value = 2;").expect("write should succeed");
        let cp2 = source_repo
            .create_checkpoint(Some("cp2".to_string()))
            .expect("second checkpoint should succeed");
        source_repo
            .upsert_ref(
                RefKind::Workspace,
                "agent/sync".to_string(),
                cp2.id.to_string(),
                Some(true),
            )
            .expect("workspace ref should sync to git refs namespace");
        let expected_head = source_repo
            .run_git(&["rev-parse", "HEAD"])
            .expect("source head should resolve")
            .trim()
            .to_string();
        source_repo
            .git_push(Some("origin".to_string()), Some(branch.clone()))
            .expect("second push should succeed");

        clone_repo
            .git_pull(Some("origin".to_string()), Some(branch))
            .expect("git pull should succeed");

        let clone_head = clone_repo
            .run_git(&["rev-parse", "HEAD"])
            .expect("clone head should resolve");
        let clone_workspace_ref = clone_repo
            .run_git(&["rev-parse", &git_ref_name(RefKind::Workspace, "agent/sync")])
            .expect("clone workspace flock ref should resolve");

        assert_eq!(clone_head.trim(), expected_head);
        assert_eq!(clone_workspace_ref.trim(), expected_head);

        let events = clone_repo.list_events().expect("events should load");
        assert!(events.iter().any(|event| {
            matches!(
                event.kind,
                EventKind::GitBridge(GitBridgeEvent {
                    action: GitBridgeAction::Pull,
                    success: true,
                    ..
                })
            )
        }));
    }

    #[test]
    fn git_import_replays_commit_history_into_checkpoints() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        fl_bridge_git::run_git(dir.path(), &["init"]).expect("git init should succeed");
        let file = dir.path().join("import.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        fl_bridge_git::run_git(dir.path(), &["add", "import.ts"]).expect("git add should succeed");
        fl_bridge_git::run_git(
            dir.path(),
            &[
                "-c",
                "user.name=Tester",
                "-c",
                "user.email=tester@example.com",
                "commit",
                "-m",
                "first",
            ],
        )
        .expect("first commit should succeed");

        fs::write(&file, "export const value = 2;").expect("write should succeed");
        fl_bridge_git::run_git(dir.path(), &["add", "import.ts"]).expect("git add should succeed");
        fl_bridge_git::run_git(
            dir.path(),
            &[
                "-c",
                "user.name=Tester",
                "-c",
                "user.email=tester@example.com",
                "commit",
                "-m",
                "second",
            ],
        )
        .expect("second commit should succeed");

        repo.git_import(None).expect("git import should succeed");

        let events = repo.list_events().expect("events should load");
        let checkpoints: Vec<&Event> = events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::Checkpoint(_)))
            .collect();
        assert_eq!(checkpoints.len(), 2);
        let EventKind::Checkpoint(last_checkpoint) = &checkpoints[1].kind else {
            panic!("checkpoint payload expected");
        };
        let restored = fs::read_to_string(
            repo.snapshot_path(last_checkpoint.snapshot_id)
                .join("import.ts"),
        )
        .expect("snapshot file should be readable");
        assert_eq!(restored, "export const value = 2;");

        assert!(events.iter().any(|event| {
            matches!(
                event.kind,
                EventKind::GitBridge(GitBridgeEvent {
                    action: GitBridgeAction::Import,
                    success: true,
                    ..
                })
            )
        }));
    }

    #[test]
    fn git_import_is_idempotent_for_existing_commit_mappings() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        fl_bridge_git::run_git(dir.path(), &["init"]).expect("git init should succeed");
        let file = dir.path().join("import-once.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        fl_bridge_git::run_git(dir.path(), &["add", "import-once.ts"])
            .expect("git add should succeed");
        fl_bridge_git::run_git(
            dir.path(),
            &[
                "-c",
                "user.name=Tester",
                "-c",
                "user.email=tester@example.com",
                "commit",
                "-m",
                "first",
            ],
        )
        .expect("commit should succeed");

        repo.git_import(None).expect("first import should succeed");
        let first_checkpoint_count = repo
            .list_events()
            .expect("events should load")
            .iter()
            .filter(|event| matches!(event.kind, EventKind::Checkpoint(_)))
            .count();

        let second = repo.git_import(None).expect("second import should succeed");
        let second_checkpoint_count = repo
            .list_events()
            .expect("events should load")
            .iter()
            .filter(|event| matches!(event.kind, EventKind::Checkpoint(_)))
            .count();

        assert_eq!(first_checkpoint_count, second_checkpoint_count);
        assert!(
            second.contains("no new commits"),
            "unexpected output: {second}"
        );
    }

    #[test]
    fn git_export_writes_checkpoint_history_to_target_branch() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        let file = dir.path().join("export.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");
        fs::write(&file, "export const value = 2;").expect("write should succeed");
        repo.create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");

        repo.git_export(Some("flock-export".to_string()))
            .expect("git export should succeed");

        let export_count = repo
            .run_git(&["rev-list", "--count", "refs/heads/flock-export"])
            .expect("export branch count should resolve");
        assert_eq!(export_count.trim(), "2");
        let exported_file = repo
            .run_git(&["show", "refs/heads/flock-export:export.ts"])
            .expect("exported file should exist");
        assert_eq!(exported_file.trim(), "export const value = 2;");

        let events = repo.list_events().expect("events should load");
        assert!(events.iter().any(|event| {
            matches!(
                event.kind,
                EventKind::GitBridge(GitBridgeEvent {
                    action: GitBridgeAction::Export,
                    success: true,
                    ..
                })
            )
        }));
    }

    #[test]
    fn git_push_failure_records_failed_event() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_colocated()
            .expect("colocated init should succeed");

        let file = dir.path().join("push-fail.ts");
        fs::write(&file, "export const value = 1;").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        let err = repo
            .git_push(Some("origin".to_string()), None)
            .expect_err("push should fail without configured origin");
        assert!(
            format!("{:#}", err).contains("failed to push branch"),
            "unexpected error: {err}"
        );

        let events = repo.list_events().expect("events should load");
        assert!(events.iter().any(|event| {
            matches!(
                event.kind,
                EventKind::GitBridge(GitBridgeEvent {
                    action: GitBridgeAction::Push,
                    success: false,
                    ..
                })
            )
        }));
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

    #[test]
    fn explore_compare_shows_differences_between_explorations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        // Base state
        let file = dir.path().join("app.ts");
        fs::write(&file, "function greet() { return 'hello'; }").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        // Start first exploration
        let exp1 = repo
            .start_exploration("exp-alpha".to_string())
            .expect("start exp1");

        // Modify and checkpoint (so exp1 has a base snapshot)
        fs::write(&file, "function greet() { return 'hi'; }").expect("write");
        repo.create_checkpoint(Some("exp1-work".to_string()))
            .expect("checkpoint");

        // Start second exploration (its base is the latest checkpoint)
        let exp2 = repo
            .start_exploration("exp-beta".to_string())
            .expect("start exp2");

        // Compare exp1 vs exp2 (different base snapshots)
        let diffs = repo
            .compare_explorations(exp1.id, Some(exp2.id))
            .expect("compare");
        // The base snapshots differ, so there should be a diff
        assert!(
            !diffs.is_empty(),
            "comparing explorations with different bases should show diffs"
        );

        // Compare exp1 against working dir
        let diffs_wd = repo
            .compare_explorations(exp1.id, None)
            .expect("compare vs working dir");
        assert!(
            !diffs_wd.is_empty(),
            "comparing exploration against modified working dir should show diffs"
        );
    }

    #[test]
    fn prune_explorations_removes_old_abandoned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        let file = dir.path().join("hello.ts");
        fs::write(&file, "export const x = 1;").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        let exp = repo
            .start_exploration("to-prune".to_string())
            .expect("start");
        repo.abandon_exploration(exp.id).expect("abandon");

        // Prune with 0 duration = prune everything abandoned
        let pruned = repo
            .prune_explorations(std::time::Duration::from_secs(0))
            .expect("prune");
        assert_eq!(pruned, 1);
    }

    #[test]
    fn quick_save_and_restore_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        let file = dir.path().join("data.ts");
        fs::write(&file, "export const v = 1;").expect("write");
        repo.create_checkpoint(Some("initial".to_string()))
            .expect("checkpoint");

        // Quick save
        fs::write(&file, "export const v = 2;").expect("modify");
        let save_event = repo.quick_save(Some("my-save".to_string())).expect("quick save");
        let EventKind::Checkpoint(payload) = &save_event.kind else {
            panic!("expected checkpoint");
        };
        assert!(payload.label.contains("my-save"));

        // Modify further
        fs::write(&file, "export const v = 3;").expect("modify again");

        // Quick restore - should undo back to the quick-save point
        let result = repo.quick_restore().expect("quick restore");
        assert_eq!(result.target_event_id, save_event.id);
    }

    #[test]
    fn workspace_create_and_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        let file = dir.path().join("main.ts");
        fs::write(&file, "export const x = 1;").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        let ws = repo
            .create_workspace("feature-a".to_string(), true)
            .expect("create workspace");
        assert_eq!(ws.name, "feature-a");
        let config = ws.workspace.as_ref().unwrap();
        assert!(config.auto_rebase);
        assert!(config.base_snapshot_id.is_some());

        let workspaces = repo.list_workspaces().expect("list workspaces");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "feature-a");
    }

    #[test]
    fn workspace_info_and_limits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        let file = dir.path().join("main.ts");
        fs::write(&file, "export const x = 1;").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        repo.create_workspace("ws1".to_string(), false)
            .expect("create workspace");

        let info = repo.workspace_info("ws1").expect("workspace info");
        assert_eq!(info.workspace.name, "ws1");
        assert!(info.event_count > 0);
        assert!(info.limits_exceeded.is_empty());

        // Set very low limits that will be exceeded
        repo.set_workspace_limits("ws1", Some(0), Some(0))
            .expect("set limits");

        let info = repo.workspace_info("ws1").expect("workspace info after limits");
        let config = info.workspace.workspace.as_ref().unwrap();
        assert_eq!(config.max_snapshots, Some(0));
        assert_eq!(config.max_events, Some(0));
        // Both limits should be exceeded
        assert!(
            info.limits_exceeded.len() >= 2,
            "expected at least 2 limit warnings, got: {:?}",
            info.limits_exceeded
        );
    }
}
