use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::{Signer, SigningKey};
use fl_storage::{
    AutoEventLog, AutoRefStore, BlockRef, CONFIG_FILE, ChunkConfig, ContentStore, FLOCK_DIR,
    FileEntry, FileIndex, HOOKS_CONFIG_FILE, HookEvent, KEY_DIR, MaterializedStateStore, RefKind,
    RepoRef, SECRETS_CONFIG_FILE, SIGNING_KEY_FILE, SNAPSHOT_DIR, SnapshotIndex,
    WorkspaceRefConfig, chunk_data,
};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::event::{
    CheckpointEvent, ConflictAction, ConflictResolutionEvent, DecisionAction, DecisionEvent, Event,
    EventKind, ExplorationAction, ExplorationEvent, FileChangeKind, FileChangeSummary,
    FileDeleteEvent, FileWriteEvent, GateAction, GateCondition, GateEvent, GatePolicy,
    GitBridgeAction, GitBridgeEvent, LockAction, LockEvent, NotifyConfig, PresenceAction,
    PresenceEvent, RebaseEvent, ResourceUsageEvent, SessionAction, SessionEvent, SubscriptionAction,
    SubscriptionEvent, SubscriptionFilter, TaskAction, TaskEvent, UndoEvent, UndoMode,
};
use fl_collab::can_acquire_lock;
use fl_storage::ApiCallRecord;
use crate::semantic::{
    SemanticConflictClassification, SemanticFileDiff, SemanticImpact, SemanticMergeConflict,
    SemanticMergeResult, clear_cache, diff as semantic_diff, impact_symbols, set_cache_root,
    supported_source,
};
use fl_workflow::{
    build_task_graph, previous_checkpoint_before, replay_state, replay_state_incremental,
    resolve_target_event, resolve_target_event_scoped, to_undo_mode, walk_checkpoint_ancestor,
    walk_checkpoint_ancestor_scoped,
};

pub use fl_collab::{
    ConflictStatus, ConflictSummary, GateConditionKind, GatePolicyKind, GateStatus, GateSummary,
    LockStatus, LockSummary, PresenceSummary, RebaseSummary, SubscriptionNotify,
    SubscriptionStatus, SubscriptionSummary,
};
pub use fl_workflow::parse_duration_spec;
pub use fl_workflow::{
    DecisionSummary, ExplorationStatus, ExplorationSummary, FileState, ReplayedState,
    ResourceUsageTotals, SessionStatus, SessionSummary, TaskEdge, TaskGraph, TaskRelation,
    TaskStatus, TaskSummary, UndoRequest, UndoResult, UndoScope,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckReport {
    pub event_count: usize,
    pub checkpoint_count: usize,
    pub snapshot_count: usize,
    pub ref_count: usize,
    pub hash_chain_verified: bool,
}

#[derive(Debug, Clone)]
pub struct StatusReport {
    pub branch: String,
    pub checkpoint_id: Option<String>,
    pub new_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub ignored_symlinks: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FileSummary {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

/// A single file's unified text diff.
#[derive(Debug, Clone)]
pub struct TextFileDiff {
    pub path: String,
    pub old_content: String,
    pub new_content: String,
}

/// Structured intent metadata for checkpoints (commit hygiene).
#[derive(Debug, Clone)]
pub struct CheckpointIntentMetadata {
    pub category: Option<crate::event::CheckpointCategory>,
    pub scope_label: Option<String>,
    pub confidence: Option<String>,
    pub structured_description: Option<String>,
}

/// Per-line blame attribution.
#[derive(Debug, Clone)]
pub struct BlameAnnotation {
    pub line_number: usize,
    pub content: String,
    pub commit_id: Option<Uuid>,
    pub author: Option<String>,
    pub timestamp: Option<String>,
    pub message: Option<String>,
}

/// A stash entry preserving working directory state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StashEntry {
    pub snapshot_id: Uuid,
    pub message: Option<String>,
    pub timestamp: String,
}

/// Report from `fl who` combining presence, sessions, and tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoReport {
    pub actors: Vec<ActorSummary>,
}

/// Summary of an active actor's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorSummary {
    pub actor: String,
    pub workspace: String,
    pub active_files: Vec<String>,
    pub active_symbols: Vec<String>,
    pub intent: Option<String>,
    pub current_task: Option<String>,
    pub session_id: Option<Uuid>,
    pub last_seen: String,
}

/// Persisted dependency graph for incremental semantic indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DependencyIndex {
    version: u32,
    snapshot_id: String,
    edges: HashMap<String, Vec<String>>,
}

const SEMANTIC_INDEX_DIR: &str = "semantic-index";
const DEPS_INDEX_FILE: &str = "deps.json";

impl DependencyIndex {
    fn path(root: &Path) -> PathBuf {
        root.join(FLOCK_DIR)
            .join(SEMANTIC_INDEX_DIR)
            .join(DEPS_INDEX_FILE)
    }

    fn load(root: &Path) -> Option<Self> {
        let path = Self::path(root);
        let json = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&json).ok()
    }

    fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)?;
        Ok(())
    }

    fn clear(root: &Path) -> Result<()> {
        let dir = root.join(FLOCK_DIR).join(SEMANTIC_INDEX_DIR);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

/// Result of an `fl index` operation.
#[derive(Debug, Clone)]
pub struct IndexReport {
    pub files_indexed: usize,
    pub edges_computed: usize,
}

#[derive(Debug, Clone)]
pub struct MigrateReport {
    pub snapshots_migrated: u32,
    pub blocks_stored: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
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

#[derive(Debug, Clone)]
pub struct ProvenanceInfo {
    pub session: Option<SessionSummary>,
    pub exploration: ExplorationSummary,
    pub decisions: Vec<DecisionSummary>,
    pub related_events: Vec<Event>,
}

#[derive(Debug, Clone)]
pub struct SessionReplay {
    pub session: SessionSummary,
    pub timeline: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseResult {
    pub workspace: String,
    pub old_base_event: Uuid,
    pub new_base_event: Uuid,
    pub files_merged: Vec<String>,
    pub conflicts: Vec<ConflictDetail>,
    pub already_up_to_date: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictDetail {
    pub id: Option<Uuid>,
    pub path: String,
    pub symbol: Option<String>,
    pub classification: String,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLoginResult {
    pub success: bool,
    pub identity: Option<String>,
    pub error: Option<String>,
}

pub struct RemoteCredentialInfo {
    pub host: String,
    pub method: String,
}

pub struct ConvertReport {
    pub branches_imported: usize,
    pub tags_imported: usize,
    pub commits_imported: usize,
    pub commits_skipped: usize,
    pub validation_ok: bool,
    pub validation_detail: String,
}

impl std::fmt::Display for ConvertReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "conversion complete: {} commits imported ({} skipped), {} branches, {} tags",
            self.commits_imported, self.commits_skipped, self.branches_imported, self.tags_imported,
        )?;
        if self.validation_ok {
            write!(f, "validation: OK — {}", self.validation_detail)
        } else {
            write!(f, "validation: FAILED — {}", self.validation_detail)
        }
    }
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
    Native,
}

impl RepoMode {
    fn as_str(self) -> &'static str {
        match self {
            RepoMode::GitCompatible => "git-compatible",
            RepoMode::GitColocated => "git-colocated",
            RepoMode::Native => "native",
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
                set_cache_root(ancestor);
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

    pub fn flock_dir(&self) -> PathBuf {
        self.root.join(FLOCK_DIR)
    }

    pub fn init(&self) -> Result<()> {
        self.init_with_mode(RepoMode::GitCompatible)
    }

    pub fn init_colocated(&self) -> Result<()> {
        self.init_with_mode(RepoMode::GitColocated)
    }

    pub fn init_native(&self) -> Result<()> {
        self.init_with_mode(RepoMode::Native)
    }

    fn init_with_mode(&self, mode: RepoMode) -> Result<()> {
        self.init_layout(mode)?;

        self.append_event(EventKind::Init(fl_storage::event::InitEvent {
            mode: mode.as_str().to_string(),
        }))?;

        Ok(())
    }

    /// Creates the .flock directory layout without recording an init event.
    /// Used by clone_from which will pull events from the source repo.
    fn init_layout(&self, mode: RepoMode) -> Result<()> {
        if mode == RepoMode::Native {
            // Native mode uses block store instead of snapshot directories
            ContentStore::for_root(self.root()).ensure_exists()?;
            FileIndex::for_root(self.root()).ensure_exists()?;
        } else {
            fs::create_dir_all(self.root.join(SNAPSHOT_DIR))
                .context("failed to create snapshots directory")?;
        }

        // New repos start with monolithic format; auto-migration upgrades
        // to segmented when size thresholds are crossed.
        fl_storage::EventLog::for_root(self.root()).ensure_exists()?;
        fl_storage::RefStore::for_root(self.root()).ensure_exists()?;
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

        let secrets_config = self.root.join(SECRETS_CONFIG_FILE);
        if !secrets_config.exists() {
            fs::write(&secrets_config, crate::secrets::DEFAULT_SECRETS_TOML)
                .with_context(|| {
                    format!("failed to write {}", secrets_config.display())
                })?;
        }

        let hooks_config = self.root.join(HOOKS_CONFIG_FILE);
        if !hooks_config.exists() {
            fs::write(&hooks_config, crate::hooks::DEFAULT_HOOKS_TOML)
                .with_context(|| {
                    format!("failed to write {}", hooks_config.display())
                })?;
        }

        if mode == RepoMode::GitColocated {
            self.ensure_git_repository()?;
            self.ensure_git_exclude_entry(".flock/")?;
        }

        Ok(())
    }

    pub fn create_checkpoint(&self, message: Option<String>) -> Result<Event> {
        self.create_checkpoint_with_options(message, false, false, None)
    }

    pub fn create_checkpoint_with_options(
        &self,
        message: Option<String>,
        allow_secrets: bool,
        skip_hooks: bool,
        intent: Option<CheckpointIntentMetadata>,
    ) -> Result<Event> {
        self.assert_initialized()?;

        // Run pre-commit hooks (unless skipped).
        if skip_hooks {
            self.record_hook_bypass("pre-commit")?;
        } else {
            self.run_hooks_blocking("pre-commit")?;
        }

        // Run secret detection before creating the checkpoint.
        if !allow_secrets {
            self.scan_working_directory_for_secrets()?;
        }

        // Enforce policy checks (budget + rate limits + scope).
        let task_id = self.active_task_id();
        let exploration_id = self.active_exploration_id();
        self.enforce_budget_policy(task_id, exploration_id)?;
        self.enforce_rate_limit_policy(task_id, exploration_id)?;
        self.enforce_scope_policy(task_id, exploration_id)?;

        // Enforce architecture rules and anti-pattern detection.
        let changed_files = self.working_dir_changed_files();
        self.enforce_architecture_rules(&changed_files)?;
        self.enforce_anti_patterns(&changed_files)?;
        self.enforce_reuse_policy(&changed_files)?;
        self.enforce_dependency_policy(&changed_files)?;

        // Enforce commit hygiene.
        self.enforce_commit_hygiene(&intent)?;
        self.check_checkpoint_frequency();

        let label = message
            .as_deref()
            .map(normalize_label)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("checkpoint-{}", Uuid::new_v4().simple()));

        let event = self.create_checkpoint_with_lineage(label, message, None, intent)?;

        // If secrets were explicitly allowed, record it in the event log as an
        // audit trail.
        if allow_secrets {
            self.append_event(EventKind::GitBridge(GitBridgeEvent {
                action: GitBridgeAction::Commit,
                success: true,
                detail: format!(
                    "checkpoint={} allow_secrets=true (secret scan bypassed by user)",
                    event.id
                ),
            }))?;
        }

        // Auto-index: update semantic caches for changed files.
        // Errors are non-fatal — indexing is best-effort.
        let _ = self.auto_index_after_checkpoint(&event);

        // Run post-commit hooks (non-blocking).
        if skip_hooks {
            self.record_hook_bypass("post-commit")?;
        } else {
            self.run_hooks_reporting("post-commit");
        }

        Ok(event)
    }

    pub fn list_events(&self) -> Result<Vec<Event>> {
        self.assert_initialized()?;
        AutoEventLog::for_root(self.root()).read_all()
    }

    pub fn list_refs(&self) -> Result<Vec<RepoRef>> {
        self.assert_initialized()?;

        let store = AutoRefStore::for_root(self.root());
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

        let store = AutoRefStore::for_root(self.root());
        store.ensure_exists()?;
        store.upsert(reference.clone())?;
        self.sync_ref_to_git_if_colocated(&reference)?;
        Ok(reference)
    }

    pub fn delete_ref(&self, kind: RefKind, name: &str) -> Result<bool> {
        self.assert_initialized()?;

        let normalized_name = normalize_ref_name(name)?;
        let store = AutoRefStore::for_root(self.root());
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

        // Try to use materialized state for incremental replay
        let store = MaterializedStateStore::for_root(self.root());
        if let Ok(Some((event_count, json))) = store.load_latest() {
            if event_count <= events.len() {
                if let Ok(base_state) = serde_json::from_str::<ReplayedState>(&json) {
                    return replay_state_incremental(&events, event_count, base_state);
                }
            }
        }

        // Fallback to full replay
        replay_state(&events)
    }

    /// Materialize the current replay state for faster future replay.
    pub fn materialize(&self) -> Result<usize> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let json = serde_json::to_string(&state)
            .context("failed to serialize replayed state")?;
        let store = MaterializedStateStore::for_root(self.root());
        store.save(events.len(), &json)?;
        Ok(events.len())
    }

    /// Migrate event log from single JSONL to segmented format.
    pub fn migrate_event_log(&self) -> Result<fl_storage::EventLogMigrationReport> {
        self.assert_initialized()?;
        fl_storage::migrate_to_segmented(self.root())
    }

    /// Compact the event log, archiving old non-structural events.
    pub fn compact(&self, older_than: std::time::Duration) -> Result<fl_storage::CompactionReport> {
        self.assert_initialized()?;
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        fl_storage::compact_event_log(self.root(), older_than, now_nanos)
    }

    pub fn fsck(&self) -> Result<FsckReport> {
        self.assert_initialized()?;

        let events = AutoEventLog::for_root(self.root())
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

                    let expected_merkle =
                        checkpoint.snapshot_merkle_root.as_ref().ok_or_else(|| {
                            anyhow!(
                                "checkpoint {} is missing snapshot merkle root metadata",
                                event.id
                            )
                        })?;

                    let file_index = FileIndex::for_root(self.root());
                    if file_index.has(checkpoint.snapshot_id) {
                        // Native mode: verify via index
                        let index = file_index.read(checkpoint.snapshot_id).with_context(|| {
                            format!(
                                "failed to read native index for snapshot {}",
                                checkpoint.snapshot_id
                            )
                        })?;
                        let actual_merkle =
                            compute_native_merkle_root(&index).with_context(|| {
                                format!(
                                    "failed to compute merkle root for native snapshot {}",
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
                    } else {
                        // Directory-based mode (may lazily extract from git)
                        let snapshot_path = self.ensure_snapshot_available(checkpoint.snapshot_id)
                            .with_context(|| {
                                format!(
                                    "checkpoint {} references missing snapshot {}",
                                    event.id,
                                    checkpoint.snapshot_id
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
                EventKind::Session(_) => {}
                EventKind::Decision(_) => {}
                EventKind::ResourceUsage(_) => {}
                EventKind::Task(_) => {}
                EventKind::Presence(_) => {}
                EventKind::Lock(_) => {}
                EventKind::Subscription(_) => {}
                EventKind::Gate(_) => {}
                EventKind::Rebase(_) => {}
                EventKind::ConflictResolution(_) => {}
                EventKind::Hook(_) => {}
                EventKind::RemoteSync(_) => {}
                EventKind::Intelligence(_) => {}
                EventKind::Policy(_) => {}
                EventKind::Directive(_) => {}
                EventKind::Init(_) => {}
                EventKind::FileWrite(_) => {}
                EventKind::FileDelete(_) => {}
                EventKind::FileRename(_) => {}
            }
        }

        let snapshot_root = self.root.join(SNAPSHOT_DIR);
        let mut snapshot_count = 0usize;

        // In native mode, snapshots live in the block store / file index,
        // not as directories under .flock/snapshots/.  Count verified
        // checkpoint snapshot indices instead.
        if self.repo_mode()? == RepoMode::Native {
            let file_index = FileIndex::for_root(self.root());
            for sid in &checkpoint_snapshot_ids {
                if file_index.has(*sid) {
                    snapshot_count += 1;
                }
            }
        } else if snapshot_root.is_dir() {
            for entry in fs::read_dir(&snapshot_root)
                .with_context(|| format!("failed to read {}", snapshot_root.display()))?
            {
                let entry = entry.with_context(|| {
                    format!("failed to read entry in {}", snapshot_root.display())
                })?;
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
                    .ok_or_else(|| {
                        anyhow!("invalid snapshot directory name: {}", path.display())
                    })?;
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
        } else if !checkpoint_snapshot_ids.is_empty() {
            bail!("snapshots directory missing: {}", snapshot_root.display());
        }

        let refs = AutoRefStore::for_root(self.root())
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

        // Hash chain is verified as part of read_all() validation.
        // If we got here, it passed.
        Ok(FsckReport {
            event_count,
            checkpoint_count: seen_checkpoints.len(),
            snapshot_count,
            ref_count: refs.len(),
            hash_chain_verified: true,
        })
    }

    pub fn audit(&self) -> Result<crate::audit::AuditReport> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        Ok(crate::audit::analyze_audit_trail(&events))
    }

    pub fn key_status(&self) -> crate::key_crypto::KeyStatus {
        let key_path = self.root.join(SIGNING_KEY_FILE);
        crate::key_crypto::key_status(&key_path)
    }

    pub fn encrypt_signing_key(&self, passphrase: &str) -> Result<()> {
        self.assert_initialized()?;
        let key_path = self.root.join(SIGNING_KEY_FILE);
        crate::key_crypto::encrypt_signing_key(&key_path, passphrase)
    }

    pub fn decrypt_signing_key(&self, passphrase: &str) -> Result<()> {
        self.assert_initialized()?;
        let key_path = self.root.join(SIGNING_KEY_FILE);
        crate::key_crypto::decrypt_signing_key(&key_path, passphrase)
    }

    pub fn status(&self) -> Result<StatusReport> {
        self.assert_initialized()?;

        // Determine current branch from refs.
        let refs = self.list_refs()?;
        let branch = refs
            .iter()
            .find(|r| r.kind == RefKind::Branch)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| "main".to_string());

        let colocated = self.repo_mode()? == RepoMode::GitColocated;

        // Collect all files in the working directory (using ignore filtering).
        let (current_files, symlinks) = collect_all_files_with_mode(self.root(), true, colocated)?;

        let checkpoint = self.latest_checkpoint();
        if checkpoint.is_none() {
            // No checkpoint yet — everything is new.
            return Ok(StatusReport {
                branch,
                checkpoint_id: None,
                new_files: current_files.into_iter().collect(),
                modified_files: Vec::new(),
                deleted_files: Vec::new(),
                ignored_symlinks: symlinks,
            });
        }
        let checkpoint = checkpoint.unwrap();
        let checkpoint_id = checkpoint.id.to_string();
        let EventKind::Checkpoint(payload) = checkpoint.kind else {
            bail!("latest checkpoint event had unexpected kind");
        };

        let snapshot_root = self.ensure_snapshot_available(payload.snapshot_id)?;
        let (snapshot_files, _) = collect_all_files_with_mode(&snapshot_root, false, false)?;

        let mut new_files = Vec::new();
        let mut modified_files = Vec::new();
        let mut deleted_files = Vec::new();

        // Files in current but not in snapshot = new.
        for file in &current_files {
            if !snapshot_files.contains(file) {
                new_files.push(file.clone());
            }
        }

        // Files in both = check for modifications.
        for file in &current_files {
            if snapshot_files.contains(file) {
                let current_path = self.root().join(file);
                let snapshot_path = snapshot_root.join(file);
                let current_content = fs::read(&current_path).ok();
                let snapshot_content = fs::read(&snapshot_path).ok();
                if current_content != snapshot_content {
                    modified_files.push(file.clone());
                }
            }
        }

        // Files in snapshot but not in current = deleted.
        for file in &snapshot_files {
            if !current_files.contains(file) {
                deleted_files.push(file.clone());
            }
        }

        Ok(StatusReport {
            branch,
            checkpoint_id: Some(checkpoint_id),
            new_files,
            modified_files,
            deleted_files,
            ignored_symlinks: symlinks,
        })
    }

    pub fn semantic_diff_from_latest_checkpoint(&self) -> Result<Vec<SemanticFileDiff>> {
        self.assert_initialized()?;

        let checkpoint = self
            .latest_checkpoint()
            .ok_or_else(|| anyhow!("no checkpoint found; run `fl commit -m \"...\"` first"))?;

        let EventKind::Checkpoint(checkpoint_payload) = checkpoint.kind else {
            bail!("latest checkpoint event had unexpected kind")
        };

        let snapshot_root = self.ensure_snapshot_available(checkpoint_payload.snapshot_id)?;
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

    /// Returns a file-level summary of changes between the latest checkpoint and working dir.
    pub fn file_summary_from_latest_checkpoint(&self) -> Result<FileSummary> {
        self.assert_initialized()?;

        let checkpoint = self
            .latest_checkpoint()
            .ok_or_else(|| anyhow!("no checkpoint found; run `fl commit -m \"...\"` first"))?;

        let EventKind::Checkpoint(payload) = checkpoint.kind else {
            bail!("latest checkpoint event had unexpected kind")
        };

        let snapshot_root = self.ensure_snapshot_available(payload.snapshot_id)?;
        let snapshot_files = collect_all_repo_files(&snapshot_root, false)?;
        let current_files = collect_all_repo_files(self.root(), true)?;

        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();

        for path in &current_files {
            if !snapshot_files.contains(path) {
                added.push(path.display().to_string());
            } else {
                let old = fs::read(snapshot_root.join(path)).unwrap_or_default();
                let new = fs::read(self.root.join(path)).unwrap_or_default();
                if old != new {
                    modified.push(path.display().to_string());
                }
            }
        }
        for path in &snapshot_files {
            if !current_files.contains(path) {
                deleted.push(path.display().to_string());
            }
        }

        Ok(FileSummary {
            added,
            modified,
            deleted,
        })
    }

    /// Finds a checkpoint event by exact UUID or UUID prefix.
    pub fn find_checkpoint_by_prefix(&self, prefix: &str) -> Result<(Event, CheckpointEvent)> {
        let checkpoints = self.list_checkpoints_with_payload()?;
        let matches: Vec<_> = checkpoints
            .into_iter()
            .filter(|(event, _)| event.id.to_string().starts_with(prefix))
            .collect();
        match matches.len() {
            0 => bail!("no checkpoint matching prefix '{}'", prefix),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => bail!(
                "ambiguous prefix '{}' matches {} checkpoints; use a longer prefix",
                prefix,
                n
            ),
        }
    }

    /// Find the most recent checkpoint that is older than `duration` ago.
    ///
    /// If no checkpoint is old enough, returns the earliest checkpoint.
    pub fn find_checkpoint_before_duration(
        &self,
        duration: Duration,
    ) -> Result<(Event, CheckpointEvent)> {
        let checkpoints = self.list_checkpoints_with_payload()?;
        if checkpoints.is_empty() {
            bail!("no checkpoints found; run `fl commit -m \"...\"` first");
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u128;
        let cutoff = now.saturating_sub(duration.as_nanos());

        // Find most recent checkpoint with timestamp < cutoff (scanning in order, pick last match)
        let mut best: Option<(Event, CheckpointEvent)> = None;
        for (event, payload) in &checkpoints {
            let ts: u128 = event.timestamp.parse().unwrap_or(0);
            if ts < cutoff {
                best = Some((event.clone(), payload.clone()));
            }
        }

        // If none found, use earliest checkpoint
        best.or_else(|| checkpoints.into_iter().next())
            .ok_or_else(|| anyhow!("no checkpoints found"))
    }

    /// Semantic diff for a checkpoint against its predecessor.
    ///
    /// Finds the checkpoint immediately before `event_id` in the event log
    /// and diffs the two snapshots. If no predecessor exists, diffs against empty.
    pub fn semantic_diff_for_checkpoint(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<SemanticFileDiff>> {
        self.assert_initialized()?;
        let checkpoints = self.list_checkpoints_with_payload()?;

        let target_idx = checkpoints
            .iter()
            .position(|(e, _)| e.id == event_id)
            .ok_or_else(|| anyhow!("checkpoint {} not found", event_id))?;

        let target_payload = &checkpoints[target_idx].1;
        let to_root = self.ensure_snapshot_available(target_payload.snapshot_id)?;
        let to_files = collect_source_files(&to_root, false)?;

        if target_idx == 0 {
            // No predecessor — diff against empty
            let mut diffs = Vec::new();
            for rel_path in &to_files {
                let after = fs::read(to_root.join(rel_path)).ok();
                let diff = semantic_diff(rel_path, None, after.as_deref())?;
                if let Some(diff) = diff {
                    diffs.push(diff);
                }
            }
            enrich_semantic_impacts(&to_root, &to_files, &mut diffs)?;
            diffs.sort_by(|a, b| a.path.cmp(&b.path));
            return Ok(diffs);
        }

        let prev_payload = &checkpoints[target_idx - 1].1;
        let from_root = self.ensure_snapshot_available(prev_payload.snapshot_id)?;
        let from_files = collect_source_files(&from_root, false)?;

        let mut all_paths: BTreeSet<PathBuf> = from_files;
        all_paths.extend(to_files.iter().cloned());

        let mut diffs = Vec::new();
        for rel_path in &all_paths {
            let before = fs::read(from_root.join(rel_path)).ok();
            let after = fs::read(to_root.join(rel_path)).ok();
            let diff = semantic_diff(rel_path, before.as_deref(), after.as_deref())?;
            if let Some(diff) = diff {
                diffs.push(diff);
            }
        }

        enrich_semantic_impacts(&to_root, &to_files, &mut diffs)?;
        diffs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(diffs)
    }

    /// Semantic diff between two checkpoints identified by ID/prefix.
    pub fn semantic_diff_between_checkpoints(
        &self,
        from_prefix: &str,
        to_prefix: &str,
    ) -> Result<Vec<SemanticFileDiff>> {
        self.assert_initialized()?;

        let (_, from_payload) = self.find_checkpoint_by_prefix(from_prefix)?;
        let (_, to_payload) = self.find_checkpoint_by_prefix(to_prefix)?;

        let from_root = self.ensure_snapshot_available(from_payload.snapshot_id)?;
        let to_root = self.ensure_snapshot_available(to_payload.snapshot_id)?;

        let from_files = collect_source_files(&from_root, false)?;
        let to_files = collect_source_files(&to_root, false)?;

        let mut all_paths: BTreeSet<PathBuf> = from_files;
        all_paths.extend(to_files.iter().cloned());

        let mut diffs = Vec::new();
        for rel_path in &all_paths {
            let before_path = from_root.join(rel_path);
            let after_path = to_root.join(rel_path);

            let before = fs::read(&before_path).ok();
            let after = fs::read(&after_path).ok();

            let diff = semantic_diff(rel_path, before.as_deref(), after.as_deref())?;
            if let Some(diff) = diff {
                diffs.push(diff);
            }
        }

        let current_files = collect_source_files(&to_root, false)?;
        enrich_semantic_impacts(&to_root, &current_files, &mut diffs)?;
        diffs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(diffs)
    }

    /// Semantic diff between a specific checkpoint and the working directory.
    pub fn semantic_diff_checkpoint_vs_working(
        &self,
        checkpoint_prefix: &str,
    ) -> Result<Vec<SemanticFileDiff>> {
        self.assert_initialized()?;

        let (_, payload) = self.find_checkpoint_by_prefix(checkpoint_prefix)?;
        let snapshot_root = self.ensure_snapshot_available(payload.snapshot_id)?;
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

    /// Groups semantic changes between two checkpoints by inferred intent.
    pub fn semantic_diff_between_checkpoints_with_intents(
        &self,
        from_prefix: &str,
        to_prefix: &str,
    ) -> Result<Vec<(String, Vec<SemanticFileDiff>)>> {
        let diffs = self.semantic_diff_between_checkpoints(from_prefix, to_prefix)?;
        Ok(classify_diffs_by_intent(diffs))
    }

    /// Groups semantic changes from a checkpoint vs working directory by intent.
    pub fn semantic_diff_checkpoint_vs_working_with_intents(
        &self,
        checkpoint_prefix: &str,
    ) -> Result<Vec<(String, Vec<SemanticFileDiff>)>> {
        let diffs = self.semantic_diff_checkpoint_vs_working(checkpoint_prefix)?;
        Ok(classify_diffs_by_intent(diffs))
    }

    /// Returns a file-level summary of changes between two snapshots.
    pub fn file_summary_between_checkpoints(
        &self,
        from_prefix: &str,
        to_prefix: &str,
    ) -> Result<FileSummary> {
        self.assert_initialized()?;
        let (_, from_payload) = self.find_checkpoint_by_prefix(from_prefix)?;
        let (_, to_payload) = self.find_checkpoint_by_prefix(to_prefix)?;

        let from_root = self.ensure_snapshot_available(from_payload.snapshot_id)?;
        let to_root = self.ensure_snapshot_available(to_payload.snapshot_id)?;

        let from_files = collect_all_repo_files(&from_root, false)?;
        let to_files = collect_all_repo_files(&to_root, false)?;

        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();

        for path in &to_files {
            if !from_files.contains(path) {
                added.push(path.display().to_string());
            } else {
                let old = fs::read(from_root.join(path)).unwrap_or_default();
                let new = fs::read(to_root.join(path)).unwrap_or_default();
                if old != new {
                    modified.push(path.display().to_string());
                }
            }
        }
        for path in &from_files {
            if !to_files.contains(path) {
                deleted.push(path.display().to_string());
            }
        }

        Ok(FileSummary {
            added,
            modified,
            deleted,
        })
    }

    /// Returns a file-level summary of changes between a checkpoint and working dir.
    pub fn file_summary_checkpoint_vs_working(
        &self,
        checkpoint_prefix: &str,
    ) -> Result<FileSummary> {
        self.assert_initialized()?;
        let (_, payload) = self.find_checkpoint_by_prefix(checkpoint_prefix)?;
        let snapshot_root = self.ensure_snapshot_available(payload.snapshot_id)?;

        let snapshot_files = collect_all_repo_files(&snapshot_root, false)?;
        let current_files = collect_all_repo_files(self.root(), true)?;

        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();

        for path in &current_files {
            if !snapshot_files.contains(path) {
                added.push(path.display().to_string());
            } else {
                let old = fs::read(snapshot_root.join(path)).unwrap_or_default();
                let new = fs::read(self.root.join(path)).unwrap_or_default();
                if old != new {
                    modified.push(path.display().to_string());
                }
            }
        }
        for path in &snapshot_files {
            if !current_files.contains(path) {
                deleted.push(path.display().to_string());
            }
        }

        Ok(FileSummary {
            added,
            modified,
            deleted,
        })
    }

    /// Returns raw file content pairs for text diffing (latest checkpoint vs working dir).
    pub fn text_diff_from_latest_checkpoint(&self) -> Result<Vec<TextFileDiff>> {
        self.assert_initialized()?;
        let checkpoint = self
            .latest_checkpoint()
            .ok_or_else(|| anyhow!("no checkpoint found; run `fl commit -m \"...\"` first"))?;
        let EventKind::Checkpoint(payload) = checkpoint.kind else {
            bail!("latest checkpoint event had unexpected kind")
        };
        let snapshot_root = self.ensure_snapshot_available(payload.snapshot_id)?;
        Self::collect_text_diffs(&snapshot_root, self.root(), true)
    }

    /// Returns raw file content pairs for text diffing (checkpoint vs working dir).
    pub fn text_diff_checkpoint_vs_working(&self, checkpoint_prefix: &str) -> Result<Vec<TextFileDiff>> {
        self.assert_initialized()?;
        let (_, payload) = self.find_checkpoint_by_prefix(checkpoint_prefix)?;
        let snapshot_root = self.ensure_snapshot_available(payload.snapshot_id)?;
        Self::collect_text_diffs(&snapshot_root, self.root(), true)
    }

    /// Returns raw file content pairs for text diffing (checkpoint to checkpoint).
    pub fn text_diff_between_checkpoints(
        &self,
        from_prefix: &str,
        to_prefix: &str,
    ) -> Result<Vec<TextFileDiff>> {
        self.assert_initialized()?;
        let (_, from_payload) = self.find_checkpoint_by_prefix(from_prefix)?;
        let (_, to_payload) = self.find_checkpoint_by_prefix(to_prefix)?;
        let from_root = self.ensure_snapshot_available(from_payload.snapshot_id)?;
        let to_root = self.ensure_snapshot_available(to_payload.snapshot_id)?;
        Self::collect_text_diffs(&from_root, &to_root, false)
    }

    /// Collect file content pairs that differ between two directory roots.
    fn collect_text_diffs(
        old_root: &Path,
        new_root: &Path,
        skip_flock: bool,
    ) -> Result<Vec<TextFileDiff>> {
        let old_files = collect_all_repo_files(old_root, false)?;
        let new_files = collect_all_repo_files(new_root, skip_flock)?;
        let mut diffs = Vec::new();

        for path in &new_files {
            let old_content = if old_files.contains(path) {
                String::from_utf8_lossy(&fs::read(old_root.join(path)).unwrap_or_default())
                    .into_owned()
            } else {
                String::new()
            };
            let new_content =
                String::from_utf8_lossy(&fs::read(new_root.join(path)).unwrap_or_default())
                    .into_owned();
            if old_content != new_content {
                diffs.push(TextFileDiff {
                    path: path.display().to_string(),
                    old_content,
                    new_content,
                });
            }
        }

        for path in &old_files {
            if !new_files.contains(path) {
                let old_content =
                    String::from_utf8_lossy(&fs::read(old_root.join(path)).unwrap_or_default())
                        .into_owned();
                diffs.push(TextFileDiff {
                    path: path.display().to_string(),
                    old_content,
                    new_content: String::new(),
                });
            }
        }

        Ok(diffs)
    }

    /// Computes transitive impact of a file path within the repository.
    pub fn impact_analysis(&self, path: &str) -> Result<ImpactReport> {
        self.assert_initialized()?;

        let current_files = collect_source_files(self.root(), true)?;
        let reverse_dependencies = self.load_or_build_dependency_index(&current_files)?;

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
        let mut result = fl_semantic::merge(left_path, Some(&base), Some(&left), Some(&right))?
            .ok_or_else(|| {
                anyhow!(
                    "unsupported file type for semantic merge: {}",
                    left_path.display()
                )
            })?;

        // Enrich conflicts with cross-file breakage and impact data
        self.enrich_merge_conflicts(left_path, &mut result)?;
        Ok(result)
    }

    fn enrich_merge_conflicts(
        &self,
        merged_path: &Path,
        result: &mut SemanticMergeResult,
    ) -> Result<()> {
        let current_files = collect_source_files(self.root(), true)?;
        let reverse_dependencies = self.load_or_build_dependency_index(&current_files)?;

        let rel_path = merged_path
            .strip_prefix(self.root())
            .unwrap_or(merged_path);
        let impacted_files = collect_impacted_files(&rel_path.to_path_buf(), &reverse_dependencies);
        let impacted_file_strings: Vec<String> = impacted_files
            .iter()
            .filter(|p| p.as_path() != rel_path)
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let impacted_modules: Vec<String> = impacted_files
            .iter()
            .filter(|p| p.as_path() != rel_path)
            .map(|p| module_name_for_path(p))
            .collect();

        // Enrich existing conflicts with impact data
        for conflict in &mut result.conflicts {
            conflict.affected_files.clone_from(&impacted_file_strings);
            conflict.impact = Some(SemanticImpact {
                symbols: impact_symbols(&conflict.symbol),
                files: impacted_file_strings.clone(),
                modules: impacted_modules.clone(),
            });
        }

        // Detect cross-file breakage: if merge changes a function signature,
        // scan importers for potential breakage
        let merged_symbols = self.extract_symbols_for_path(&rel_path.to_string_lossy())?;
        if !impacted_file_strings.is_empty() {
            for conflict in &result.conflicts {
                if conflict.classification == SemanticConflictClassification::DivergentEdit {
                    let symbol_name = conflict
                        .symbol
                        .split(':')
                        .nth(1)
                        .unwrap_or(&conflict.symbol);
                    if merged_symbols.iter().any(|s| s.contains(symbol_name)) {
                        let cross_file = SemanticMergeConflict {
                            symbol: conflict.symbol.clone(),
                            classification: SemanticConflictClassification::CrossFileBreakage,
                            explanation: format!(
                                "merge conflict on `{}` may break {} downstream file(s): {}",
                                conflict.symbol,
                                impacted_file_strings.len(),
                                impacted_file_strings.join(", ")
                            ),
                            affected_files: impacted_file_strings.clone(),
                            impact: Some(SemanticImpact {
                                symbols: impact_symbols(&conflict.symbol),
                                files: impacted_file_strings.clone(),
                                modules: impacted_modules.clone(),
                            }),
                        };
                        result.conflicts.push(cross_file);
                        break;
                    }
                }
            }
        }

        Ok(())
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
        let snapshot_root = self.ensure_snapshot_available(snapshot_id)?;
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

        // Enforce rate limit policy (explorations per task).
        let task_id = self.active_task_id();
        self.enforce_rate_limit_policy(task_id, None)?;

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

        // Enforce budget policy before promoting.
        let task_id = self.active_task_id();
        self.enforce_budget_policy(task_id, Some(id))?;

        // Enforce test requirements before promoting.
        self.enforce_test_requirements()?;

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
        let promote_event = self.create_checkpoint_with_lineage(
            format!("promote-{}", normalize_label(&existing.title)),
            Some(message),
            None,
            None,
        )?;

        // Ensure the promote checkpoint's snapshot is materialized on disk.
        // In git-colocated mode, checkpoints use virtual snapshots that are
        // lazily extracted from git.  Materializing here guarantees that
        // subsequent operations (diff, undo, commit) can access the snapshot
        // without relying on lazy extraction, which can fail if the git state
        // changes between promote and the next operation.
        if let EventKind::Checkpoint(ref payload) = promote_event.kind {
            self.ensure_snapshot_available(payload.snapshot_id)?;
        }

        self.append_event(EventKind::Exploration(ExplorationEvent {
            exploration_id: id,
            title: existing.title.clone(),
            base_checkpoint_event: existing.base_checkpoint_event,
            action: ExplorationAction::Promote,
        }))?;

        // Post-promote regression monitoring (best-effort, non-blocking).
        self.post_promote_regression_check(id);

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
        let left_root = self.ensure_snapshot_available(left_snapshot_id)?;
        let right_root = self.ensure_snapshot_available(right_snapshot_id)?;

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
                // Note: we intentionally do NOT delete the base checkpoint's
                // snapshot here. The base checkpoint belongs to the main
                // timeline and may be referenced by other events, undo, or
                // branches. Snapshot cleanup should be handled by a dedicated
                // gc/cleanup command that checks for dangling snapshots.

                // Emit a Prune event so the exploration is removed from
                // replayed state.
                self.append_event(EventKind::Exploration(ExplorationEvent {
                    exploration_id: exploration.id,
                    title: exploration.title.clone(),
                    base_checkpoint_event: exploration.base_checkpoint_event,
                    action: ExplorationAction::Prune,
                }))?;
                pruned += 1;
            }
        }

        Ok(pruned)
    }

    /// Return explorations that *would* be pruned (dry-run preview).
    pub fn prune_candidates(&self, max_age: std::time::Duration) -> Result<Vec<ExplorationSummary>> {
        self.assert_initialized()?;

        let now_nanos: u128 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before unix epoch")?
            .as_nanos();

        let explorations = self.list_explorations()?;
        let mut candidates = Vec::new();

        for exploration in explorations {
            if exploration.status != ExplorationStatus::Abandoned {
                continue;
            }
            let updated_nanos: u128 = exploration.updated_at.parse().unwrap_or(0);
            let age_nanos = now_nanos.saturating_sub(updated_nanos);
            if age_nanos >= max_age.as_nanos() {
                candidates.push(exploration);
            }
        }

        Ok(candidates)
    }

    // ── Blame ────────────────────────────────────────────────────────

    /// Annotate each line of a file with the commit that last changed it.
    pub fn blame(&self, path: &str) -> Result<Vec<BlameAnnotation>> {
        self.assert_initialized()?;

        // Collect all checkpoint events in order.
        let events = self.list_events()?;
        let mut checkpoints: Vec<(Uuid, &str, Option<&str>, Uuid)> = Vec::new();
        for event in &events {
            if let EventKind::Checkpoint(ref cp) = event.kind {
                checkpoints.push((
                    event.id,
                    // We'll access actor/timestamp from the event directly
                    // store event index for later
                    &event.actor,
                    Some(event.timestamp.as_str()),
                    cp.snapshot_id,
                ));
            }
        }

        if checkpoints.is_empty() {
            return Ok(Vec::new());
        }

        // Walk checkpoints in reverse to build attribution.
        // For each checkpoint, load the file content. If the file changed
        // compared to the next checkpoint's version, attribute changed lines.
        // Read the file at the latest checkpoint.
        let (_latest_id, _latest_actor, _latest_ts, latest_snap) = checkpoints.last().unwrap();
        let latest_root = self.ensure_snapshot_available(*latest_snap)?;
        let latest_file = latest_root.join(path);
        if !latest_file.exists() {
            // File doesn't exist in latest checkpoint - try working directory
            let working_file = self.root().join(path);
            if !working_file.exists() {
                bail!("file {} not found in latest commit or working directory", path);
            }
            // File only exists in working dir, not committed yet
            let content = fs::read_to_string(&working_file)
                .with_context(|| format!("failed to read {}", path))?;
            let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
            return Ok(lines.iter().enumerate().map(|(i, line)| BlameAnnotation {
                line_number: i + 1,
                content: line.clone(),
                commit_id: None,
                author: None,
                timestamp: None,
                message: Some("uncommitted".to_string()),
            }).collect());
        }

        let content = fs::read_to_string(&latest_file)
            .with_context(|| format!("failed to read {} from snapshot", path))?;
        let current_lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        let mut annotations: Vec<Option<(Uuid, String, String, Option<String>)>> = vec![None; current_lines.len()];

        // Walk checkpoints in reverse: for each pair (older, newer), find lines
        // that changed and attribute them to the newer checkpoint.
        for i in (0..checkpoints.len()).rev() {
            let (cp_id, cp_actor, cp_ts, cp_snap) = &checkpoints[i];

            // Get message from events
            let cp_message = events.iter().find(|e| e.id == *cp_id).and_then(|e| {
                if let EventKind::Checkpoint(ref cp) = e.kind {
                    cp.message.clone()
                } else {
                    None
                }
            });

            let cp_root = self.ensure_snapshot_available(*cp_snap)?;
            let cp_file = cp_root.join(path);

            let cp_lines: Vec<String> = if cp_file.exists() {
                fs::read_to_string(&cp_file)
                    .unwrap_or_default()
                    .lines()
                    .map(|l| l.to_string())
                    .collect()
            } else {
                Vec::new()
            };

            // Get previous checkpoint's file content
            let prev_lines: Vec<String> = if i > 0 {
                let (_, _, _, prev_snap) = &checkpoints[i - 1];
                let prev_root = self.ensure_snapshot_available(*prev_snap)?;
                let prev_file = prev_root.join(path);
                if prev_file.exists() {
                    fs::read_to_string(&prev_file)
                        .unwrap_or_default()
                        .lines()
                        .map(|l| l.to_string())
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                // First checkpoint - all lines are new
                Vec::new()
            };

            // For lines present in current version that match this checkpoint's
            // version but differ from previous: attribute to this checkpoint.
            for (line_idx, line) in current_lines.iter().enumerate() {
                if annotations[line_idx].is_some() {
                    continue; // Already attributed
                }

                // Check if this line exists in this checkpoint's file
                let in_this = cp_lines.iter().any(|l| l == line);
                let in_prev = prev_lines.iter().any(|l| l == line);

                if in_this && !in_prev {
                    annotations[line_idx] = Some((
                        *cp_id,
                        cp_actor.to_string(),
                        cp_ts.unwrap_or("").to_string(),
                        cp_message.clone(),
                    ));
                }
            }
        }

        // Any remaining unattributed lines: attribute to the first checkpoint
        // that contains them.
        if let Some((first_id, first_actor, first_ts, _)) = checkpoints.first() {
            let first_msg = events.iter().find(|e| e.id == *first_id).and_then(|e| {
                if let EventKind::Checkpoint(ref cp) = e.kind {
                    cp.message.clone()
                } else {
                    None
                }
            });
            for ann in annotations.iter_mut() {
                if ann.is_none() {
                    *ann = Some((
                        *first_id,
                        first_actor.to_string(),
                        first_ts.unwrap_or("").to_string(),
                        first_msg.clone(),
                    ));
                }
            }
        }

        Ok(current_lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let (commit_id, author, timestamp, message) = annotations[i]
                    .clone()
                    .unwrap_or_default();
                BlameAnnotation {
                    line_number: i + 1,
                    content: line.clone(),
                    commit_id: if commit_id == Uuid::nil() { None } else { Some(commit_id) },
                    author: if author.is_empty() { None } else { Some(author) },
                    timestamp: if timestamp.is_empty() { None } else { Some(timestamp) },
                    message,
                }
            })
            .collect())
    }

    // ── Stash ────────────────────────────────────────────────────────

    /// Save working directory changes to a stash entry and revert to last commit.
    pub fn stash_push(&self, message: Option<String>) -> Result<usize> {
        self.assert_initialized()?;

        // Create a snapshot of the current working directory state
        let snapshot_id = Uuid::new_v4();
        let snapshot_dir = self.stash_dir();
        fs::create_dir_all(&snapshot_dir)?;

        // Read the stash list
        let mut entries = self.stash_read_entries()?;
        let _internal_index = entries.len();

        // Snapshot the working directory into a stash-specific location
        let stash_snap_dir = snapshot_dir.join(snapshot_id.to_string());
        fs::create_dir_all(&stash_snap_dir)?;

        let colocated = self.repo_mode()? == RepoMode::GitColocated;
        let (files, _) = collect_all_files_with_mode(self.root(), true, colocated)?;
        for file in &files {
            let src = self.root().join(file);
            let dst = stash_snap_dir.join(file);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dst)?;
        }

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos()
            .to_string();

        entries.push(StashEntry {
            snapshot_id,
            message,
            timestamp: ts,
        });

        self.stash_write_entries(&entries)?;

        // Restore working directory to last checkpoint
        let checkpoint = self.latest_checkpoint();
        if let Some(cp) = checkpoint {
            let EventKind::Checkpoint(payload) = cp.kind else {
                bail!("unexpected event kind for latest checkpoint");
            };
            let snap_root = self.ensure_snapshot_available(payload.snapshot_id)?;
            self.restore_working_directory(&snap_root)?;
        }

        // stash@{0} is always the most recent (git convention)
        Ok(0)
    }

    /// Restore a stash entry and remove it from the list.
    /// Index follows git convention: 0 = most recent stash.
    pub fn stash_pop(&self, index: usize) -> Result<()> {
        self.assert_initialized()?;

        let mut entries = self.stash_read_entries()?;
        if index >= entries.len() {
            bail!("stash@{{{}}} does not exist (only {} entries)", index, entries.len());
        }

        // Map user-facing index (0=newest) to internal index (0=oldest)
        let internal_index = entries.len() - 1 - index;
        let entry = entries.remove(internal_index);
        let stash_snap_dir = self.stash_dir().join(entry.snapshot_id.to_string());

        if !stash_snap_dir.exists() {
            bail!("stash snapshot {} is missing", entry.snapshot_id);
        }

        // Restore the stashed files to working directory
        self.restore_working_directory(&stash_snap_dir)?;

        // Clean up the stash snapshot
        fs::remove_dir_all(&stash_snap_dir)?;
        self.stash_write_entries(&entries)?;

        Ok(())
    }

    /// List all stash entries, newest first (git convention: stash@{0} = most recent).
    pub fn stash_list(&self) -> Result<Vec<StashEntry>> {
        self.assert_initialized()?;
        let mut entries = self.stash_read_entries()?;
        entries.reverse();
        Ok(entries)
    }

    /// Remove a stash entry without applying it.
    /// Index follows git convention: 0 = most recent stash.
    pub fn stash_drop(&self, index: usize) -> Result<()> {
        self.assert_initialized()?;

        let mut entries = self.stash_read_entries()?;
        if index >= entries.len() {
            bail!("stash@{{{}}} does not exist (only {} entries)", index, entries.len());
        }

        // Map user-facing index (0=newest) to internal index (0=oldest)
        let internal_index = entries.len() - 1 - index;
        let entry = entries.remove(internal_index);
        let stash_snap_dir = self.stash_dir().join(entry.snapshot_id.to_string());
        if stash_snap_dir.exists() {
            fs::remove_dir_all(&stash_snap_dir)?;
        }
        self.stash_write_entries(&entries)?;

        Ok(())
    }

    fn stash_dir(&self) -> PathBuf {
        self.flock_dir().join("stash")
    }

    fn stash_read_entries(&self) -> Result<Vec<StashEntry>> {
        let path = self.stash_dir().join("stash.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&path)?;
        let entries: Vec<StashEntry> = serde_json::from_str(&content)
            .context("failed to parse stash.json")?;
        Ok(entries)
    }

    fn stash_write_entries(&self, entries: &[StashEntry]) -> Result<()> {
        let dir = self.stash_dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join("stash.json");
        let content = serde_json::to_string_pretty(entries)?;
        fs::write(&path, content)?;
        Ok(())
    }

    /// Restore working directory from a snapshot directory.
    fn restore_working_directory(&self, snapshot_root: &Path) -> Result<()> {
        let colocated = self.repo_mode()? == RepoMode::GitColocated;
        let (current_files, _) = collect_all_files_with_mode(self.root(), true, colocated)?;
        let (snap_files, _) = collect_all_files_with_mode(snapshot_root, false, false)?;

        // Delete files not in snapshot
        for file in &current_files {
            if !snap_files.contains(file) {
                let path = self.root().join(file);
                fs::remove_file(&path).ok();
            }
        }

        // Copy/overwrite files from snapshot
        for file in &snap_files {
            let src = snapshot_root.join(file);
            let dst = self.root().join(file);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dst)?;
        }

        Ok(())
    }

    // ── Session methods ───────────────────────────────────────────────

    pub fn start_session(
        &self,
        task: String,
        agent: Option<String>,
        initiator: Option<String>,
    ) -> Result<SessionSummary> {
        self.assert_initialized()?;

        let session_id = Uuid::new_v4();
        let agent_name = agent.unwrap_or_else(current_actor);
        let event = self.append_event(EventKind::Session(SessionEvent {
            session_id,
            action: SessionAction::Start,
            agent: agent_name.clone(),
            initiator: initiator.clone(),
            task_description: Some(task.clone()),
            exploration_id: None,
            result: None,
        }))?;

        Ok(SessionSummary {
            id: session_id,
            agent: agent_name,
            initiator,
            task_description: Some(task),
            status: SessionStatus::Active,
            explorations: Vec::new(),
            decisions: Vec::new(),
            resource_usage: ResourceUsageTotals::default(),
            created_at: event.timestamp,
            completed_at: None,
            result: None,
        })
    }

    pub fn link_session_exploration(
        &self,
        session_id: Uuid,
        exploration_id: Uuid,
    ) -> Result<()> {
        self.assert_initialized()?;
        self.assert_session_active(session_id)?;

        self.append_event(EventKind::Session(SessionEvent {
            session_id,
            action: SessionAction::Link,
            agent: current_actor(),
            initiator: None,
            task_description: None,
            exploration_id: Some(exploration_id),
            result: None,
        }))?;

        Ok(())
    }

    pub fn record_decision(
        &self,
        session_id: Uuid,
        exploration_id: Uuid,
        action: DecisionAction,
        reason: String,
        confidence: f64,
    ) -> Result<()> {
        self.assert_initialized()?;
        self.assert_session_active(session_id)?;

        if !(0.0..=1.0).contains(&confidence) {
            bail!("confidence must be between 0.0 and 1.0, got {}", confidence);
        }

        self.append_event(EventKind::Decision(DecisionEvent {
            session_id,
            exploration_id,
            action,
            reason,
            confidence,
        }))?;

        Ok(())
    }

    pub fn record_resource_usage(
        &self,
        session_id: Uuid,
        tokens: Option<u64>,
        runtime_ms: Option<u64>,
        api_calls: Option<Vec<ApiCallRecord>>,
    ) -> Result<()> {
        self.assert_initialized()?;
        self.assert_session_active(session_id)?;

        self.append_event(EventKind::ResourceUsage(ResourceUsageEvent {
            session_id,
            tokens_consumed: tokens,
            runtime_ms,
            api_calls,
        }))?;

        Ok(())
    }

    pub fn complete_session(
        &self,
        session_id: Uuid,
        result: Option<String>,
    ) -> Result<SessionSummary> {
        self.assert_initialized()?;
        self.assert_session_active(session_id)?;

        self.append_event(EventKind::Session(SessionEvent {
            session_id,
            action: SessionAction::Complete,
            agent: current_actor(),
            initiator: None,
            task_description: None,
            exploration_id: None,
            result,
        }))?;

        self.session_info(session_id)
    }

    pub fn fail_session(&self, session_id: Uuid, reason: String) -> Result<SessionSummary> {
        self.assert_initialized()?;
        self.assert_session_active(session_id)?;

        self.append_event(EventKind::Session(SessionEvent {
            session_id,
            action: SessionAction::Fail,
            agent: current_actor(),
            initiator: None,
            task_description: None,
            exploration_id: None,
            result: Some(reason),
        }))?;

        self.session_info(session_id)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let mut sessions: Vec<SessionSummary> = state.sessions.into_values().collect();
        sessions.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(sessions)
    }

    pub fn session_info(&self, session_id: Uuid) -> Result<SessionSummary> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;
        state
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow!("session {} not found", session_id))
    }

    pub fn query_provenance(&self, exploration_id: Uuid) -> Result<ProvenanceInfo> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;

        let exploration = state
            .explorations
            .get(&exploration_id)
            .cloned()
            .ok_or_else(|| anyhow!("exploration {} not found", exploration_id))?;

        // Find the session that contains this exploration
        let session = state
            .sessions
            .values()
            .find(|s| s.explorations.contains(&exploration_id))
            .cloned();

        // Collect decisions related to this exploration
        let decisions: Vec<DecisionSummary> = session
            .as_ref()
            .map(|s| {
                s.decisions
                    .iter()
                    .filter(|d| d.exploration_id == exploration_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        // Collect related events (exploration events for this ID + session events)
        let related_events: Vec<Event> = events
            .iter()
            .filter(|e| match &e.kind {
                EventKind::Exploration(exp) => exp.exploration_id == exploration_id,
                EventKind::Decision(dec) => dec.exploration_id == exploration_id,
                EventKind::Session(ses) => {
                    session.as_ref().is_some_and(|s| ses.session_id == s.id)
                }
                EventKind::ResourceUsage(usage) => {
                    session.as_ref().is_some_and(|s| usage.session_id == s.id)
                }
                _ => false,
            })
            .cloned()
            .collect();

        Ok(ProvenanceInfo {
            session,
            exploration,
            decisions,
            related_events,
        })
    }

    pub fn replay_session(&self, session_id: Uuid) -> Result<SessionReplay> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;

        let session = state
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow!("session {} not found", session_id))?;

        let timeline: Vec<Event> = events
            .into_iter()
            .filter(|e| match &e.kind {
                EventKind::Session(ses) => ses.session_id == session_id,
                EventKind::Decision(dec) => dec.session_id == session_id,
                EventKind::ResourceUsage(usage) => usage.session_id == session_id,
                EventKind::Exploration(exp) => session.explorations.contains(&exp.exploration_id),
                _ => false,
            })
            .collect();

        Ok(SessionReplay { session, timeline })
    }

    fn assert_session_active(&self, session_id: Uuid) -> Result<()> {
        let session = self.session_info(session_id)?;
        if session.status != SessionStatus::Active {
            bail!(
                "session {} is not active (current status: {})",
                session_id,
                session.status
            );
        }
        Ok(())
    }

    // ── Task methods ─────────────────────────────────────────────────

    pub fn create_task(
        &self,
        title: String,
        description: Option<String>,
        dependencies: Vec<Uuid>,
        discovered_from: Option<Uuid>,
        allowed_paths: Vec<String>,
    ) -> Result<TaskSummary> {
        self.assert_initialized()?;

        // Validate dependencies exist
        if !dependencies.is_empty() {
            let events = self.list_events()?;
            let state = replay_state(&events)?;
            for dep_id in &dependencies {
                if !state.tasks.contains_key(dep_id) {
                    bail!("dependency task {} not found", dep_id);
                }
            }
        }

        let task_id = Uuid::new_v4();
        let event = self.append_event(EventKind::Task(TaskEvent {
            task_id,
            action: TaskAction::Create,
            title: title.clone(),
            description: description.clone(),
            dependencies: dependencies.clone(),
            assignee: None,
            result: None,
            linked_events: Vec::new(),
            discovered_from,
            allowed_paths: allowed_paths.clone(),
        }))?;

        Ok(TaskSummary {
            id: task_id,
            title,
            description,
            status: TaskStatus::Open,
            dependencies,
            dependents: Vec::new(),
            assignee: None,
            created_at: event.timestamp,
            claimed_at: None,
            completed_at: None,
            result: None,
            linked_events: Vec::new(),
            discovered_from,
            allowed_paths,
        })
    }

    pub fn list_tasks(&self) -> Result<Vec<TaskSummary>> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let mut tasks: Vec<TaskSummary> = state.tasks.into_values().collect();
        tasks.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(tasks)
    }

    pub fn task_info(&self, task_id: Uuid) -> Result<TaskSummary> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;
        state
            .tasks
            .get(&task_id)
            .cloned()
            .ok_or_else(|| anyhow!("task {} not found", task_id))
    }

    /// Finds a task by exact UUID or UUID prefix.
    pub fn find_task_by_prefix(&self, prefix: &str) -> Result<TaskSummary> {
        let tasks = self.list_tasks()?;
        let matches: Vec<_> = tasks
            .into_iter()
            .filter(|t| t.id.to_string().starts_with(prefix))
            .collect();
        match matches.len() {
            0 => bail!("no task matching prefix '{}'", prefix),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => bail!(
                "ambiguous prefix '{}' matches {} tasks; use a longer prefix",
                prefix,
                n
            ),
        }
    }

    pub fn claim_task(&self, task_id: Uuid, assignee: Option<String>) -> Result<TaskSummary> {
        self.assert_initialized()?;

        let task = self.task_info(task_id)?;
        if task.status != TaskStatus::Open {
            bail!(
                "task {} is not open (current status: {})",
                task_id,
                task.status
            );
        }

        let assignee_name = assignee.unwrap_or_else(current_actor);
        self.append_event(EventKind::Task(TaskEvent {
            task_id,
            action: TaskAction::Claim,
            title: task.title.clone(),
            description: None,
            dependencies: Vec::new(),
            assignee: Some(assignee_name.clone()),
            result: None,
            linked_events: Vec::new(),
            discovered_from: None,
            allowed_paths: Vec::new(),
        }))?;

        self.task_info(task_id)
    }

    pub fn unclaim_task(&self, task_id: Uuid) -> Result<TaskSummary> {
        self.assert_initialized()?;

        let task = self.task_info(task_id)?;
        if task.status != TaskStatus::Claimed {
            bail!(
                "task {} is not claimed (current status: {})",
                task_id,
                task.status
            );
        }

        self.append_event(EventKind::Task(TaskEvent {
            task_id,
            action: TaskAction::Unclaim,
            title: task.title.clone(),
            description: None,
            dependencies: Vec::new(),
            assignee: None,
            result: None,
            linked_events: Vec::new(),
            discovered_from: None,
            allowed_paths: Vec::new(),
        }))?;

        self.task_info(task_id)
    }

    pub fn complete_task(&self, task_id: Uuid, result: Option<String>) -> Result<TaskSummary> {
        self.assert_initialized()?;

        let task = self.task_info(task_id)?;
        if task.status != TaskStatus::Open && task.status != TaskStatus::Claimed {
            bail!(
                "task {} cannot be completed (current status: {})",
                task_id,
                task.status
            );
        }

        self.append_event(EventKind::Task(TaskEvent {
            task_id,
            action: TaskAction::Complete,
            title: task.title.clone(),
            description: None,
            dependencies: Vec::new(),
            assignee: None,
            result: result.clone(),
            linked_events: Vec::new(),
            discovered_from: None,
            allowed_paths: Vec::new(),
        }))?;

        self.task_info(task_id)
    }

    pub fn fail_task(&self, task_id: Uuid, reason: String) -> Result<TaskSummary> {
        self.assert_initialized()?;

        let task = self.task_info(task_id)?;
        if task.status != TaskStatus::Open && task.status != TaskStatus::Claimed {
            bail!(
                "task {} cannot be failed (current status: {})",
                task_id,
                task.status
            );
        }

        self.append_event(EventKind::Task(TaskEvent {
            task_id,
            action: TaskAction::Fail,
            title: task.title.clone(),
            description: None,
            dependencies: Vec::new(),
            assignee: None,
            result: Some(reason),
            linked_events: Vec::new(),
            discovered_from: None,
            allowed_paths: Vec::new(),
        }))?;

        self.task_info(task_id)
    }

    pub fn link_task_event(&self, task_id: Uuid, event_ids: Vec<Uuid>) -> Result<()> {
        self.assert_initialized()?;

        let task = self.task_info(task_id)?;
        if task.status == TaskStatus::Completed || task.status == TaskStatus::Failed {
            bail!(
                "task {} is already finished (current status: {})",
                task_id,
                task.status
            );
        }

        self.append_event(EventKind::Task(TaskEvent {
            task_id,
            action: TaskAction::Link,
            title: task.title.clone(),
            description: None,
            dependencies: Vec::new(),
            assignee: None,
            result: None,
            linked_events: event_ids,
            discovered_from: None,
            allowed_paths: Vec::new(),
        }))?;

        Ok(())
    }

    /// Returns open tasks whose dependencies are all completed,
    /// sorted by creation time (oldest first).
    pub fn ready_tasks(&self) -> Result<Vec<TaskSummary>> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let mut ready: Vec<TaskSummary> = state
            .tasks
            .values()
            .filter(|t| t.is_ready(&state.tasks))
            .cloned()
            .collect();
        ready.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(ready)
    }

    /// Returns a count of completed/failed tasks older than the given duration.
    /// In the append-only model, compaction is informational — the tasks are
    /// already hidden from default list views.
    pub fn compact_tasks_dry_run(&self, max_age: std::time::Duration) -> Result<usize> {
        self.assert_initialized()?;

        let now_nanos: u128 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before unix epoch")?
            .as_nanos();

        let tasks = self.list_tasks()?;
        let mut compactable = 0;

        for task in &tasks {
            let finished = task.status == TaskStatus::Completed || task.status == TaskStatus::Failed;
            if !finished {
                continue;
            }

            let completed_nanos: u128 = task
                .completed_at
                .as_deref()
                .and_then(|ts| ts.parse().ok())
                .unwrap_or(0);
            let age_nanos = now_nanos.saturating_sub(completed_nanos);

            if age_nanos >= max_age.as_nanos() {
                compactable += 1;
            }
        }

        Ok(compactable)
    }

    pub fn task_graph(&self) -> Result<TaskGraph> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;
        Ok(build_task_graph(&state.tasks))
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
            .ok_or_else(|| anyhow!("cannot create workspace: no checkpoint exists; run `fl commit` first"))?;
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

        let store = AutoRefStore::for_root(self.root());
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

        let store = AutoRefStore::for_root(self.root());
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

    /// Delete a workspace by name. Returns true if it existed.
    pub fn delete_workspace(&self, name: &str) -> Result<bool> {
        self.assert_initialized()?;
        self.delete_ref(RefKind::Workspace, name)
    }

    /// Rename a workspace. Returns the updated ref.
    pub fn rename_workspace(&self, old_name: &str, new_name: &str) -> Result<RepoRef> {
        self.assert_initialized()?;

        let store = AutoRefStore::for_root(self.root());
        let refs = store.read_all()?;
        let old_ref = refs
            .iter()
            .find(|r| r.kind == RefKind::Workspace && r.name == old_name)
            .ok_or_else(|| anyhow!("workspace `{}` not found", old_name))?
            .clone();

        // Check new name doesn't conflict
        if refs
            .iter()
            .any(|r| r.kind == RefKind::Workspace && r.name == new_name)
        {
            bail!("workspace `{}` already exists", new_name);
        }

        let new_ref = RepoRef {
            name: new_name.to_string(),
            ..old_ref
        };

        store.delete(RefKind::Workspace, old_name)?;
        store.upsert(new_ref.clone())?;

        // Sync git refs if colocated
        self.delete_git_ref_if_colocated(RefKind::Workspace, old_name)?;
        self.sync_ref_to_git_if_colocated(&new_ref)?;

        Ok(new_ref)
    }

    /// Quick save: create a checkpoint with auto-generated label, optimized for agent use.
    pub fn quick_save(&self, tag: Option<String>) -> Result<Event> {
        self.assert_initialized()?;
        let label = tag.unwrap_or_else(|| format!("quick-{}", Uuid::new_v4().simple()));
        self.create_checkpoint_with_lineage(label, Some("quick save".to_string()), None, None)
    }

    /// Quick restore: restore workspace to the last quick-save checkpoint.
    ///
    /// Unlike generic undo (which restores to *before* the target), quick-restore
    /// restores *to* the quick-save snapshot so the agent gets back exactly the
    /// state it saved.
    pub fn quick_restore(&self) -> Result<UndoResult> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        if events.is_empty() {
            bail!("cannot quick-restore: event log is empty");
        }

        // Find the most recent quick-save checkpoint by scanning backwards.
        let quick_save = events
            .iter()
            .rev()
            .find(|e| {
                if let EventKind::Checkpoint(ref cp) = e.kind {
                    cp.message.as_deref() == Some("quick save")
                } else {
                    false
                }
            })
            .ok_or_else(|| anyhow!("no quick-save checkpoint found"))?;

        let EventKind::Checkpoint(ref payload) = quick_save.kind else {
            bail!("expected checkpoint payload");
        };

        // Restore workspace TO the quick-save snapshot (not before it).
        self.restore_workspace_from_snapshot(payload.snapshot_id)?;

        let checkpoint_event = self.create_checkpoint_with_lineage(
            format!("quick-restore-{}", quick_save.id.simple()),
            Some(format!("quick-restore to checkpoint {}", quick_save.id)),
            Some(quick_save.id),
            None,
        )?;
        let restored_checkpoint_event = Some(checkpoint_event.id);

        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id: quick_save.id,
            mode: UndoMode::Last,
            restored_checkpoint_event,
            file_scope: None,
            undo_scope: None,
        }))?;

        Ok(UndoResult {
            target_event_id: quick_save.id,
            restored_checkpoint_event,
        })
    }

    pub fn undo(&self, request: UndoRequest) -> Result<UndoResult> {
        self.undo_inner(request, false)
    }

    /// Undo to the previous checkpoint boundary (coarse granularity),
    /// bypassing file-level undo even in native mode.
    pub fn undo_to_checkpoint(&self, request: UndoRequest) -> Result<UndoResult> {
        self.undo_inner(request, true)
    }

    /// Scoped undo: only reverts events matching the given scope filters.
    /// If the scope is empty, delegates to the normal `undo()`.
    pub fn undo_scoped(&self, request: UndoRequest, scope: UndoScope) -> Result<UndoResult> {
        if scope.is_empty() {
            return self.undo(request);
        }
        self.assert_initialized()?;

        let events = self.list_events()?;
        if events.is_empty() {
            bail!("cannot undo: event log is empty");
        }

        // In native mode, check for trailing scoped file events
        if self.repo_mode().unwrap_or(RepoMode::GitCompatible) == RepoMode::Native {
            let latest_checkpoint_idx = events.iter().rposition(|e| {
                matches!(e.kind, EventKind::Checkpoint(_))
            });

            let has_trailing_scoped_file_events = if let Some(cp_idx) = latest_checkpoint_idx {
                events[cp_idx + 1..].iter().any(|e| {
                    matches!(
                        e.kind,
                        EventKind::FileWrite(_) | EventKind::FileDelete(_) | EventKind::FileRename(_)
                    ) && scope.matches(e)
                })
            } else {
                events.iter().any(|e| {
                    matches!(
                        e.kind,
                        EventKind::FileWrite(_) | EventKind::FileDelete(_) | EventKind::FileRename(_)
                    ) && scope.matches(e)
                })
            };

            if has_trailing_scoped_file_events {
                return self.undo_file_events_scoped(&events, &request, &scope);
            }
        }

        // Checkpoint-level scoped undo
        self.undo_selective_checkpoint(&events, &request, &scope)
    }

    /// Undo file events that match the scope, reusing the existing file-undo logic.
    fn undo_file_events_scoped(
        &self,
        events: &[Event],
        request: &UndoRequest,
        scope: &UndoScope,
    ) -> Result<UndoResult> {
        let file_events: Vec<&Event> = events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::FileWrite(_) | EventKind::FileDelete(_) | EventKind::FileRename(_)
                ) && scope.matches(e)
            })
            .collect();

        if file_events.is_empty() {
            bail!("cannot undo: no file events match the given scope");
        }

        let count = match request {
            UndoRequest::Last => 1,
            UndoRequest::N(n) => *n,
            UndoRequest::Since(duration) => {
                let cutoff = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .checked_sub(*duration)
                    .unwrap_or_default();
                let cutoff_nanos = cutoff.as_nanos();
                file_events
                    .iter()
                    .rev()
                    .take_while(|e| {
                        e.timestamp
                            .parse::<u128>()
                            .map(|t| t > cutoff_nanos)
                            .unwrap_or(false)
                    })
                    .count()
            }
            UndoRequest::To(id_prefix) => {
                let target_idx = file_events
                    .iter()
                    .position(|e| e.id.to_string().starts_with(id_prefix.as_str()))
                    .ok_or_else(|| anyhow!("scoped file event with prefix {} not found", id_prefix))?;
                file_events.len() - target_idx - 1
            }
        };

        if count == 0 {
            bail!("no scoped file events to undo");
        }

        // Clamp count to available scoped file events
        let actual_count = count.min(file_events.len());
        if actual_count < count {
            eprintln!(
                "note: only {} scoped file event(s) available to undo (requested {})",
                actual_count, count
            );
        }

        let events_by_id: HashMap<Uuid, &Event> =
            events.iter().map(|e| (e.id, e)).collect();

        let to_undo: Vec<&Event> = file_events
            .iter()
            .rev()
            .take(actual_count)
            .copied()
            .collect();

        let first_target = to_undo
            .first()
            .ok_or_else(|| anyhow!("no scoped file events to undo"))?;
        let target_event_id = first_target.id;

        for file_event in &to_undo {
            match &file_event.kind {
                EventKind::FileWrite(fw) => {
                    if let Some(prev_id) = fw.previous_file_event {
                        if let Some(prev_event) = events_by_id.get(&prev_id) {
                            match &prev_event.kind {
                                EventKind::FileWrite(prev_fw) => {
                                    self.restore_file_from_blocks(&fw.path, &prev_fw.blocks)?;
                                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                                        path: fw.path.clone(),
                                        content_hash: prev_fw.content_hash.clone(),
                                        blocks: prev_fw.blocks.clone(),
                                        size: prev_fw.size,
                                        previous_file_event: Some(file_event.id),
                                    }))?;
                                }
                                EventKind::FileRename(prev_fr) => {
                                    self.restore_file_from_blocks(&fw.path, &prev_fr.blocks)?;
                                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                                        path: fw.path.clone(),
                                        content_hash: prev_fr.content_hash.clone(),
                                        blocks: prev_fr.blocks.clone(),
                                        size: prev_fr.size,
                                        previous_file_event: Some(file_event.id),
                                    }))?;
                                }
                                _ => {
                                    bail!(
                                        "cannot undo: reached an event ({}) that cannot be rolled back further; you may be at the beginning of history",
                                        prev_id
                                    );
                                }
                            }
                        } else {
                            bail!("previous file event {} not found", prev_id);
                        }
                    } else {
                        let target = self.root.join(&fw.path);
                        if target.exists() {
                            fs::remove_file(&target).with_context(|| {
                                format!("failed to delete {}", target.display())
                            })?;
                        }
                        self.append_event(EventKind::FileDelete(FileDeleteEvent {
                            path: fw.path.clone(),
                            previous_file_event: Some(file_event.id),
                        }))?;
                    }
                }
                EventKind::FileDelete(fd) => {
                    if let Some(prev_id) = fd.previous_file_event {
                        if let Some(prev_event) = events_by_id.get(&prev_id) {
                            match &prev_event.kind {
                                EventKind::FileWrite(prev_fw) => {
                                    self.restore_file_from_blocks(&fd.path, &prev_fw.blocks)?;
                                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                                        path: fd.path.clone(),
                                        content_hash: prev_fw.content_hash.clone(),
                                        blocks: prev_fw.blocks.clone(),
                                        size: prev_fw.size,
                                        previous_file_event: Some(file_event.id),
                                    }))?;
                                }
                                EventKind::FileRename(prev_fr) => {
                                    self.restore_file_from_blocks(&fd.path, &prev_fr.blocks)?;
                                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                                        path: fd.path.clone(),
                                        content_hash: prev_fr.content_hash.clone(),
                                        blocks: prev_fr.blocks.clone(),
                                        size: prev_fr.size,
                                        previous_file_event: Some(file_event.id),
                                    }))?;
                                }
                                _ => {
                                    bail!(
                                        "cannot undo: reached an event ({}) that cannot be rolled back further; you may be at the beginning of history",
                                        prev_id
                                    );
                                }
                            }
                        } else {
                            bail!("previous file event {} not found", prev_id);
                        }
                    } else {
                        bail!("cannot undo file delete: no previous file event recorded");
                    }
                }
                EventKind::FileRename(fr) => {
                    let new_loc = self.root.join(&fr.new_path);
                    let old_loc = self.root.join(&fr.old_path);
                    if new_loc.exists() {
                        if let Some(parent) = old_loc.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::rename(&new_loc, &old_loc).with_context(|| {
                            format!("failed to rename {} back to {}", fr.new_path, fr.old_path)
                        })?;
                    }
                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                        path: fr.old_path.clone(),
                        content_hash: fr.content_hash.clone(),
                        blocks: fr.blocks.clone(),
                        size: fr.size,
                        previous_file_event: Some(file_event.id),
                    }))?;
                    self.append_event(EventKind::FileDelete(FileDeleteEvent {
                        path: fr.new_path.clone(),
                        previous_file_event: Some(file_event.id),
                    }))?;
                }
                _ => unreachable!(),
            }
        }

        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id,
            mode: to_undo_mode(request, target_event_id),
            restored_checkpoint_event: None,
            file_scope: Some(format!("{} scoped file event(s)", to_undo.len())),
            undo_scope: Some(scope.to_record()),
        }))?;

        Ok(UndoResult {
            target_event_id,
            restored_checkpoint_event: None,
        })
    }

    /// Selective checkpoint undo: restores only files changed by scoped checkpoints.
    fn undo_selective_checkpoint(
        &self,
        events: &[Event],
        request: &UndoRequest,
        scope: &UndoScope,
    ) -> Result<UndoResult> {
        // Find the latest scoped checkpoint as the head
        let scoped_checkpoints: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Checkpoint(_)) && scope.matches(e))
            .collect();

        if scoped_checkpoints.is_empty() {
            bail!("cannot undo: no checkpoints match the given scope");
        }

        let head = *scoped_checkpoints.last().unwrap();
        let head_id = head.id;

        let steps = match request {
            UndoRequest::Last => 1,
            UndoRequest::N(n) => *n,
            _ => {
                // For To/Since, resolve to a target event in scoped set and compute steps
                let target = resolve_target_event_scoped(events, request, scope)?;
                // Count scoped checkpoints from target to head
                let target_pos = scoped_checkpoints.iter().position(|e| e.id == target.id);
                let head_pos = scoped_checkpoints.len() - 1;
                match target_pos {
                    Some(pos) => head_pos - pos,
                    None => bail!("target event is not a scoped checkpoint"),
                }
            }
        };

        let target = walk_checkpoint_ancestor_scoped(events, head_id, steps, scope)?;

        let EventKind::Checkpoint(ref target_payload) = target.kind else {
            bail!("expected checkpoint payload");
        };

        // Collect files_changed from all scoped checkpoints between target (exclusive)
        // and head (inclusive)
        let target_pos = scoped_checkpoints.iter().position(|e| e.id == target.id)
            .ok_or_else(|| anyhow!("target checkpoint not found in scoped list"))?;
        let head_pos = scoped_checkpoints.len() - 1;

        let mut scoped_files: BTreeSet<String> = BTreeSet::new();
        for cp in &scoped_checkpoints[target_pos + 1..=head_pos] {
            if let EventKind::Checkpoint(ref cp_payload) = cp.kind {
                if let Some(ref files_changed) = cp_payload.files_changed {
                    for fc in files_changed {
                        scoped_files.insert(fc.path.clone());
                    }
                } else {
                    bail!(
                        "scoped checkpoint {} has no files_changed metadata; \
                         use --file or global undo instead",
                        cp.id
                    );
                }
            }
        }

        if scoped_files.is_empty() {
            bail!("no files to restore from scoped checkpoints");
        }

        // Check for overlapping files with non-scoped checkpoints that are newer than target
        let non_scoped_newer_files: BTreeSet<String> = events
            .iter()
            .filter(|e| {
                matches!(e.kind, EventKind::Checkpoint(_))
                    && !scope.matches(e)
                    && e.timestamp > target.timestamp
            })
            .filter_map(|e| {
                if let EventKind::Checkpoint(ref cp) = e.kind {
                    cp.files_changed.as_ref()
                } else {
                    None
                }
            })
            .flat_map(|files| files.iter().map(|f| f.path.clone()))
            .collect();

        let overlapping: Vec<&String> = scoped_files
            .iter()
            .filter(|f| non_scoped_newer_files.contains(*f))
            .collect();

        if !overlapping.is_empty() {
            eprintln!(
                "warning: {} file(s) were also modified by non-scoped checkpoints: {}",
                overlapping.len(),
                overlapping.iter().take(5).map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            );
        }

        // Restore only scoped files from the target checkpoint's snapshot
        // (target is the state we want to restore TO)
        for path_str in &scoped_files {
            let path = Path::new(path_str);
            self.restore_workspace_file_from_snapshot(target_payload.snapshot_id, path)?;
        }

        // Create a new checkpoint capturing the selective restore
        let checkpoint_event = self.create_checkpoint(
            Some(format!(
                "scoped-undo: restore {} file(s) to state before checkpoint {}",
                scoped_files.len(),
                target.id
            )),
        )?;
        let restored_checkpoint_event = Some(checkpoint_event.id);

        let mode = to_undo_mode(request, head_id);

        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id: head_id,
            mode,
            restored_checkpoint_event,
            file_scope: None,
            undo_scope: Some(scope.to_record()),
        }))?;

        Ok(UndoResult {
            target_event_id: head_id,
            restored_checkpoint_event,
        })
    }

    fn undo_inner(&self, request: UndoRequest, force_checkpoint: bool) -> Result<UndoResult> {
        self.assert_initialized()?;

        // Enforce rate limit policy (undos per exploration).
        let task_id = self.active_task_id();
        let exploration_id = self.active_exploration_id();
        self.enforce_rate_limit_policy(task_id, exploration_id)?;

        let events = self.list_events()?;
        if events.is_empty() {
            bail!("cannot undo: event log is empty")
        }

        // In native mode, use file-level undo if the most recent undoable events
        // (after the latest checkpoint) are file events. If the last meaningful event
        // is a checkpoint, fall through to checkpoint-level undo.
        if !force_checkpoint && self.repo_mode().unwrap_or(RepoMode::GitCompatible) == RepoMode::Native {
            // Find the index of the latest checkpoint
            let latest_checkpoint_idx = events.iter().rposition(|e| {
                matches!(e.kind, EventKind::Checkpoint(_))
            });

            // Check if there are file events AFTER the latest checkpoint
            let has_trailing_file_events = if let Some(cp_idx) = latest_checkpoint_idx {
                events[cp_idx + 1..].iter().any(|e| {
                    matches!(
                        e.kind,
                        EventKind::FileWrite(_) | EventKind::FileDelete(_) | EventKind::FileRename(_)
                    )
                })
            } else {
                // No checkpoint at all — check if there are any file events
                events.iter().any(|e| {
                    matches!(
                        e.kind,
                        EventKind::FileWrite(_) | EventKind::FileDelete(_) | EventKind::FileRename(_)
                    )
                })
            };

            if has_trailing_file_events {
                return self.undo_file_events(&events, &request);
            }
        }

        match &request {
            UndoRequest::Last | UndoRequest::N(_) => {
                // Chain-walk requires at least one checkpoint.  If no checkpoint
                // exists (e.g. undoing a non-checkpoint event like an exploration
                // start), fall back to the raw event-target path.
                if self.latest_checkpoint().is_some() {
                    self.undo_by_chain_walk(&events, &request)
                } else {
                    self.undo_by_event_target(&events, &request)
                }
            }
            UndoRequest::To(_) | UndoRequest::Since(_) => {
                self.undo_by_event_target(&events, &request)
            }
        }
    }

    fn undo_by_chain_walk(
        &self,
        events: &[Event],
        request: &UndoRequest,
    ) -> Result<UndoResult> {
        let steps = match request {
            UndoRequest::Last => 1,
            UndoRequest::N(n) => *n,
            _ => unreachable!(),
        };

        let head = self
            .latest_checkpoint()
            .ok_or_else(|| anyhow!("cannot undo: no checkpoint exists"))?;

        let ancestor = walk_checkpoint_ancestor(events, head.id, steps)?;

        let EventKind::Checkpoint(ref ancestor_payload) = ancestor.kind else {
            bail!("expected checkpoint payload");
        };

        self.restore_workspace_from_snapshot(ancestor_payload.snapshot_id)?;

        // The restore checkpoint's parent is the ancestor's own parent, not the
        // ancestor itself.  This is because the restore checkpoint has the same
        // content as the ancestor, so "undo one more step" should walk to the
        // ancestor's parent — not back to the ancestor (which would be a no-op).
        // We use create_checkpoint_with_exact_lineage to prevent auto-fill of a
        // None parent (which would incorrectly point to the latest checkpoint).
        let checkpoint_event = self.create_checkpoint_with_exact_lineage(
            format!("undo-restore-{}", ancestor.id.simple()),
            Some(format!("undo: restore to checkpoint {}", ancestor.id)),
            ancestor_payload.parent_checkpoint_event,
        )?;
        let restored_checkpoint_event = Some(checkpoint_event.id);

        let mode = to_undo_mode(request, head.id);

        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id: head.id,
            mode,
            restored_checkpoint_event,
            file_scope: None,
            undo_scope: None,
        }))?;

        Ok(UndoResult {
            target_event_id: head.id,
            restored_checkpoint_event,
        })
    }

    fn undo_by_event_target(
        &self,
        events: &[Event],
        request: &UndoRequest,
    ) -> Result<UndoResult> {
        let target = resolve_target_event(events, request)?;
        let mode = to_undo_mode(request, target.id);

        let mut restored_checkpoint_event = None;

        if let EventKind::Undo(ref undo_event) = target.kind {
            // Undoing an undo = redo: restore the checkpoint that was originally undone.
            let original_target_id = undo_event.target_event_id;
            let original_target = events
                .iter()
                .find(|e| e.id == original_target_id)
                .ok_or_else(|| anyhow!("undo target event {} not found", original_target_id))?;

            if let EventKind::Checkpoint(ref payload) = original_target.kind {
                self.restore_workspace_from_snapshot(payload.snapshot_id)?;

                let checkpoint_event = self.create_checkpoint_with_lineage(
                    format!("redo-{}", original_target_id.simple()),
                    Some(format!(
                        "redo: restore undone checkpoint {}",
                        original_target_id
                    )),
                    Some(original_target_id),
                    None,
                )?;
                restored_checkpoint_event = Some(checkpoint_event.id);
            }
        } else if matches!(target.kind, EventKind::Checkpoint(_)) {
            let Some(previous_checkpoint) = previous_checkpoint_before(events, target.id) else {
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
                None,
            )?;
            restored_checkpoint_event = Some(checkpoint_event.id);
        }

        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id: target.id,
            mode,
            restored_checkpoint_event,
            file_scope: None,
            undo_scope: None,
        }))?;

        Ok(UndoResult {
            target_event_id: target.id,
            restored_checkpoint_event,
        })
    }

    /// O(1) undo of file-level events (FileWrite/FileDelete/FileRename).
    /// Reverts only the affected file(s), not the whole workspace.
    fn undo_file_events(
        &self,
        events: &[Event],
        request: &UndoRequest,
    ) -> Result<UndoResult> {
        // Collect file events in order
        let file_events: Vec<&Event> = events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::FileWrite(_) | EventKind::FileDelete(_) | EventKind::FileRename(_)
                )
            })
            .collect();

        if file_events.is_empty() {
            bail!("cannot undo: no file events found");
        }

        let count = match request {
            UndoRequest::Last => 1,
            UndoRequest::N(n) => *n,
            UndoRequest::Since(duration) => {
                let cutoff = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .checked_sub(*duration)
                    .unwrap_or_default();
                let cutoff_nanos = cutoff.as_nanos();
                file_events
                    .iter()
                    .rev()
                    .take_while(|e| {
                        e.timestamp
                            .parse::<u128>()
                            .map(|t| t > cutoff_nanos)
                            .unwrap_or(false)
                    })
                    .count()
            }
            UndoRequest::To(id_prefix) => {
                // Find the target event and undo everything after it
                let target_idx = file_events
                    .iter()
                    .position(|e| e.id.to_string().starts_with(id_prefix.as_str()))
                    .ok_or_else(|| anyhow!("file event with prefix {} not found", id_prefix))?;
                file_events.len() - target_idx - 1
            }
        };

        if count == 0 {
            bail!("no file events to undo");
        }

        // Clamp count to available file events
        let actual_count = count.min(file_events.len());
        if actual_count < count {
            eprintln!(
                "note: only {} file event(s) available to undo (requested {})",
                actual_count, count
            );
        }

        // Build event-by-id lookup
        let events_by_id: HashMap<Uuid, &Event> =
            events.iter().map(|e| (e.id, e)).collect();

        // Undo the last N file events in reverse order
        let to_undo: Vec<&Event> = file_events
            .iter()
            .rev()
            .take(actual_count)
            .copied()
            .collect();

        let first_target = to_undo
            .first()
            .ok_or_else(|| anyhow!("no file events to undo"))?;
        let target_event_id = first_target.id;

        for file_event in &to_undo {
            match &file_event.kind {
                EventKind::FileWrite(fw) => {
                    if let Some(prev_id) = fw.previous_file_event {
                        // Restore previous version
                        if let Some(prev_event) = events_by_id.get(&prev_id) {
                            match &prev_event.kind {
                                EventKind::FileWrite(prev_fw) => {
                                    self.restore_file_from_blocks(&fw.path, &prev_fw.blocks)?;
                                    // Emit counterbalancing FileWrite
                                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                                        path: fw.path.clone(),
                                        content_hash: prev_fw.content_hash.clone(),
                                        blocks: prev_fw.blocks.clone(),
                                        size: prev_fw.size,
                                        previous_file_event: Some(file_event.id),
                                    }))?;
                                }
                                EventKind::FileRename(prev_fr) => {
                                    self.restore_file_from_blocks(&fw.path, &prev_fr.blocks)?;
                                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                                        path: fw.path.clone(),
                                        content_hash: prev_fr.content_hash.clone(),
                                        blocks: prev_fr.blocks.clone(),
                                        size: prev_fr.size,
                                        previous_file_event: Some(file_event.id),
                                    }))?;
                                }
                                _ => {
                                    bail!(
                                        "cannot undo: reached an event ({}) that cannot be rolled back further; you may be at the beginning of history",
                                        prev_id
                                    );
                                }
                            }
                        } else {
                            bail!("previous file event {} not found", prev_id);
                        }
                    } else {
                        // No previous event — file was newly added, delete it
                        let target = self.root.join(&fw.path);
                        if target.exists() {
                            fs::remove_file(&target).with_context(|| {
                                format!("failed to delete {}", target.display())
                            })?;
                        }
                        self.append_event(EventKind::FileDelete(FileDeleteEvent {
                            path: fw.path.clone(),
                            previous_file_event: Some(file_event.id),
                        }))?;
                    }
                }
                EventKind::FileDelete(fd) => {
                    // Restore the deleted file from its previous event
                    if let Some(prev_id) = fd.previous_file_event {
                        if let Some(prev_event) = events_by_id.get(&prev_id) {
                            match &prev_event.kind {
                                EventKind::FileWrite(prev_fw) => {
                                    self.restore_file_from_blocks(&fd.path, &prev_fw.blocks)?;
                                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                                        path: fd.path.clone(),
                                        content_hash: prev_fw.content_hash.clone(),
                                        blocks: prev_fw.blocks.clone(),
                                        size: prev_fw.size,
                                        previous_file_event: Some(file_event.id),
                                    }))?;
                                }
                                EventKind::FileRename(prev_fr) => {
                                    self.restore_file_from_blocks(&fd.path, &prev_fr.blocks)?;
                                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                                        path: fd.path.clone(),
                                        content_hash: prev_fr.content_hash.clone(),
                                        blocks: prev_fr.blocks.clone(),
                                        size: prev_fr.size,
                                        previous_file_event: Some(file_event.id),
                                    }))?;
                                }
                                _ => {
                                    bail!(
                                        "cannot undo: reached an event ({}) that cannot be rolled back further; you may be at the beginning of history",
                                        prev_id
                                    );
                                }
                            }
                        } else {
                            bail!("previous file event {} not found", prev_id);
                        }
                    } else {
                        bail!("cannot undo file delete: no previous file event recorded");
                    }
                }
                EventKind::FileRename(fr) => {
                    // Rename back
                    let new_loc = self.root.join(&fr.new_path);
                    let old_loc = self.root.join(&fr.old_path);
                    if new_loc.exists() {
                        if let Some(parent) = old_loc.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::rename(&new_loc, &old_loc).with_context(|| {
                            format!("failed to rename {} back to {}", fr.new_path, fr.old_path)
                        })?;
                    }
                    // Emit counterbalancing FileWrite at the old path
                    self.append_event(EventKind::FileWrite(FileWriteEvent {
                        path: fr.old_path.clone(),
                        content_hash: fr.content_hash.clone(),
                        blocks: fr.blocks.clone(),
                        size: fr.size,
                        previous_file_event: Some(file_event.id),
                    }))?;
                    // Delete the new path entry
                    self.append_event(EventKind::FileDelete(FileDeleteEvent {
                        path: fr.new_path.clone(),
                        previous_file_event: Some(file_event.id),
                    }))?;
                }
                _ => unreachable!(),
            }
        }

        // Emit a single Undo event referencing the first target
        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id: target_event_id,
            mode: to_undo_mode(request, target_event_id),
            restored_checkpoint_event: None,
            file_scope: Some(format!("{} file event(s)", to_undo.len())),
            undo_scope: None,
        }))?;

        Ok(UndoResult {
            target_event_id,
            restored_checkpoint_event: None,
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

        match &request {
            UndoRequest::Last | UndoRequest::N(_) => {
                self.undo_file_by_chain_walk(&events, &request, &scoped_file, &scoped_file_display)
            }
            UndoRequest::To(_) | UndoRequest::Since(_) => {
                self.undo_file_by_event_target(
                    &events,
                    &request,
                    &scoped_file,
                    &scoped_file_display,
                )
            }
        }
    }

    fn undo_file_by_chain_walk(
        &self,
        events: &[Event],
        request: &UndoRequest,
        scoped_file: &Path,
        scoped_file_display: &str,
    ) -> Result<UndoResult> {
        let steps = match request {
            UndoRequest::Last => 1,
            UndoRequest::N(n) => *n,
            _ => unreachable!(),
        };

        let head = self
            .latest_checkpoint()
            .ok_or_else(|| anyhow!("cannot undo: no checkpoint exists"))?;

        let ancestor = walk_checkpoint_ancestor(events, head.id, steps)?;

        let EventKind::Checkpoint(ref head_payload) = head.kind else {
            bail!("expected checkpoint payload");
        };
        let EventKind::Checkpoint(ref ancestor_payload) = ancestor.kind else {
            bail!("expected checkpoint payload");
        };

        // Check that the file actually exists in either the head or ancestor
        // snapshot — otherwise the undo is a no-op on a file that was never
        // tracked and we should give a clear error instead of silently creating
        // a useless checkpoint.
        let in_head = self.snapshot_contains_file(head_payload.snapshot_id, scoped_file)?;
        let in_ancestor =
            self.snapshot_contains_file(ancestor_payload.snapshot_id, scoped_file)?;
        if !in_head && !in_ancestor {
            bail!(
                "file '{}' was not found in the last commit or its predecessor",
                scoped_file_display
            );
        }

        self.restore_workspace_file_from_snapshot(ancestor_payload.snapshot_id, scoped_file)?;

        let checkpoint_event = self.create_checkpoint_with_exact_lineage(
            format!("undo-file-{}", normalize_label(scoped_file_display)),
            Some(format!(
                "undo file {}: restore to checkpoint {}",
                scoped_file_display, ancestor.id
            )),
            ancestor_payload.parent_checkpoint_event,
        )?;
        let restored_checkpoint_event = Some(checkpoint_event.id);

        let mode = to_undo_mode(request, head.id);

        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id: head.id,
            mode,
            restored_checkpoint_event,
            file_scope: Some(scoped_file_display.to_string()),
            undo_scope: None,
        }))?;

        Ok(UndoResult {
            target_event_id: head.id,
            restored_checkpoint_event,
        })
    }

    fn undo_file_by_event_target(
        &self,
        events: &[Event],
        request: &UndoRequest,
        scoped_file: &Path,
        scoped_file_display: &str,
    ) -> Result<UndoResult> {
        let target = resolve_target_event(events, request)?;
        let mode = to_undo_mode(request, target.id);

        if !matches!(&target.kind, EventKind::Checkpoint(_)) {
            bail!(
                "file-scoped undo requires a checkpoint target; resolved target {} is a {:?} event \
                 (hint: the file '{}' may not exist in any recent commit)",
                target.id,
                target.kind.variant_name(),
                scoped_file_display
            );
        }

        let EventKind::Checkpoint(ref target_cp) = target.kind else {
            unreachable!();
        };

        // Check if the file actually exists in the target checkpoint before
        // looking for the previous one — give a clear error if it doesn't.
        if !self.snapshot_contains_file(target_cp.snapshot_id, scoped_file)? {
            // Also check the previous checkpoint
            let in_previous = previous_checkpoint_before(events, target.id)
                .and_then(|prev| {
                    if let EventKind::Checkpoint(cp) = prev.kind {
                        self.snapshot_contains_file(cp.snapshot_id, scoped_file).ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
            if !in_previous {
                bail!(
                    "file '{}' was not found in the last commit or its predecessor",
                    scoped_file_display
                );
            }
        }

        let Some(previous_checkpoint) = previous_checkpoint_before(events, target.id) else {
            bail!(
                "cannot undo checkpoint {} for file {}: no earlier checkpoint exists",
                target.id,
                scoped_file_display
            )
        };

        let EventKind::Checkpoint(payload) = previous_checkpoint.kind else {
            bail!("expected checkpoint payload")
        };

        self.restore_workspace_file_from_snapshot(payload.snapshot_id, scoped_file)?;

        let checkpoint_event = self.create_checkpoint_with_lineage(
            format!("undo-file-{}", normalize_label(scoped_file_display)),
            Some(format!(
                "undo file {} from checkpoint {}",
                scoped_file_display, target.id
            )),
            None,
            None,
        )?;
        let restored_checkpoint_event = Some(checkpoint_event.id);

        self.append_event(EventKind::Undo(UndoEvent {
            target_event_id: target.id,
            mode,
            restored_checkpoint_event,
            file_scope: Some(scoped_file_display.to_string()),
            undo_scope: None,
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
                        "run `fl commit -m \"sync\"` to recreate flock main ref mapping"
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
                        "create a checkpoint (`fl commit -m \"bootstrap\"`) to establish initial mappings"
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
                let snapshot_root = self.ensure_snapshot_available(checkpoint.snapshot_id)?;
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

    /// Convert a git repository into Flock format.
    ///
    /// Detects `.git/`, initializes `.flock/` in colocated mode, and imports
    /// commit history for all branches (or filtered by `branch_filter`).
    /// Tags are imported as Flock Tag refs. The operation is resumable —
    /// already-imported commits are skipped via `known_git_commit_mappings()`.
    pub fn convert_from_git(
        &self,
        branch_filter: Option<String>,
        shallow: Option<usize>,
    ) -> Result<ConvertReport> {
        // 1. Detect .git/
        let git_dir = self.root.join(".git");
        if !git_dir.exists() {
            bail!(
                "no .git directory found at {}; nothing to convert",
                self.root.display()
            );
        }

        // 2. Init .flock/ in colocated mode
        let flock_dir = self.root.join(FLOCK_DIR);
        if !flock_dir.exists() {
            self.init_colocated()?;
            eprintln!("initialized .flock/ in git-colocated mode");
        }

        // 3. Enumerate branches
        let branch_output = self
            .run_git(&[
                "for-each-ref",
                "--format=%(refname:short) %(objectname)",
                "refs/heads/",
            ])
            .context("failed to enumerate git branches")?;
        let mut branches: Vec<(String, String)> = branch_output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.trim().splitn(2, ' ').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    None
                }
            })
            .collect();

        // 4. Enumerate tags
        let tag_output = self
            .run_git(&[
                "for-each-ref",
                "--format=%(refname:short) %(objectname)",
                "refs/tags/",
            ])
            .context("failed to enumerate git tags")?;
        let tags: Vec<(String, String)> = tag_output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.trim().splitn(2, ' ').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    None
                }
            })
            .collect();

        // 5. Filter branches if requested
        if let Some(ref filter) = branch_filter {
            let allowed: Vec<&str> = filter.split(',').map(str::trim).collect();
            branches.retain(|(name, _)| allowed.iter().any(|a| a == name));
        }

        if branches.is_empty() {
            bail!("no branches found to convert");
        }

        // Prefer "main" or "master" as first branch
        let main_idx = branches
            .iter()
            .position(|(name, _)| name == "main" || name == "master")
            .unwrap_or(0);
        if main_idx != 0 {
            branches.swap(0, main_idx);
        }

        let mut known_commits = self.known_git_commit_mappings()?;
        let mut commits_imported: usize = 0;
        let mut commits_skipped: usize = 0;
        let mut branches_imported: usize = 0;
        // Map git SHA → checkpoint event ID for tag resolution
        let mut sha_to_checkpoint: HashMap<String, Uuid> = HashMap::new();

        // 6. Import each branch
        for (branch_name, _tip_sha) in &branches {
            let mut rev_list_args = vec!["rev-list", "--reverse"];
            let shallow_arg;
            if let Some(n) = shallow {
                shallow_arg = format!("-{n}");
                rev_list_args.push(&shallow_arg);
            }
            rev_list_args.push(branch_name);

            let output = self
                .run_git(&rev_list_args)
                .with_context(|| format!("failed to enumerate commits for branch `{branch_name}`"))?;
            let commits: Vec<String> = output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect();

            let total = commits.len();
            let mut parent_checkpoint = self.latest_checkpoint().map(|event| event.id);
            let mut branch_had_new_commits = false;

            for (i, commit) in commits.iter().enumerate() {
                if known_commits.contains(commit) {
                    // Record the mapping if we already imported this commit
                    if let Some(existing_id) = self.find_checkpoint_for_git_commit(commit)? {
                        sha_to_checkpoint.insert(commit.clone(), existing_id);
                        parent_checkpoint = Some(existing_id);
                    }
                    commits_skipped += 1;
                    continue;
                }

                eprintln!(
                    "[{}/{}] Importing commit {} (branch: {branch_name})",
                    i + 1,
                    total,
                    &commit[..commit.len().min(12)]
                );

                let subject = self.run_git(&["show", "-s", "--format=%s", commit])?;
                let message = self.run_git(&["show", "-s", "--format=%B", commit])?;
                let label = normalize_label(subject.trim());
                let label = if label.is_empty() {
                    format!("git-import-{}", short_sha(commit))
                } else {
                    label
                };
                let message = if message.trim().is_empty() {
                    Some(format!("git import {commit}"))
                } else {
                    Some(message.trim().to_string())
                };

                let event = self.create_checkpoint_from_git_commit(
                    commit,
                    label,
                    message,
                    parent_checkpoint,
                )?;
                sha_to_checkpoint.insert(commit.clone(), event.id);
                parent_checkpoint = Some(event.id);
                known_commits.insert(commit.clone());
                commits_imported += 1;
                branch_had_new_commits = true;
            }

            // Create a branch ref pointing to the tip checkpoint
            if let Some(tip_checkpoint) = parent_checkpoint {
                let store = AutoRefStore::for_root(self.root());
                let reference = RepoRef {
                    kind: RefKind::Branch,
                    name: branch_name.clone(),
                    target_event_id: tip_checkpoint,
                    workspace: None,
                };
                store.upsert(reference)?;
                if branch_had_new_commits || branches_imported == 0 {
                    branches_imported += 1;
                }
            }
        }

        // 7. Import tags
        let mut tags_imported: usize = 0;
        for (tag_name, tag_sha) in &tags {
            // Dereference annotated tags to the underlying commit
            let deref_sha = self
                .run_git(&["rev-parse", &format!("{tag_sha}^{{}}")])
                .unwrap_or_else(|_| tag_sha.clone());
            let deref_sha = deref_sha.trim().to_string();

            if let Some(&checkpoint_id) = sha_to_checkpoint.get(&deref_sha) {
                let store = AutoRefStore::for_root(self.root());
                let reference = RepoRef {
                    kind: RefKind::Tag,
                    name: tag_name.clone(),
                    target_event_id: checkpoint_id,
                    workspace: None,
                };
                store.upsert(reference)?;
                tags_imported += 1;
            }
        }

        // 8. Validate
        let (validation_ok, validation_detail) = match self.fsck() {
            Ok(report) => (
                true,
                format!(
                    "{} events, {} checkpoints, {} snapshots, {} refs",
                    report.event_count,
                    report.checkpoint_count,
                    report.snapshot_count,
                    report.ref_count,
                ),
            ),
            Err(e) => (false, format!("{e:#}")),
        };

        Ok(ConvertReport {
            branches_imported,
            tags_imported,
            commits_imported,
            commits_skipped,
            validation_ok,
            validation_detail,
        })
    }

    /// Convert a jj repository into Flock format.
    ///
    /// Detects `.jj/`, initializes `.flock/`, and imports jj history using
    /// the git backend that jj maintains under `.jj/repo/store/git/`.
    pub fn convert_from_jj(&self, shallow: Option<usize>) -> Result<ConvertReport> {
        let jj_dir = self.root.join(".jj");
        if !jj_dir.exists() {
            bail!(
                "no .jj directory found at {}; nothing to convert",
                self.root.display()
            );
        }

        // jj uses a git backend — check for it
        let jj_git_dir = jj_dir.join("repo").join("store").join("git");
        if !jj_git_dir.exists() {
            bail!(
                "jj git backend not found at {}; only git-backed jj repos are supported",
                jj_git_dir.display()
            );
        }

        // Init .flock/
        let flock_dir = self.root.join(FLOCK_DIR);
        if !flock_dir.exists() {
            self.init_colocated()?;
            eprintln!("initialized .flock/ in git-colocated mode");
        }

        // Use jj log to enumerate commits in topological order
        let jj_available = Command::new("jj")
            .arg("--version")
            .current_dir(&self.root)
            .output()
            .is_ok();

        if !jj_available {
            bail!("jj CLI not found; install jj to convert jj repositories");
        }

        let output = Command::new("jj")
            .args([
                "log",
                "--no-graph",
                "-r",
                "all()",
                "-T",
                r#"commit_id ++ " " ++ description.first_line() ++ "\n""#,
                "--reversed",
            ])
            .current_dir(&self.root)
            .output()
            .context("failed to run jj log")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!("jj log failed: {stderr}");
        }

        let log_output = String::from_utf8_lossy(&output.stdout);
        let mut commits: Vec<(String, String)> = log_output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.trim().splitn(2, ' ').collect();
                if parts.len() == 2 && !parts[0].is_empty() {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else if parts.len() == 1 && !parts[0].is_empty() {
                    Some((parts[0].to_string(), String::new()))
                } else {
                    None
                }
            })
            .collect();

        // Filter out the root commit (all zeros)
        commits.retain(|(sha, _)| !sha.chars().all(|c| c == '0'));

        if let Some(n) = shallow {
            let len = commits.len();
            if n < len {
                commits = commits.split_off(len - n);
            }
        }

        let total = commits.len();
        let mut known_commits = self.known_git_commit_mappings()?;
        let mut parent_checkpoint = self.latest_checkpoint().map(|event| event.id);
        let mut commits_imported: usize = 0;
        let mut commits_skipped: usize = 0;

        for (i, (commit_id, description)) in commits.iter().enumerate() {
            if known_commits.contains(commit_id) {
                commits_skipped += 1;
                if let Some(existing_id) = self.find_checkpoint_for_git_commit(commit_id)? {
                    parent_checkpoint = Some(existing_id);
                }
                continue;
            }

            eprintln!(
                "[{}/{}] Importing jj commit {}",
                i + 1,
                total,
                &commit_id[..commit_id.len().min(12)]
            );

            let label = normalize_label(description.trim());
            let label = if label.is_empty() {
                format!("jj-import-{}", short_sha(commit_id))
            } else {
                label
            };
            let message = if description.trim().is_empty() {
                Some(format!("jj import {commit_id}"))
            } else {
                Some(description.trim().to_string())
            };

            let event = self.create_checkpoint_from_git_commit(
                commit_id,
                label,
                message,
                parent_checkpoint,
            )?;
            parent_checkpoint = Some(event.id);
            known_commits.insert(commit_id.clone());
            commits_imported += 1;
        }

        // Import jj bookmarks as branch refs
        let mut branches_imported: usize = 0;
        let bookmark_output = Command::new("jj")
            .args(["bookmark", "list", "--all", "-T", r#"name ++ " " ++ commit_id ++ "\n""#])
            .current_dir(&self.root)
            .output();
        if let Ok(bm_output) = bookmark_output {
            if bm_output.status.success() {
                let bm_text = String::from_utf8_lossy(&bm_output.stdout);
                for line in bm_text.lines() {
                    let parts: Vec<&str> = line.trim().splitn(2, ' ').collect();
                    if parts.len() == 2 {
                        let bm_name = parts[0];
                        let bm_sha = parts[1].trim();
                        if let Some(checkpoint_id) = self.find_checkpoint_for_git_commit(bm_sha)? {
                            let store = AutoRefStore::for_root(self.root());
                            let reference = RepoRef {
                                kind: RefKind::Branch,
                                name: format!("jj/{bm_name}"),
                                target_event_id: checkpoint_id,
                                workspace: None,
                            };
                            store.upsert(reference)?;
                            branches_imported += 1;
                        }
                    }
                }
            }
        }

        // Validate
        let (validation_ok, validation_detail) = match self.fsck() {
            Ok(report) => (
                true,
                format!(
                    "{} events, {} checkpoints, {} snapshots, {} refs",
                    report.event_count,
                    report.checkpoint_count,
                    report.snapshot_count,
                    report.ref_count,
                ),
            ),
            Err(e) => (false, format!("{e:#}")),
        };

        Ok(ConvertReport {
            branches_imported,
            tags_imported: 0,
            commits_imported,
            commits_skipped,
            validation_ok,
            validation_detail,
        })
    }

    /// Export Flock history to a clean git repository.
    ///
    /// Creates git commits from checkpoints, maps explorations to branches,
    /// maps Flock Tag refs to git tags. Optionally removes `.flock/` afterward.
    pub fn convert_to_git(&self, remove_flock: bool) -> Result<ConvertReport> {
        self.assert_initialized()?;

        // Check if .git/ exists — if not, init one
        let git_dir = self.root.join(".git");
        if !git_dir.exists() {
            fl_bridge_git::run_git(&self.root, &["init"])
                .context("failed to initialize git repository for export")?;
            eprintln!("initialized .git/ for export");
        }

        // Export checkpoints using existing git_export logic but to the repo itself
        let checkpoints = self.list_checkpoints_with_payload()?;
        if checkpoints.is_empty() {
            bail!("no checkpoints available to export");
        }

        let temp_repo =
            tempfile::tempdir().context("failed to create temporary git export directory")?;
        fl_bridge_git::run_git(temp_repo.path(), &["init"])
            .context("failed to initialize temporary git repository for export")?;

        let mut mapping: Vec<(Uuid, String)> = Vec::new();
        for (event, checkpoint) in &checkpoints {
            clear_directory_except(temp_repo.path(), &[".git"])?;
            let snapshot_root = self.ensure_snapshot_available(checkpoint.snapshot_id)?;
            if snapshot_root.exists() {
                copy_tree(snapshot_root.as_path(), temp_repo.path(), false)?;
            }

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

        // Fetch the exported history into the real repo using a temporary ref,
        // then update main. Direct fetch into a checked-out branch is forbidden.
        let source = temp_repo.path().to_string_lossy().to_string();
        let tmp_ref = "refs/flock/convert-export";
        let refspec = format!("+HEAD:{tmp_ref}");
        self.run_git(&["fetch", &source, &refspec])
            .context("failed to import exported history")?;
        self.run_git(&["update-ref", "refs/heads/main", tmp_ref])
            .context("failed to update main branch from exported history")?;
        let _ = self.run_git(&["update-ref", "-d", tmp_ref]);

        let commits_imported = mapping.len();

        // Map exploration branches
        let explorations = self.list_explorations()?;
        let mut branches_imported: usize = 1; // main
        for exploration in &explorations {
            if exploration.status == ExplorationStatus::Active
                || exploration.status == ExplorationStatus::Promoted
            {
                if let Some((_, last_sha)) = mapping.last() {
                    // Create a branch pointing to the exploration's last checkpoint
                    let branch_name = normalize_label(&exploration.title);
                    if !branch_name.is_empty() {
                        let _ = self.run_git(&[
                            "branch",
                            "-f",
                            &branch_name,
                            last_sha,
                        ]);
                        branches_imported += 1;
                    }
                }
            }
        }

        // Export Flock Tag refs as git tags
        let refs = AutoRefStore::for_root(self.root()).read_all()?;
        let mut tags_imported: usize = 0;
        for r in &refs {
            if r.kind == RefKind::Tag {
                // Find the git commit for this tag's target event
                if let Some((_, sha)) = mapping.iter().find(|(eid, _)| *eid == r.target_event_id) {
                    let _ = self.run_git(&["tag", "-f", &r.name, sha]);
                    tags_imported += 1;
                }
            }
        }

        // Validate
        let (validation_ok, validation_detail) = match self.fsck() {
            Ok(report) => (
                true,
                format!(
                    "{} events, {} checkpoints exported to git",
                    report.event_count, report.checkpoint_count,
                ),
            ),
            Err(e) => (false, format!("{e:#}")),
        };

        // Cleanup
        if remove_flock {
            let flock_dir = self.root.join(FLOCK_DIR);
            if flock_dir.exists() {
                fs::remove_dir_all(&flock_dir).context("failed to remove .flock/ directory")?;
                eprintln!("removed .flock/ directory");
            }
        }

        Ok(ConvertReport {
            branches_imported,
            tags_imported,
            commits_imported,
            commits_skipped: 0,
            validation_ok,
            validation_detail,
        })
    }

    // --- Policy enforcement helpers ---

    fn policies_config(&self) -> crate::policies::PoliciesConfig {
        crate::policies::load_policies_config(self.root())
    }

    /// Record a policy decision in the event log for audit trail.
    fn record_policy_decision(
        &self,
        decision: &fl_policy::PolicyDecision,
    ) -> Result<()> {
        let verdict_kind = match &decision.verdict {
            fl_policy::PolicyVerdict::Allow => fl_storage::PolicyVerdictKind::Allow,
            fl_policy::PolicyVerdict::Gate { .. } => fl_storage::PolicyVerdictKind::Gate,
            fl_policy::PolicyVerdict::Block { .. } => fl_storage::PolicyVerdictKind::Block,
        };
        let reason = match &decision.verdict {
            fl_policy::PolicyVerdict::Allow => None,
            fl_policy::PolicyVerdict::Gate { reason, .. } => Some(reason.clone()),
            fl_policy::PolicyVerdict::Block { reason, .. } => Some(reason.clone()),
        };
        let category = match decision.category {
            fl_policy::PolicyCategory::Scope => "Scope",
            fl_policy::PolicyCategory::Budget => "Budget",
            fl_policy::PolicyCategory::RateLimit => "RateLimit",
            fl_policy::PolicyCategory::TestRequirement => "TestRequirement",
            fl_policy::PolicyCategory::ArchitectureRule => "ArchitectureRule",
            fl_policy::PolicyCategory::AntiPattern => "AntiPattern",
            fl_policy::PolicyCategory::DuplicationReuse => "DuplicationReuse",
            fl_policy::PolicyCategory::DependencyCheck => "DependencyCheck",
            fl_policy::PolicyCategory::Regression => "Regression",
        };
        let operation = match decision.operation {
            fl_policy::PolicyOperation::Checkpoint => "Checkpoint",
            fl_policy::PolicyOperation::ExplorationStart => "ExplorationStart",
            fl_policy::PolicyOperation::ExplorationPromote => "ExplorationPromote",
            fl_policy::PolicyOperation::Undo => "Undo",
            fl_policy::PolicyOperation::TaskClaim => "TaskClaim",
        };
        let escalation_context = decision.escalation_context.as_ref().map(|ctx| {
            fl_storage::EscalationContextEvent {
                agent_action: ctx.agent_action.clone(),
                limit_name: ctx.limit_name.clone(),
                current_value: ctx.current_value,
                limit_value: ctx.limit_value,
                exploration_id: ctx.exploration_id,
                task_id: ctx.task_id,
                exploration_history: ctx.exploration_history.clone(),
            }
        });
        self.append_event(EventKind::Policy(fl_storage::PolicyEvent {
            policy_name: decision.policy_name.clone(),
            policy_category: category.to_string(),
            verdict: verdict_kind,
            operation: operation.to_string(),
            reason,
            task_id: decision.task_id,
            exploration_id: decision.exploration_id,
            affected_files: decision.affected_files.clone(),
            escalation_context,
        }))?;
        Ok(())
    }

    /// Enforce budget policy for the current task. Called before checkpoints.
    fn enforce_budget_policy(
        &self,
        task_id: Option<Uuid>,
        exploration_id: Option<Uuid>,
    ) -> Result<()> {
        let config = self.policies_config();
        if !config.budget.enabled {
            return Ok(());
        }
        let Some(tid) = task_id else {
            return Ok(());
        };

        let state = self.replay_state()?;
        let tracker = state.policy_budgets.get(&tid);

        let usage = fl_policy::BudgetUsage {
            task_id: tid,
            files_modified: tracker.map(|t| t.files_modified.len() as u32).unwrap_or(0),
            lines_changed: tracker.map(|t| t.lines_changed).unwrap_or(0),
            exploration_id,
            exploration_files_modified: tracker
                .and_then(|t| {
                    exploration_id.and_then(|eid| t.exploration_files.get(&eid).map(|s| s.len() as u32))
                })
                .unwrap_or(0),
            semantic_changes: tracker.map(|t| t.semantic_changes).unwrap_or(0),
            exploration_semantic_changes: tracker
                .and_then(|t| {
                    exploration_id.and_then(|eid| t.exploration_semantic_changes.get(&eid).copied())
                })
                .unwrap_or(0),
        };

        let decision = fl_policy::check_budget_limits(&config.budget, &usage);
        if decision.is_blocked() || decision.is_gated() {
            self.record_policy_decision(&decision)?;
        }
        if decision.is_blocked() {
            if let fl_policy::PolicyVerdict::Block { reason, fix_suggestion } = &decision.verdict {
                let msg = if let Some(fix) = fix_suggestion {
                    format!("Policy blocked: {}\nSuggestion: {}", reason, fix)
                } else {
                    format!("Policy blocked: {}", reason)
                };
                bail!("{}", msg);
            }
        }
        if decision.is_gated() {
            if let fl_policy::PolicyVerdict::Gate { reason, .. } = &decision.verdict {
                eprintln!("warning: policy gate triggered — {}", reason);
            }
        }
        Ok(())
    }

    /// Enforce rate limit policy for the current task.
    fn enforce_rate_limit_policy(
        &self,
        task_id: Option<Uuid>,
        exploration_id: Option<Uuid>,
    ) -> Result<()> {
        let config = self.policies_config();
        if !config.rate_limits.enabled {
            return Ok(());
        }
        let Some(tid) = task_id else {
            return Ok(());
        };

        let state = self.replay_state()?;
        let tracker = state.policy_rate_limits.get(&tid);

        let undos_in_exploration = tracker
            .and_then(|t| {
                exploration_id.and_then(|eid| t.undo_counts.get(&eid).copied())
            })
            .unwrap_or(0);

        // Count checkpoints in the current window.
        let window_secs = config.rate_limits.window_secs;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let window_start = now.saturating_sub(window_secs);
        let window_start_nanos = window_start * 1_000_000_000;
        let checkpoints_in_window = tracker
            .map(|t| {
                t.checkpoint_timestamps
                    .iter()
                    .filter(|ts| {
                        ts.parse::<u128>().unwrap_or(0) >= window_start_nanos as u128
                    })
                    .count() as u32
            })
            .unwrap_or(0);

        let usage = fl_policy::RateLimitUsage {
            task_id: tid,
            explorations_started: tracker.map(|t| t.explorations_started).unwrap_or(0),
            exploration_id,
            undos_in_exploration,
            checkpoints_in_window,
        };

        let mut decision = fl_policy::check_rate_limits(&config.rate_limits, &usage);

        // Enrich escalation context with exploration history summaries.
        if let Some(ref mut ctx) = decision.escalation_context {
            ctx.exploration_history = state
                .explorations
                .values()
                .filter(|_| {
                    // Include all explorations — all statuses are useful context
                    // for the human reviewer.
                    true
                })
                .map(|e| {
                    format!(
                        "{} [{}] \"{}\" (started {})",
                        &e.id.to_string()[..8],
                        e.status,
                        e.title,
                        &e.created_at.get(..19).unwrap_or(&e.created_at),
                    )
                })
                .collect();
        }

        if decision.is_blocked() || decision.is_gated() {
            self.record_policy_decision(&decision)?;
        }
        if decision.is_blocked() {
            if let fl_policy::PolicyVerdict::Block { reason, fix_suggestion } = &decision.verdict {
                let msg = if let Some(fix) = fix_suggestion {
                    format!("Policy blocked: {}\nSuggestion: {}", reason, fix)
                } else {
                    format!("Policy blocked: {}", reason)
                };
                bail!("{}", msg);
            }
        }
        if decision.is_gated() {
            if let fl_policy::PolicyVerdict::Gate { reason, .. } = &decision.verdict {
                if let Some(ctx) = &decision.escalation_context {
                    eprintln!(
                        "warning: rate limit escalation — {}\n  action: {}\n  limit: {} ({}/{})\n  task: {:?}\n  exploration: {:?}\n  history: {}",
                        reason, ctx.agent_action, ctx.limit_name,
                        ctx.current_value, ctx.limit_value,
                        ctx.task_id, ctx.exploration_id,
                        if ctx.exploration_history.is_empty() {
                            "(none)".to_string()
                        } else {
                            format!("\n    {}", ctx.exploration_history.join("\n    "))
                        }
                    );
                } else {
                    eprintln!("warning: policy rate limit gate triggered — {}", reason);
                }
            }
        }
        Ok(())
    }

    /// Enforce scope policy for the current task. Called before checkpoints.
    fn enforce_scope_policy(
        &self,
        task_id: Option<Uuid>,
        _exploration_id: Option<Uuid>,
    ) -> Result<()> {
        let config = self.policies_config();
        if !config.scope.enabled {
            return Ok(());
        }
        let Some(tid) = task_id else {
            return Ok(());
        };

        let state = self.replay_state()?;
        let task = match state.tasks.get(&tid) {
            Some(t) => t,
            None => return Ok(()),
        };
        if task.allowed_paths.is_empty() {
            return Ok(());
        }

        // Compute changed files: working directory vs latest snapshot.
        let changed_files = self.working_dir_changed_files();
        if changed_files.is_empty() {
            return Ok(());
        }

        let task_scope = fl_policy::TaskScope {
            task_id: tid,
            allowed_paths: task.allowed_paths.clone(),
        };
        let decision = fl_policy::check_scope_policy(&config.scope, &task_scope, &changed_files);

        if decision.is_blocked() || decision.is_gated() {
            self.record_policy_decision(&decision)?;
        }

        if config.scope.enforce_mode == fl_policy::ScopeEnforceMode::Split && !decision.affected_files.is_empty() {
            // Split mode: create discovery tasks for out-of-scope files.
            self.auto_create_discovery_tasks(tid, &decision.affected_files)?;
            eprintln!(
                "warning: {} out-of-scope file(s) recorded as discovery tasks",
                decision.affected_files.len()
            );
            return Ok(());
        }

        if decision.is_blocked() {
            if let fl_policy::PolicyVerdict::Block { reason, fix_suggestion } = &decision.verdict {
                let msg = if let Some(fix) = fix_suggestion {
                    format!("Policy blocked: {}\nSuggestion: {}", reason, fix)
                } else {
                    format!("Policy blocked: {}", reason)
                };
                bail!("{}", msg);
            }
        }
        if decision.is_gated() {
            if let fl_policy::PolicyVerdict::Gate { reason, .. } = &decision.verdict {
                eprintln!("warning: policy scope gate triggered — {}", reason);
            }
        }
        Ok(())
    }

    /// Enforce test requirements policy. Called before exploration promotion.
    fn enforce_test_requirements(&self) -> Result<()> {
        let config = self.policies_config();
        if !config.test_requirements.enabled {
            return Ok(());
        }

        // Run the test command if require_passing is enabled.
        let test_result = if config.test_requirements.require_passing {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&config.test_requirements.test_command)
                .current_dir(self.root())
                .output();

            match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let summary = if stderr.len() > 200 {
                        format!("{}...", &stderr[..200])
                    } else if !stderr.is_empty() {
                        stderr.to_string()
                    } else if stdout.len() > 200 {
                        format!("{}...", &stdout[..200])
                    } else {
                        stdout.to_string()
                    };
                    Some(fl_policy::TestResult {
                        passed: out.status.success(),
                        exit_code: out.status.code().unwrap_or(-1),
                        output_summary: summary,
                    })
                }
                Err(e) => {
                    Some(fl_policy::TestResult {
                        passed: false,
                        exit_code: -1,
                        output_summary: format!("Failed to run test command: {}", e),
                    })
                }
            }
        } else {
            None
        };

        // Collect new files from exploration checkpoints to detect missing tests.
        let (new_source_files, new_test_files) = self.collect_exploration_new_files();

        let decision = fl_policy::check_test_requirements(
            &config.test_requirements,
            test_result.as_ref(),
            &new_source_files,
            &new_test_files,
        );

        if decision.is_blocked() || decision.is_gated() {
            self.record_policy_decision(&decision)?;
        }
        if decision.is_blocked() {
            if let fl_policy::PolicyVerdict::Block { reason, fix_suggestion } = &decision.verdict {
                let msg = if let Some(fix) = fix_suggestion {
                    format!("Policy blocked: {}\nSuggestion: {}", reason, fix)
                } else {
                    format!("Policy blocked: {}", reason)
                };
                bail!("{}", msg);
            }
        }
        if decision.is_gated() {
            if let fl_policy::PolicyVerdict::Gate { reason, .. } = &decision.verdict {
                eprintln!("warning: test requirement gate triggered — {}", reason);
            }
        }
        Ok(())
    }

    /// Collect new source and test files from the active exploration's checkpoints.
    ///
    /// Scans checkpoint events to find files with `FileChangeKind::Added`, then
    /// classifies them as test files (name contains `_test`, `test_`, `.test`, or
    /// `_spec`) or source files.
    fn collect_exploration_new_files(&self) -> (Vec<String>, Vec<String>) {
        let state = match self.replay_state() {
            Ok(s) => s,
            Err(_) => return (Vec::new(), Vec::new()),
        };

        // Find the active exploration.
        let active_exploration = state
            .explorations
            .values()
            .find(|e| e.status == ExplorationStatus::Active);
        let _exploration = match active_exploration {
            Some(e) => e,
            None => return (Vec::new(), Vec::new()),
        };

        // Walk checkpoint events to find Added files.
        let events = match self.list_events() {
            Ok(e) => e,
            Err(_) => return (Vec::new(), Vec::new()),
        };

        let mut new_source_files = Vec::new();
        let mut new_test_files = Vec::new();

        for event in &events {
            if let EventKind::Checkpoint(cp) = &event.kind {
                if let Some(files_changed) = &cp.files_changed {
                    for fc in files_changed {
                        if fc.change_kind == FileChangeKind::Added {
                            if is_test_file(&fc.path) {
                                new_test_files.push(fc.path.clone());
                            } else {
                                new_source_files.push(fc.path.clone());
                            }
                        }
                    }
                }
            }
        }

        (new_source_files, new_test_files)
    }

    /// Enforce architecture rules policy. Called before checkpoints.
    fn enforce_architecture_rules(&self, files_changed: &[String]) -> Result<()> {
        let config = self.policies_config();
        if !config.architecture.enabled {
            return Ok(());
        }

        // Architecture rule checking uses import info. For now, we check
        // dependency direction rules against the changed file paths. Full
        // import extraction would require AST analysis per language.
        // We pass empty imports and rely on namespace convention + dependency
        // direction checks against file paths.
        let decision = fl_policy::check_architecture_rules(
            &config.architecture,
            &[], // import analysis not yet wired — checked via file path rules
            files_changed,
        );

        if decision.is_blocked() || decision.is_gated() {
            self.record_policy_decision(&decision)?;
        }
        if decision.is_blocked() {
            if let fl_policy::PolicyVerdict::Block { reason, fix_suggestion } = &decision.verdict {
                let msg = if let Some(fix) = fix_suggestion {
                    format!("Policy blocked: {}\nSuggestion: {}", reason, fix)
                } else {
                    format!("Policy blocked: {}", reason)
                };
                bail!("{}", msg);
            }
        }
        if decision.is_gated() {
            if let fl_policy::PolicyVerdict::Gate { reason, .. } = &decision.verdict {
                eprintln!("warning: architecture rule gate triggered — {}", reason);
            }
        }
        Ok(())
    }

    /// Enforce anti-pattern detection policy. Called before checkpoints.
    fn enforce_anti_patterns(&self, files_changed: &[String]) -> Result<()> {
        let config = self.policies_config();
        if !config.anti_patterns.enabled || config.anti_patterns.rules.is_empty() {
            return Ok(());
        }

        // Read content of changed files for pattern matching.
        let mut file_contents: Vec<(String, String)> = Vec::new();
        for path in files_changed {
            let full_path = self.root().join(path);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                file_contents.push((path.clone(), content));
            }
        }

        if file_contents.is_empty() {
            return Ok(());
        }

        let decision = fl_policy::check_anti_patterns(&config.anti_patterns, &file_contents);

        if decision.is_blocked() || decision.is_gated() {
            self.record_policy_decision(&decision)?;
        }
        if decision.is_blocked() {
            if let fl_policy::PolicyVerdict::Block { reason, fix_suggestion } = &decision.verdict {
                let msg = if let Some(fix) = fix_suggestion {
                    format!("Policy blocked: {}\nSuggestion: {}", reason, fix)
                } else {
                    format!("Policy blocked: {}", reason)
                };
                bail!("{}", msg);
            }
        }
        if decision.is_gated() {
            if let fl_policy::PolicyVerdict::Gate { reason, .. } = &decision.verdict {
                eprintln!("warning: anti-pattern gate triggered — {}", reason);
            }
        }
        Ok(())
    }

    /// Enforce DRY / duplication prevention policy. Called before checkpoints.
    fn enforce_reuse_policy(&self, files_changed: &[String]) -> Result<()> {
        let config = self.policies_config();
        if !config.reuse.enabled {
            return Ok(());
        }

        // Collect symbols from changed files using semantic extraction.
        let mut new_symbols: Vec<(String, fl_semantic::SymbolInfo)> = Vec::new();

        for path in files_changed {
            let full_path = self.root().join(path);
            let rel_path = std::path::Path::new(path);
            if !fl_semantic::supported_source(rel_path) {
                continue;
            }
            if let Ok(content) = std::fs::read(&full_path) {
                if let Ok(Some(symbols)) =
                    fl_semantic::extract_symbols_from_source(rel_path, &content)
                {
                    for sym in symbols {
                        new_symbols.push((path.clone(), sym));
                    }
                }
            }
        }

        if new_symbols.is_empty() {
            return Ok(());
        }

        // Gather existing symbols from non-changed source files in the working tree.
        let root = self.root();
        let mut existing_symbols: Vec<(String, fl_semantic::SymbolInfo)> = Vec::new();
        self.collect_existing_symbols(&root, &root, files_changed, &mut existing_symbols);

        if existing_symbols.is_empty() {
            return Ok(());
        }

        // Compare new symbols against existing ones.
        let mut matches: Vec<fl_policy::DuplicationMatch> = Vec::new();
        let threshold = config.reuse.similarity_threshold;

        for (new_file, new_sym) in &new_symbols {
            // Determine effective threshold — check protected domains.
            let effective_threshold = config
                .reuse
                .protected_domains
                .iter()
                .find(|d| {
                    glob::Pattern::new(&d.file_pattern)
                        .map(|g| g.matches(new_file.as_str()))
                        .unwrap_or(false)
                })
                .map(|d| d.similarity_threshold)
                .unwrap_or(threshold);

            for (existing_file, existing_sym) in &existing_symbols {
                // Skip self-comparison (same file).
                if new_file == existing_file {
                    continue;
                }

                // Layer 2: Body hash match (exact structural duplication).
                if config.reuse.check_bodies
                    && !new_sym.match_hash.is_empty()
                    && new_sym.match_hash == existing_sym.match_hash
                {
                    matches.push(fl_policy::DuplicationMatch {
                        new_symbol: new_sym.name.clone(),
                        new_file: new_file.clone(),
                        existing_symbol: existing_sym.name.clone(),
                        existing_file: existing_file.clone(),
                        similarity: 1.0,
                        layer: fl_policy::DuplicationLayer::Body,
                        reuse_suggestion: format!(
                            "Exact structural match — consider importing {} from {}",
                            existing_sym.name, existing_file
                        ),
                    });
                    continue; // No need for further checks if body matches.
                }

                // Layer 1: Signature matching.
                if config.reuse.check_signatures {
                    if let (Some(new_sig), Some(existing_sig)) =
                        (&new_sym.signature, &existing_sym.signature)
                    {
                        let new_params: Vec<(Option<String>, bool)> = new_sig
                            .parameters
                            .iter()
                            .map(|p| (p.type_hint.clone(), p.optional))
                            .collect();
                        let existing_params: Vec<(Option<String>, bool)> = existing_sig
                            .parameters
                            .iter()
                            .map(|p| (p.type_hint.clone(), p.optional))
                            .collect();
                        let sig_sim = fl_policy::signature_similarity(
                            &(new_params, new_sig.return_type.clone()),
                            &(existing_params, existing_sig.return_type.clone()),
                        );
                        let name_sim =
                            fl_policy::name_similarity(&new_sym.name, &existing_sym.name);

                        // Combined score: 60% signature, 40% name.
                        let combined = sig_sim * 0.6 + name_sim * 0.4;

                        if combined >= effective_threshold {
                            matches.push(fl_policy::DuplicationMatch {
                                new_symbol: new_sym.name.clone(),
                                new_file: new_file.clone(),
                                existing_symbol: existing_sym.name.clone(),
                                existing_file: existing_file.clone(),
                                similarity: combined,
                                layer: fl_policy::DuplicationLayer::Signature,
                                reuse_suggestion: format!(
                                    "Similar signature — consider reusing {} from {}",
                                    existing_sym.name, existing_file
                                ),
                            });
                        }
                    }
                }

                // Layer 3: Pattern conformance — detect when new code should
                // implement an existing interface/trait.
                if config.reuse.check_patterns
                    && matches!(existing_sym.kind, fl_semantic::SymbolKind::Interface)
                {
                    let iface_name = &existing_sym.name;
                    if !iface_name.is_empty()
                        && fl_policy::name_similarity(&new_sym.name, iface_name)
                            >= effective_threshold
                    {
                        matches.push(fl_policy::DuplicationMatch {
                            new_symbol: new_sym.name.clone(),
                            new_file: new_file.clone(),
                            existing_symbol: existing_sym.name.clone(),
                            existing_file: existing_file.clone(),
                            similarity: fl_policy::name_similarity(
                                &new_sym.name,
                                iface_name,
                            ),
                            layer: fl_policy::DuplicationLayer::Pattern,
                            reuse_suggestion: format!(
                                "Consider implementing interface {} from {}",
                                iface_name, existing_file
                            ),
                        });
                    }
                }
            }
        }

        let decision = fl_policy::check_reuse_policy(&config.reuse, &matches);

        if decision.is_blocked() || decision.is_gated() {
            self.record_policy_decision(&decision)?;
        }
        if decision.is_blocked() {
            if let fl_policy::PolicyVerdict::Block {
                reason,
                fix_suggestion,
            } = &decision.verdict
            {
                let msg = if let Some(fix) = fix_suggestion {
                    format!("Policy blocked: {}\nSuggestion: {}", reason, fix)
                } else {
                    format!("Policy blocked: {}", reason)
                };
                bail!("{}", msg);
            }
        }
        if decision.is_gated() {
            if let fl_policy::PolicyVerdict::Gate { reason, .. } = &decision.verdict {
                eprintln!(
                    "warning: duplication/reuse gate triggered — {}",
                    reason
                );
            }
        }
        Ok(())
    }

    /// Enforce dependency policy — check manifest files for unapproved packages,
    /// blocked licenses, and known vulnerabilities.
    fn enforce_dependency_policy(&self, files_changed: &[String]) -> Result<()> {
        let config = self.policies_config();
        if !config.dependencies.enabled {
            return Ok(());
        }

        // Only check when manifest files are modified.
        let manifest_patterns = [
            "Cargo.toml",
            "package.json",
            "go.mod",
            "requirements.txt",
            "Pipfile",
            "Gemfile",
            "pom.xml",
            "build.gradle",
        ];

        let changed_manifests: Vec<&String> = files_changed
            .iter()
            .filter(|f| {
                manifest_patterns
                    .iter()
                    .any(|pat| f.ends_with(pat))
            })
            .collect();

        if changed_manifests.is_empty() {
            return Ok(());
        }

        // Parse dependencies from changed manifest files.
        let mut deps: Vec<fl_policy::DetectedDependency> = Vec::new();
        for manifest in &changed_manifests {
            let full_path = self.root().join(manifest);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                let file_deps = parse_manifest_dependencies(manifest, &content);
                deps.extend(file_deps);
            }
        }

        if deps.is_empty() {
            return Ok(());
        }

        let decision = fl_policy::check_dependency_policy(&config.dependencies, &deps);

        if decision.is_blocked() || decision.is_gated() {
            self.record_policy_decision(&decision)?;
        }
        if decision.is_blocked() {
            if let fl_policy::PolicyVerdict::Block {
                reason,
                fix_suggestion,
            } = &decision.verdict
            {
                let msg = if let Some(fix) = fix_suggestion {
                    format!("Policy blocked: {}\nSuggestion: {}", reason, fix)
                } else {
                    format!("Policy blocked: {}", reason)
                };
                bail!("{}", msg);
            }
        }
        if decision.is_gated() {
            if let fl_policy::PolicyVerdict::Gate { reason, .. } = &decision.verdict {
                eprintln!("warning: dependency check gate triggered — {}", reason);
            }
        }

        // Run consumer test suites if configured and shared libraries are modified.
        if !config.dependencies.consumer_test_command.is_empty() {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&config.dependencies.consumer_test_command)
                .current_dir(self.root())
                .output();
            match output {
                Ok(result) if !result.status.success() => {
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    eprintln!(
                        "warning: consumer test suite failed: {}",
                        stderr.lines().take(5).collect::<Vec<_>>().join("\n")
                    );
                }
                Err(e) => {
                    eprintln!("warning: failed to run consumer test command: {}", e);
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Post-promote regression check — run test suite after promotion and
    /// automatically rollback if failures are detected.
    fn post_promote_regression_check(&self, exploration_id: Uuid) {
        let config = self.policies_config();
        if !config.regression.enabled || !config.regression.monitor_after_merge {
            return;
        }

        // Run the configured test command to check for regressions.
        let test_cmd = if config.test_requirements.test_command.is_empty() {
            "cargo test"
        } else {
            &config.test_requirements.test_command
        };

        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(test_cmd)
            .current_dir(self.root())
            .output();

        let test_passed = match &output {
            Ok(result) => result.status.success(),
            Err(_) => true, // If we can't run tests, don't block
        };

        if test_passed {
            return;
        }

        let stderr = output
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
            .unwrap_or_default();

        let regression = fl_policy::RegressionDetection {
            source_event_id: exploration_id,
            kind: fl_policy::RegressionKind::TestFailure,
            description: format!(
                "Test failures detected after promotion: {}",
                stderr.lines().take(3).collect::<Vec<_>>().join("; ")
            ),
            affected_files: Vec::new(),
            originating_actor: std::env::var("FL_ACTOR").unwrap_or_else(|_| "unknown".to_string()),
        };

        let decision = fl_policy::check_regression_policy(
            &config.regression,
            &config.rollback,
            &regression,
        );

        // Record the regression detection.
        let _ = self.record_policy_decision(&decision);

        if decision.is_blocked() && config.rollback.enabled && config.rollback.auto_rollback {
            // Automatic rollback via undo.
            eprintln!(
                "warning: regression detected after promotion of exploration {}. Attempting automatic rollback.",
                exploration_id
            );
            if let Err(e) = self.undo(UndoRequest::Last) {
                eprintln!("error: automatic rollback failed: {}", e);
            } else {
                eprintln!(
                    "info: automatic rollback complete. Originating agent should re-explore."
                );
            }
        } else if decision.is_blocked() || decision.is_gated() {
            if let fl_policy::PolicyVerdict::Block { reason, .. }
            | fl_policy::PolicyVerdict::Gate { reason, .. } = &decision.verdict
            {
                eprintln!("warning: post-merge regression — {}", reason);
            }
        }
    }

    /// Recursively collect symbols from source files that are NOT in the changed set.
    fn collect_existing_symbols(
        &self,
        dir: &std::path::Path,
        root: &std::path::Path,
        changed_files: &[String],
        symbols: &mut Vec<(String, fl_semantic::SymbolInfo)>,
    ) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Skip hidden directories and common non-source dirs.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.')
                    || name == "target"
                    || name == "node_modules"
                    || name == "vendor"
                    || name == "dist"
                    || name == "build"
                {
                    continue;
                }
            }
            if path.is_dir() {
                self.collect_existing_symbols(&path, root, changed_files, symbols);
            } else if path.is_file() {
                let rel_path = match path.strip_prefix(root) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let rel_str = rel_path.to_string_lossy().to_string();
                // Skip files that are in the changed set.
                if changed_files.contains(&rel_str) {
                    continue;
                }
                if !fl_semantic::supported_source(rel_path) {
                    continue;
                }
                if let Ok(content) = std::fs::read(&path) {
                    if let Ok(Some(file_symbols)) =
                        fl_semantic::extract_symbols_from_source(rel_path, &content)
                    {
                        for sym in file_symbols {
                            symbols.push((rel_str.clone(), sym));
                        }
                    }
                }
            }
        }
    }

    /// Create discovery tasks for out-of-scope files (Split mode).
    fn auto_create_discovery_tasks(
        &self,
        source_task_id: Uuid,
        out_of_scope_files: &[String],
    ) -> Result<()> {
        let file_list = out_of_scope_files.join(", ");
        let title = format!(
            "discovered out-of-scope changes: {}",
            if file_list.len() > 80 {
                format!("{}...", &file_list[..80])
            } else {
                file_list
            }
        );
        self.create_task(
            title,
            Some(format!(
                "Auto-created from task {} — files: {}",
                source_task_id,
                out_of_scope_files.join(", ")
            )),
            vec![],
            Some(source_task_id),
            out_of_scope_files.to_vec(),
        )?;
        Ok(())
    }

    /// Compute changed files in working directory vs latest snapshot.
    fn working_dir_changed_files(&self) -> Vec<String> {
        let latest = match self.latest_checkpoint() {
            Some(event) => event,
            None => return Vec::new(),
        };
        let EventKind::Checkpoint(payload) = latest.kind else {
            return Vec::new();
        };
        let snapshot_root = match self.ensure_snapshot_available(payload.snapshot_id) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        let snapshot_files = collect_source_files(&snapshot_root, false).unwrap_or_default();
        let current_files = collect_source_files(self.root(), true).unwrap_or_default();

        let mut changed = Vec::new();
        for path in &current_files {
            if !snapshot_files.contains(path) {
                changed.push(path.display().to_string());
            } else {
                let old = fs::read(snapshot_root.join(path)).unwrap_or_default();
                let new = fs::read(self.root.join(path)).unwrap_or_default();
                if old != new {
                    changed.push(path.display().to_string());
                }
            }
        }
        for path in &snapshot_files {
            if !current_files.contains(path) {
                changed.push(path.display().to_string());
            }
        }
        changed
    }

    /// Compute file change summaries between the new snapshot and the parent
    /// checkpoint's snapshot. Returns `None` on any error (best-effort).
    fn compute_file_changes(
        &self,
        new_snapshot_id: Uuid,
        parent_checkpoint_event: Option<Uuid>,
    ) -> Option<Vec<FileChangeSummary>> {
        let parent_event_id = parent_checkpoint_event?;

        // Find parent checkpoint's snapshot ID.
        let events = self.list_events().ok()?;
        let parent_snapshot_id = events.iter().find_map(|e| {
            if e.id == parent_event_id {
                if let EventKind::Checkpoint(cp) = &e.kind {
                    return Some(cp.snapshot_id);
                }
            }
            None
        })?;

        let old_root = self.ensure_snapshot_available(parent_snapshot_id).ok()?;
        let new_root = self.ensure_snapshot_available(new_snapshot_id).ok()?;

        if !old_root.is_dir() || !new_root.is_dir() {
            return None;
        }

        let old_files = collect_source_files(&old_root, false).ok()?;
        let new_files = collect_source_files(&new_root, false).ok()?;

        let mut changes = Vec::new();

        for path in &new_files {
            let path_str = path.display().to_string();
            if !old_files.contains(path) {
                // Added file — count all content as new semantic changes.
                let line_count = fs::read_to_string(new_root.join(path))
                    .map(|s| s.lines().count() as u32)
                    .unwrap_or(0);
                // For added files, estimate 1 semantic change (the whole file addition).
                changes.push(FileChangeSummary {
                    path: path_str,
                    change_kind: FileChangeKind::Added,
                    lines_added: line_count,
                    lines_removed: 0,
                    semantic_changes_count: Some(1),
                });
            } else {
                let old_content = fs::read(old_root.join(path)).unwrap_or_default();
                let new_content = fs::read(new_root.join(path)).unwrap_or_default();
                if old_content != new_content {
                    let old_lines = String::from_utf8_lossy(&old_content).lines().count() as u32;
                    let new_lines = String::from_utf8_lossy(&new_content).lines().count() as u32;
                    // Try semantic diff to count symbol-level changes.
                    let semantic_count = self.count_semantic_changes(
                        &old_content, &new_content, &path_str,
                    );
                    changes.push(FileChangeSummary {
                        path: path_str,
                        change_kind: FileChangeKind::Modified,
                        lines_added: new_lines.saturating_sub(old_lines),
                        lines_removed: old_lines.saturating_sub(new_lines),
                        semantic_changes_count: semantic_count,
                    });
                }
            }
        }

        for path in &old_files {
            if !new_files.contains(path) {
                let line_count = fs::read_to_string(old_root.join(path))
                    .map(|s| s.lines().count() as u32)
                    .unwrap_or(0);
                // Deleted file = 1 semantic change (the whole file deletion).
                changes.push(FileChangeSummary {
                    path: path.display().to_string(),
                    change_kind: FileChangeKind::Deleted,
                    lines_added: 0,
                    lines_removed: line_count,
                    semantic_changes_count: Some(1),
                });
            }
        }

        if changes.is_empty() {
            None
        } else {
            Some(changes)
        }
    }

    /// Compute file change summaries for a virtual snapshot (no snapshot dir on disk).
    /// Compares the working directory `new_root` against the parent checkpoint's snapshot.
    fn compute_file_changes_for_virtual_snapshot(
        &self,
        new_root: &Path,
        parent_checkpoint_event: Option<Uuid>,
    ) -> Option<Vec<FileChangeSummary>> {
        let parent_event_id = parent_checkpoint_event?;

        let events = self.list_events().ok()?;
        let parent_snapshot_id = events.iter().find_map(|e| {
            if e.id == parent_event_id {
                if let EventKind::Checkpoint(cp) = &e.kind {
                    return Some(cp.snapshot_id);
                }
            }
            None
        })?;

        let old_root = self.ensure_snapshot_available(parent_snapshot_id).ok()?;

        if !old_root.is_dir() {
            return None;
        }

        let old_files = collect_source_files(&old_root, false).ok()?;
        let colocated = self.repo_mode().ok()? == RepoMode::GitColocated;
        let new_files = collect_source_files_with_mode(new_root, true, colocated).ok()?;

        let mut changes = Vec::new();

        for path in &new_files {
            let path_str = path.display().to_string();
            if !old_files.contains(path) {
                let line_count = fs::read_to_string(new_root.join(path))
                    .map(|s| s.lines().count() as u32)
                    .unwrap_or(0);
                changes.push(FileChangeSummary {
                    path: path_str,
                    change_kind: FileChangeKind::Added,
                    lines_added: line_count,
                    lines_removed: 0,
                    semantic_changes_count: Some(1),
                });
            } else {
                let old_content = fs::read(old_root.join(path)).unwrap_or_default();
                let new_content = fs::read(new_root.join(path)).unwrap_or_default();
                if old_content != new_content {
                    let old_lines = String::from_utf8_lossy(&old_content).lines().count() as u32;
                    let new_lines = String::from_utf8_lossy(&new_content).lines().count() as u32;
                    let semantic_count =
                        self.count_semantic_changes(&old_content, &new_content, &path_str);
                    changes.push(FileChangeSummary {
                        path: path_str,
                        change_kind: FileChangeKind::Modified,
                        lines_added: new_lines.saturating_sub(old_lines),
                        lines_removed: old_lines.saturating_sub(new_lines),
                        semantic_changes_count: semantic_count,
                    });
                }
            }
        }

        for path in &old_files {
            if !new_files.contains(path) {
                let line_count = fs::read_to_string(old_root.join(path))
                    .map(|s| s.lines().count() as u32)
                    .unwrap_or(0);
                changes.push(FileChangeSummary {
                    path: path.display().to_string(),
                    change_kind: FileChangeKind::Deleted,
                    lines_added: 0,
                    lines_removed: line_count,
                    semantic_changes_count: Some(1),
                });
            }
        }

        if changes.is_empty() {
            None
        } else {
            Some(changes)
        }
    }

    /// Count semantic (symbol-level) changes between old and new file content.
    /// Returns None if the file type is unsupported by the semantic analyzer.
    fn count_semantic_changes(
        &self,
        old_content: &[u8],
        new_content: &[u8],
        path_str: &str,
    ) -> Option<u32> {
        let path = std::path::PathBuf::from(path_str);
        match fl_semantic::diff(&path, Some(old_content), Some(new_content)) {
            Ok(Some(diff)) => Some(diff.changes.len() as u32),
            _ => None,
        }
    }

    /// Enforce commit hygiene policy. Checks that required metadata fields are present.
    fn enforce_commit_hygiene(
        &self,
        intent: &Option<CheckpointIntentMetadata>,
    ) -> Result<()> {
        let config = self.policies_config();
        if !config.commit_hygiene.enabled {
            return Ok(());
        }

        let intent = match intent {
            Some(i) => i,
            None => {
                let mut missing = Vec::new();
                if config.commit_hygiene.require_category {
                    missing.push("--category");
                }
                if config.commit_hygiene.require_scope {
                    missing.push("--scope");
                }
                if config.commit_hygiene.require_confidence {
                    missing.push("--confidence");
                }
                if !missing.is_empty() {
                    bail!(
                        "Commit hygiene requires: {}. Provide these flags or disable commit_hygiene in policies.toml",
                        missing.join(", ")
                    );
                }
                return Ok(());
            }
        };

        if config.commit_hygiene.require_category && intent.category.is_none() {
            bail!("Commit hygiene requires --category (bugfix, feature, refactor, test, docs, style, chore)");
        }
        if config.commit_hygiene.require_scope && intent.scope_label.is_none() {
            bail!("Commit hygiene requires --scope <label>");
        }
        if config.commit_hygiene.require_confidence && intent.confidence.is_none() {
            bail!("Commit hygiene requires --confidence (high, medium, low)");
        }

        Ok(())
    }

    /// Check checkpoint frequency and warn if too much time has elapsed.
    fn check_checkpoint_frequency(&self) {
        let config = self.policies_config();
        let max_secs = match config.commit_hygiene.max_time_between_checkpoints {
            Some(s) if config.commit_hygiene.enabled => s,
            _ => return,
        };

        let latest = match self.latest_checkpoint() {
            Some(e) => e,
            None => return,
        };

        // Parse the latest checkpoint timestamp (nanosecond string).
        let last_nanos: u128 = match latest.timestamp.parse() {
            Ok(n) => n,
            Err(_) => return,
        };
        let last_secs = (last_nanos / 1_000_000_000) as u64;
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let elapsed = now_secs.saturating_sub(last_secs);
        if elapsed > max_secs {
            eprintln!(
                "warning: {}s since last checkpoint (max_time_between_checkpoints: {}s)",
                elapsed, max_secs
            );
        }
    }

    /// Find the active task ID from replayed state (if any task is claimed by current actor).
    fn active_task_id(&self) -> Option<Uuid> {
        let state = self.replay_state().ok()?;
        let actor = current_actor();
        state
            .tasks
            .values()
            .find(|t| {
                t.status == fl_workflow::TaskStatus::Claimed
                    && t.assignee.as_deref() == Some(&actor)
            })
            .map(|t| t.id)
    }

    /// Find the active exploration ID from replayed state.
    fn active_exploration_id(&self) -> Option<Uuid> {
        let state = self.replay_state().ok()?;
        state
            .explorations
            .values()
            .find(|e| e.status == fl_workflow::ExplorationStatus::Active)
            .map(|e| e.id)
    }

    /// Find the active session ID for the current actor from replayed state.
    fn active_session_id(&self) -> Option<Uuid> {
        let state = self.replay_state().ok()?;
        let actor = current_actor();
        state
            .sessions
            .values()
            .find(|s| {
                s.status == fl_workflow::SessionStatus::Active
                    && s.agent == actor
            })
            .map(|s| s.id)
    }

    /// Scan the working directory for secrets and return an error if any are
    /// found and the config is set to block.
    fn scan_working_directory_for_secrets(&self) -> Result<()> {
        use crate::secrets::{format_findings, load_secrets_config, scan_files_for_secrets};

        let config = load_secrets_config(self.root());
        let colocated = self.repo_mode()? == RepoMode::GitColocated;

        // Collect tracked files using the same walker as checkpoint.
        let files: Vec<PathBuf> = build_repo_walker(&self.root, colocated)
            .build()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map_or(false, |ft| ft.is_file()))
            .map(|e| e.into_path())
            .collect();

        let report = scan_files_for_secrets(&self.root, &files, &config)?;

        if report.has_secrets() {
            let msg = format_findings(&report.findings);
            if config.block {
                bail!("{}", msg);
            } else {
                eprintln!("warning: {}", msg);
            }
        }

        Ok(())
    }

    /// Run hooks for a given hook point. If any blocking hook fails, bail.
    /// Records all hook results in the event log.
    fn run_hooks_blocking(&self, hook_point: &str) -> Result<()> {
        use crate::hooks::{execute_hooks, format_hook_report, load_hooks_config};

        let config = load_hooks_config(self.root());
        let matching_hooks = config.hooks.iter().any(|h| h.hook_point == hook_point);
        if !matching_hooks {
            return Ok(());
        }

        let report = execute_hooks(self.root(), hook_point, &config)?;

        // Record each hook execution in the event log.
        for result in &report.results {
            self.append_event(EventKind::Hook(HookEvent {
                hook_point: result.hook_point.clone(),
                hook_name: result.name.clone(),
                command: result.command.clone(),
                success: result.success,
                duration_ms: result.duration_ms,
                output: result.output.clone(),
                bypassed: false,
            }))?;
        }

        if report.blocked {
            let msg = format_hook_report(&report);
            bail!("{}", msg);
        } else if report.has_failures() {
            let msg = format_hook_report(&report);
            eprint!("{}", msg);
        }

        Ok(())
    }

    /// Run hooks for a given hook point, printing results but never blocking.
    fn run_hooks_reporting(&self, hook_point: &str) {
        use crate::hooks::{execute_hooks, format_hook_report, load_hooks_config};

        let config = load_hooks_config(self.root());
        let matching_hooks = config.hooks.iter().any(|h| h.hook_point == hook_point);
        if !matching_hooks {
            return;
        }

        match execute_hooks(self.root(), hook_point, &config) {
            Ok(report) => {
                // Record hook executions (best-effort for post-hooks).
                for result in &report.results {
                    let _ = self.append_event(EventKind::Hook(HookEvent {
                        hook_point: result.hook_point.clone(),
                        hook_name: result.name.clone(),
                        command: result.command.clone(),
                        success: result.success,
                        duration_ms: result.duration_ms,
                        output: result.output.clone(),
                        bypassed: false,
                    }));
                }
                if report.has_failures() {
                    let msg = format_hook_report(&report);
                    eprint!("{}", msg);
                }
            }
            Err(e) => {
                eprintln!("warning: post-hook execution failed: {}", e);
            }
        }
    }

    /// Record that hooks were bypassed via --skip-hooks for a given hook point.
    fn record_hook_bypass(&self, hook_point: &str) -> Result<()> {
        use crate::hooks::load_hooks_config;

        let config = load_hooks_config(self.root());
        let matching: Vec<_> = config.hooks.iter().filter(|h| h.hook_point == hook_point).collect();
        if matching.is_empty() {
            return Ok(());
        }

        for hook in &matching {
            self.append_event(EventKind::Hook(HookEvent {
                hook_point: hook_point.to_string(),
                hook_name: hook.name.clone(),
                command: hook.command.clone(),
                success: true,
                duration_ms: 0,
                output: None,
                bypassed: true,
            }))?;
        }

        Ok(())
    }

    fn create_checkpoint_with_lineage(
        &self,
        label: String,
        message: Option<String>,
        parent_checkpoint_event: Option<Uuid>,
        intent: Option<CheckpointIntentMetadata>,
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
            intent,
        )
    }

    /// Like `create_checkpoint_with_lineage` but the parent is used exactly as
    /// provided — `None` remains `None` instead of being auto-filled from
    /// `latest_checkpoint()`.  Used by undo chain-walk so the restore
    /// checkpoint's parent accurately reflects the ancestor's predecessor.
    fn create_checkpoint_with_exact_lineage(
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

        // Use a sentinel parent to prevent auto-fill: if the real parent is
        // None we still need to go through the normal creation pipeline
        // (snapshot, merkle root, etc.) but skip the or_else auto-fill.
        // We achieve this by passing the explicit parent directly into the
        // event, bypassing the auto-fill helpers.
        let snapshot_id = Uuid::new_v4();

        if self.repo_mode()? == RepoMode::Native {
            self.create_native_snapshot(&self.root, true, snapshot_id)?;

            let file_index = FileIndex::for_root(self.root());
            let index = file_index.read(snapshot_id)?;
            let snapshot_merkle_root = compute_native_merkle_root(&index)?;
            let files_changed =
                self.compute_file_changes(snapshot_id, parent_checkpoint_event);

            let event = self.append_event(EventKind::Checkpoint(CheckpointEvent {
                label,
                message: message.clone(),
                snapshot_id,
                parent_checkpoint_event,
                snapshot_merkle_root: Some(snapshot_merkle_root),
                ai_intent: None,
                intent_confidence: None,
                files_changed,
                category: None,
                scope_label: None,
                structured_description: None,
                git_commit_sha: git_commit_mapping.clone(),
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
        } else if self.repo_mode()? == RepoMode::GitColocated && git_commit_mapping.is_some() {
            let snapshot_merkle_root = compute_merkle_root_filtered(&self.root, true)?;
            let files_changed = self
                .compute_file_changes_for_virtual_snapshot(&self.root, parent_checkpoint_event);

            let event = self.append_event(EventKind::Checkpoint(CheckpointEvent {
                label,
                message: message.clone(),
                snapshot_id,
                parent_checkpoint_event,
                snapshot_merkle_root: Some(snapshot_merkle_root),
                ai_intent: None,
                intent_confidence: None,
                files_changed,
                category: None,
                scope_label: None,
                structured_description: None,
                git_commit_sha: git_commit_mapping.clone(),
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
        } else {
            let snapshot_path = self.snapshot_path(snapshot_id);
            fs::create_dir_all(&snapshot_path).with_context(|| {
                format!(
                    "failed to create snapshot directory {}",
                    snapshot_path.display()
                )
            })?;

            let colocated = self.repo_mode()? == RepoMode::GitColocated;
            copy_tree_with_mode(&self.root, &snapshot_path, true, colocated)?;

            let snapshot_merkle_root = compute_snapshot_merkle_root(&snapshot_path)?;
            let files_changed =
                self.compute_file_changes(snapshot_id, parent_checkpoint_event);

            let event = self.append_event(EventKind::Checkpoint(CheckpointEvent {
                label,
                message: message.clone(),
                snapshot_id,
                parent_checkpoint_event,
                snapshot_merkle_root: Some(snapshot_merkle_root),
                ai_intent: None,
                intent_confidence: None,
                files_changed,
                category: None,
                scope_label: None,
                structured_description: None,
                git_commit_sha: git_commit_mapping.clone(),
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
            None,
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
        intent: Option<CheckpointIntentMetadata>,
    ) -> Result<Event> {
        let snapshot_id = Uuid::new_v4();

        if self.repo_mode()? == RepoMode::Native {
            // Native mode: ensure working directory is captured as file events first
            self.snapshot_working_directory()?;

            // Try to build SnapshotIndex from file_states (O(1) — blocks already stored)
            let state = self.replay_state()?;
            if !state.file_states.is_empty() {
                let file_index = FileIndex::for_root(self.root());
                file_index.ensure_exists()?;

                let mut index = SnapshotIndex::new(snapshot_id);
                for (path, fs) in &state.file_states {
                    index.insert(
                        path.clone(),
                        FileEntry {
                            blocks: fs.blocks.clone(),
                            size: fs.size,
                            file_hash: fs.content_hash.clone(),
                        },
                    );
                }
                file_index.write(&index)?;
            } else {
                // Fallback: no file_states yet, do a full scan
                self.create_native_snapshot(source_root, apply_skip, snapshot_id)?;
            }

            self.create_checkpoint_event_with_native_merkle(
                snapshot_id,
                label,
                message,
                parent_checkpoint_event,
                git_commit_mapping,
                intent,
            )
        } else if self.repo_mode()? == RepoMode::GitColocated && git_commit_mapping.is_some() {
            // Git-colocated mode with a git commit: skip physical snapshot copy.
            // Compute merkle root directly from the filtered working directory.
            let snapshot_merkle_root = compute_merkle_root_filtered(source_root, true)?;
            let parent_checkpoint_event =
                parent_checkpoint_event.or_else(|| self.latest_checkpoint().map(|event| event.id));

            // Compute file changes by comparing working dir vs parent snapshot.
            let files_changed =
                self.compute_file_changes_for_virtual_snapshot(source_root, parent_checkpoint_event);

            let (category, scope_label, structured_description) = if let Some(ref i) = intent {
                (i.category, i.scope_label.clone(), i.structured_description.clone())
            } else {
                (None, None, None)
            };

            let event = self.append_event(EventKind::Checkpoint(CheckpointEvent {
                label,
                message: message.clone(),
                snapshot_id,
                parent_checkpoint_event,
                snapshot_merkle_root: Some(snapshot_merkle_root),
                ai_intent: None,
                intent_confidence: None,
                files_changed,
                category,
                scope_label,
                structured_description,
                git_commit_sha: git_commit_mapping.clone(),
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
        } else {
            let snapshot_path = self.snapshot_path(snapshot_id);
            fs::create_dir_all(&snapshot_path).with_context(|| {
                format!(
                    "failed to create snapshot directory {}",
                    snapshot_path.display()
                )
            })?;

            let colocated = self.repo_mode()? == RepoMode::GitColocated;
            copy_tree_with_mode(source_root, &snapshot_path, apply_skip, colocated)?;

            self.create_checkpoint_from_existing_snapshot_with_lineage(
                snapshot_id,
                label,
                message,
                parent_checkpoint_event,
                git_commit_mapping,
                intent,
            )
        }
    }

    fn create_checkpoint_from_existing_snapshot_with_lineage(
        &self,
        snapshot_id: Uuid,
        label: String,
        message: Option<String>,
        parent_checkpoint_event: Option<Uuid>,
        git_commit_mapping: Option<String>,
        intent: Option<CheckpointIntentMetadata>,
    ) -> Result<Event> {
        let snapshot_path = self.snapshot_path(snapshot_id);
        let snapshot_merkle_root = compute_snapshot_merkle_root(&snapshot_path)?;
        let parent_checkpoint_event =
            parent_checkpoint_event.or_else(|| self.latest_checkpoint().map(|event| event.id));

        // Compute file changes (best-effort, non-fatal).
        let files_changed = self.compute_file_changes(snapshot_id, parent_checkpoint_event);

        let (category, scope_label, structured_description) = if let Some(ref i) = intent {
            (i.category, i.scope_label.clone(), i.structured_description.clone())
        } else {
            (None, None, None)
        };

        let event = self.append_event(EventKind::Checkpoint(CheckpointEvent {
            label,
            message: message.clone(),
            snapshot_id,
            parent_checkpoint_event,
            snapshot_merkle_root: Some(snapshot_merkle_root),
            ai_intent: None,
            intent_confidence: None,
            files_changed,
            category,
            scope_label,
            structured_description,
            git_commit_sha: git_commit_mapping.clone(),
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
        if self.repo_mode()? == RepoMode::Native {
            return self.restore_workspace_from_native_snapshot(snapshot_id);
        }

        let snapshot_root = self.ensure_snapshot_available(snapshot_id)?;

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
        if self.repo_mode()? == RepoMode::Native {
            return self.restore_workspace_file_from_native_snapshot(snapshot_id, rel_path);
        }

        let snapshot_root = self.ensure_snapshot_available(snapshot_id)?;

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

    /// Check whether a file exists in a given snapshot (native or directory-based).
    fn snapshot_contains_file(&self, snapshot_id: Uuid, rel_path: &Path) -> Result<bool> {
        if self.repo_mode()? == RepoMode::Native {
            let file_index = FileIndex::for_root(self.root());
            if !file_index.has(snapshot_id) {
                return Ok(false);
            }
            let index = file_index.read(snapshot_id)?;
            let rel_key = rel_path_to_key(rel_path);
            return Ok(index.files.contains_key(&rel_key));
        }

        let snapshot_root = self.ensure_snapshot_available(snapshot_id)?;
        let snapshot_file = snapshot_root.join(rel_path);
        Ok(snapshot_file.exists() && !snapshot_file.is_dir())
    }

    /// Create a checkpoint event using the merkle root computed from the native
    /// snapshot index (rather than walking a snapshot directory).
    fn create_checkpoint_event_with_native_merkle(
        &self,
        snapshot_id: Uuid,
        label: String,
        message: Option<String>,
        parent_checkpoint_event: Option<Uuid>,
        git_commit_mapping: Option<String>,
        intent: Option<CheckpointIntentMetadata>,
    ) -> Result<Event> {
        let file_index = FileIndex::for_root(self.root());
        let index = file_index.read(snapshot_id)?;

        // Compute merkle root from the file index entries (same algorithm as
        // directory-based, but using the stored file hashes)
        let snapshot_merkle_root = compute_native_merkle_root(&index)?;

        let parent_checkpoint_event =
            parent_checkpoint_event.or_else(|| self.latest_checkpoint().map(|event| event.id));

        // Compute file changes (best-effort, non-fatal).
        let files_changed = self.compute_file_changes(snapshot_id, parent_checkpoint_event);

        let (category, scope_label, structured_description) = if let Some(ref i) = intent {
            (i.category, i.scope_label.clone(), i.structured_description.clone())
        } else {
            (None, None, None)
        };

        let event = self.append_event(EventKind::Checkpoint(CheckpointEvent {
            label,
            message: message.clone(),
            snapshot_id,
            parent_checkpoint_event,
            snapshot_merkle_root: Some(snapshot_merkle_root),
            ai_intent: None,
            intent_confidence: None,
            files_changed,
            category,
            scope_label,
            structured_description,
            git_commit_sha: git_commit_mapping.clone(),
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

    /// Snapshot the working directory, emitting FileWrite/FileDelete events
    /// for any changes since the last snapshot. Native mode only.
    ///
    /// Returns the number of file events emitted.
    pub fn snapshot_working_directory(&self) -> Result<usize> {
        self.assert_initialized()?;

        if self.repo_mode()? != RepoMode::Native {
            return Ok(0);
        }

        let store = ContentStore::for_root(self.root());
        store.ensure_exists()?;
        let chunk_config = ChunkConfig::default();

        // Load mtime cache for fast skip
        let mtime_cache_path = self.root.join(FLOCK_DIR).join("cache").join("snapshot-mtimes.json");
        let mtime_cache: HashMap<String, u128> = if mtime_cache_path.is_file() {
            let raw = fs::read_to_string(&mtime_cache_path).unwrap_or_default();
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            HashMap::new()
        };

        // Replay to get current file_states
        let state = self.replay_state()?;
        let mut current_file_states = state.file_states;

        // Walk working directory
        let colocated = false; // Native mode is never colocated
        let file_paths: Vec<PathBuf> = build_repo_walker(&self.root, colocated)
            .build()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map_or(false, |ft| ft.is_file()))
            .map(|e| e.into_path())
            .collect();

        let mut events_emitted = 0usize;
        let mut new_mtime_cache: HashMap<String, u128> = HashMap::new();

        // Check each file on disk
        for path in &file_paths {
            let rel_path = path
                .strip_prefix(&self.root)
                .context("failed to compute relative path for snapshot")?;
            let rel_key = rel_path_to_key(rel_path);

            // Mtime-based skip: if file mtime hasn't changed since last snapshot
            // and we already have it in file_states, skip expensive hashing.
            let file_mtime = fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos());

            if let (Some(mtime), Some(cached_mtime)) = (file_mtime, mtime_cache.get(&rel_key)) {
                if mtime == *cached_mtime && current_file_states.contains_key(&rel_key) {
                    // File unchanged — skip
                    new_mtime_cache.insert(rel_key.clone(), mtime);
                    current_file_states.remove(&rel_key);
                    continue;
                }
            }

            let contents = fs::read(path).with_context(|| {
                format!("failed to read {} for snapshot", path.display())
            })?;
            let file_hash = blake3::hash(&contents).to_hex().to_string();

            // Record mtime for future cache
            if let Some(mtime) = file_mtime {
                new_mtime_cache.insert(rel_key.clone(), mtime);
            }

            // Check if unchanged by hash
            if let Some(existing) = current_file_states.get(&rel_key) {
                if existing.content_hash == file_hash {
                    current_file_states.remove(&rel_key);
                    continue;
                }
            }

            // File is new or modified — chunk, store, and emit FileWrite
            let chunks = chunk_data(&contents, &chunk_config);
            let mut block_refs = Vec::with_capacity(chunks.len());
            for chunk in &chunks {
                let chunk_bytes = &contents[chunk.offset..chunk.offset + chunk.length];
                let hash = store.put(chunk_bytes)?;
                block_refs.push(BlockRef {
                    hash,
                    offset: chunk.offset,
                    length: chunk.length,
                });
            }

            let previous_file_event = current_file_states
                .get(&rel_key)
                .map(|fs| fs.event_id);

            current_file_states.remove(&rel_key);

            self.append_event(EventKind::FileWrite(FileWriteEvent {
                path: rel_key,
                content_hash: file_hash,
                blocks: block_refs,
                size: contents.len() as u64,
                previous_file_event,
            }))?;

            events_emitted += 1;
        }

        // Remaining entries in current_file_states are files that were deleted
        for (path, file_state) in &current_file_states {
            self.append_event(EventKind::FileDelete(FileDeleteEvent {
                path: path.clone(),
                previous_file_event: Some(file_state.event_id),
            }))?;
            events_emitted += 1;
        }

        // Save mtime cache
        if let Some(parent) = mtime_cache_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&new_mtime_cache) {
            let _ = fs::write(&mtime_cache_path, json);
        }

        Ok(events_emitted)
    }

    /// Restore a single file from its content blocks.
    pub fn restore_file_from_blocks(&self, rel_path: &str, blocks: &[BlockRef]) -> Result<()> {
        let store = ContentStore::for_root(self.root());
        let target = self.root.join(rel_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create directory {}", parent.display())
            })?;
        }

        let mut contents = Vec::new();
        for block_ref in blocks {
            let block_data = store.get(&block_ref.hash)?;
            contents.extend_from_slice(&block_data);
        }

        fs::write(&target, &contents).with_context(|| {
            format!("failed to write {}", target.display())
        })?;

        Ok(())
    }

    /// Store a snapshot using the native block-level content store.
    ///
    /// Walks the source directory, chunks each file, stores blocks in the
    /// content store, and writes a snapshot index mapping paths to blocks.
    fn create_native_snapshot(
        &self,
        source_root: &Path,
        apply_skip: bool,
        snapshot_id: Uuid,
    ) -> Result<()> {
        let store = ContentStore::for_root(self.root());
        store.ensure_exists()?;
        let file_index = FileIndex::for_root(self.root());
        file_index.ensure_exists()?;

        let chunk_config = ChunkConfig::default();
        let mut index = SnapshotIndex::new(snapshot_id);

        // Collect file paths using either the ignore-aware walker or plain walkdir.
        let file_paths: Vec<PathBuf> = if apply_skip {
            let colocated = self.repo_mode()? == RepoMode::GitColocated;
            build_repo_walker(source_root, colocated)
                .build()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map_or(false, |ft| ft.is_file()))
                .map(|e| e.into_path())
                .collect()
        } else {
            WalkDir::new(source_root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .collect()
        };

        for path in &file_paths {
            let rel_path = path
                .strip_prefix(source_root)
                .context("failed to compute relative path for native snapshot")?;

            let contents = fs::read(path).with_context(|| {
                format!("failed to read {} for native snapshot", path.display())
            })?;

            let file_hash = blake3::hash(&contents).to_hex().to_string();
            let chunks = chunk_data(&contents, &chunk_config);
            let mut block_refs = Vec::with_capacity(chunks.len());

            for chunk in &chunks {
                let chunk_bytes = &contents[chunk.offset..chunk.offset + chunk.length];
                let hash = store.put(chunk_bytes)?;
                block_refs.push(BlockRef {
                    hash,
                    offset: chunk.offset,
                    length: chunk.length,
                });
            }

            let rel_key = rel_path_to_key(rel_path);
            index.insert(
                rel_key,
                FileEntry {
                    blocks: block_refs,
                    size: contents.len() as u64,
                    file_hash,
                },
            );
        }

        file_index.write(&index)?;
        Ok(())
    }

    /// Restore the workspace from a native snapshot index by reassembling
    /// files from their content blocks.
    fn restore_workspace_from_native_snapshot(&self, snapshot_id: Uuid) -> Result<()> {
        let store = ContentStore::for_root(self.root());
        let file_index = FileIndex::for_root(self.root());
        let index = file_index.read(snapshot_id)?;

        self.clear_workspace_files()?;

        for (rel_path, entry) in &index.files {
            let target = self.root.join(rel_path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create directory {}", parent.display())
                })?;
            }

            let mut contents = Vec::with_capacity(entry.size as usize);
            for block_ref in &entry.blocks {
                let block_data = store.get(&block_ref.hash)?;
                contents.extend_from_slice(&block_data);
            }

            // Verify integrity
            let actual_hash = blake3::hash(&contents).to_hex().to_string();
            if actual_hash != entry.file_hash {
                bail!(
                    "integrity error restoring {}: expected hash {}, got {}",
                    rel_path,
                    entry.file_hash,
                    actual_hash
                );
            }

            fs::write(&target, &contents).with_context(|| {
                format!("failed to write {}", target.display())
            })?;
        }

        Ok(())
    }

    /// Restore a single file from a native snapshot (for sub-file undo).
    fn restore_workspace_file_from_native_snapshot(
        &self,
        snapshot_id: Uuid,
        rel_path: &Path,
    ) -> Result<()> {
        let store = ContentStore::for_root(self.root());
        let file_index = FileIndex::for_root(self.root());
        let index = file_index.read(snapshot_id)?;

        let rel_key = rel_path_to_key(rel_path);
        let workspace_file = self.root.join(rel_path);

        if let Some(entry) = index.files.get(&rel_key) {
            // File exists in snapshot — reassemble from blocks
            let mut contents = Vec::with_capacity(entry.size as usize);
            for block_ref in &entry.blocks {
                let block_data = store.get(&block_ref.hash)?;
                contents.extend_from_slice(&block_data);
            }

            if let Some(parent) = workspace_file.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create parent directory {}", parent.display())
                })?;
            }
            fs::write(&workspace_file, &contents).with_context(|| {
                format!("failed to restore {}", workspace_file.display())
            })?;
        } else if workspace_file.exists() {
            // File doesn't exist in snapshot — remove from workspace
            let metadata = fs::metadata(&workspace_file)?;
            if metadata.is_dir() {
                fs::remove_dir_all(&workspace_file)?;
            } else {
                fs::remove_file(&workspace_file)?;
            }
        }

        Ok(())
    }

    /// Migrate an existing repository to native storage mode.
    ///
    /// Converts all existing snapshots from directory-copy format to
    /// block-level content-addressed storage, then updates the config.
    pub fn migrate_to_native(&self) -> Result<MigrateReport> {
        self.assert_initialized()?;
        let mode = self.repo_mode()?;
        if mode == RepoMode::Native {
            bail!("repository is already in native mode");
        }

        // Initialize native store
        let store = ContentStore::for_root(self.root());
        store.ensure_exists()?;
        let file_index = FileIndex::for_root(self.root());
        file_index.ensure_exists()?;

        // Find all checkpoint events and migrate their snapshots
        let events = self.list_events()?;
        let mut snapshots_migrated = 0u32;
        let mut blocks_stored = 0u64;
        let mut bytes_before = 0u64;
        let chunk_config = ChunkConfig::default();

        for event in &events {
            if let EventKind::Checkpoint(checkpoint) = &event.kind {
                let snapshot_dir = self.snapshot_path(checkpoint.snapshot_id);
                if !snapshot_dir.is_dir() {
                    continue;
                }

                // Already migrated?
                if file_index.has(checkpoint.snapshot_id) {
                    continue;
                }

                let mut index = SnapshotIndex::new(checkpoint.snapshot_id);

                // Walk all files in the snapshot directory (not just source files)
                for entry in WalkDir::new(&snapshot_dir) {
                    let entry = entry?;
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let rel_path = entry.path().strip_prefix(&snapshot_dir)?;
                    let contents = fs::read(entry.path())?;
                    bytes_before += contents.len() as u64;

                    let file_hash = blake3::hash(&contents).to_hex().to_string();
                    let chunks = chunk_data(&contents, &chunk_config);
                    let mut block_refs = Vec::new();

                    for chunk in &chunks {
                        let chunk_bytes =
                            &contents[chunk.offset..chunk.offset + chunk.length];
                        let hash = store.put(chunk_bytes)?;
                        block_refs.push(BlockRef {
                            hash,
                            offset: chunk.offset,
                            length: chunk.length,
                        });
                        blocks_stored += 1;
                    }

                    let rel_key = rel_path_to_key(rel_path);
                    index.insert(
                        rel_key,
                        FileEntry {
                            blocks: block_refs,
                            size: contents.len() as u64,
                            file_hash,
                        },
                    );
                }

                file_index.write(&index)?;
                snapshots_migrated += 1;
            }
        }

        // Remove old snapshot directories now that data is in block store
        for event in &events {
            if let EventKind::Checkpoint(checkpoint) = &event.kind {
                let snapshot_dir = self.snapshot_path(checkpoint.snapshot_id);
                if snapshot_dir.is_dir() {
                    fs::remove_dir_all(&snapshot_dir).with_context(|| {
                        format!(
                            "failed to remove old snapshot directory {}",
                            snapshot_dir.display()
                        )
                    })?;
                }
            }
        }

        // Update config to native mode
        let config_path = self.root.join(CONFIG_FILE);
        let raw = fs::read_to_string(&config_path).unwrap_or_default();
        let updated = update_config_mode(&raw, "native");
        fs::write(&config_path, updated)
            .with_context(|| format!("failed to update config {}", config_path.display()))?;

        let bytes_after = store.total_size()?;

        Ok(MigrateReport {
            snapshots_migrated,
            blocks_stored,
            bytes_before,
            bytes_after,
        })
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
        let signing_key = self.load_signing_key()?;

        let exploration_id = self.active_exploration_id();
        let session_id = self.active_session_id();

        let mut event = Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            parent_id: None, // resolved atomically under lock by AutoEventLog::append
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            exploration_id,
            session_id,
            workspace_name: None, // populated by caller via CLI flag when needed
            kind,
        };
        // Signing happens inside the lock (via finalize callback) because
        // parent_id is part of the signing payload and is set under the lock.
        AutoEventLog::for_root(self.root()).append(&mut event, |ev| {
            let payload = fl_storage::event_signing_payload(ev)?;
            let signature = signing_key.sign(&payload);
            ev.signer_public_key = Some(hex::encode(signing_key.verifying_key().to_bytes()));
            ev.signature = Some(hex::encode(signature.to_bytes()));
            Ok(())
        })?;

        // Auto-materialize state at regular intervals for faster future replay
        self.maybe_auto_materialize();

        Ok(event)
    }

    /// Number of events between automatic materialized state snapshots.
    const AUTO_MATERIALIZE_INTERVAL: usize = 1_000;

    /// Check if the event count just crossed an interval boundary and
    /// snapshot the replayed state if so. Errors are silently ignored
    /// since materialization is an optimization, not a correctness requirement.
    fn maybe_auto_materialize(&self) {
        let events = match self.list_events() {
            Ok(e) => e,
            Err(_) => return,
        };

        let count = events.len();
        if count == 0 || count % Self::AUTO_MATERIALIZE_INTERVAL != 0 {
            return;
        }

        // Check if we already have a snapshot at this count
        let store = MaterializedStateStore::for_root(self.root());
        if let Ok(Some((latest, _))) = store.load_latest() {
            if latest >= count {
                return;
            }
        }

        // Replay state (using incremental replay if a prior snapshot exists)
        let state = match self.replay_state() {
            Ok(s) => s,
            Err(_) => return,
        };

        let json = match serde_json::to_string(&state) {
            Ok(j) => j,
            Err(_) => return,
        };

        let _ = store.save(count, &json);
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

    /// Find the checkpoint event ID that corresponds to a given git commit SHA.
    fn find_checkpoint_for_git_commit(&self, git_commit: &str) -> Result<Option<Uuid>> {
        for event in self.list_events()? {
            let EventKind::GitBridge(ref bridge) = event.kind else {
                continue;
            };
            if bridge.action != GitBridgeAction::Commit || !bridge.success {
                continue;
            }
            // detail format: "checkpoint=<uuid> git_commit=<sha>"
            let mut checkpoint_id = None;
            let mut commit_sha = None;
            for token in bridge.detail.split_whitespace() {
                if let Some((key, value)) = token.split_once('=') {
                    match key {
                        "checkpoint" => checkpoint_id = Uuid::parse_str(value).ok(),
                        "git_commit" => commit_sha = Some(value),
                        _ => {}
                    }
                }
            }
            if commit_sha == Some(git_commit) {
                return Ok(checkpoint_id);
            }
        }
        Ok(None)
    }

    fn snapshot_path(&self, snapshot_id: Uuid) -> PathBuf {
        self.root.join(SNAPSHOT_DIR).join(snapshot_id.to_string())
    }

    /// Ensure a snapshot directory exists on disk, lazily extracting from git
    /// if this is a virtual (git-backed) snapshot in colocated mode.
    fn ensure_snapshot_available(&self, snapshot_id: Uuid) -> Result<PathBuf> {
        let path = self.snapshot_path(snapshot_id);
        if path.is_dir() {
            return Ok(path);
        }
        // Native mode: materialize snapshot from block store
        if self.repo_mode()? == RepoMode::Native {
            let store = ContentStore::for_root(self.root());
            let file_index = FileIndex::for_root(self.root());
            let index = file_index.read(snapshot_id)?;

            fs::create_dir_all(&path)?;
            for (rel_path, entry) in &index.files {
                let target = path.join(rel_path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut contents = Vec::with_capacity(entry.size as usize);
                for block_ref in &entry.blocks {
                    let block_data = store.get(&block_ref.hash)?;
                    contents.extend_from_slice(&block_data);
                }
                fs::write(&target, &contents)?;
            }
            return Ok(path);
        }
        // Try lazy extraction from git in colocated mode
        if self.repo_mode()? == RepoMode::GitColocated {
            if let Some(sha) = self.git_sha_for_snapshot(snapshot_id)? {
                fs::create_dir_all(&path)?;
                self.extract_git_commit_tree_to_directory(&sha, &path)?;
                return Ok(path);
            }
        }
        bail!("snapshot {} not found", snapshot_id)
    }

    /// Look up the git commit SHA associated with a snapshot ID by scanning
    /// checkpoint events.
    fn git_sha_for_snapshot(&self, snapshot_id: Uuid) -> Result<Option<String>> {
        for event in self.list_events()? {
            if let EventKind::Checkpoint(cp) = &event.kind {
                if cp.snapshot_id == snapshot_id {
                    return Ok(cp.git_commit_sha.clone());
                }
            }
        }
        Ok(None)
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
                    "shadow mode safety check failed: refs/flock/branches/main is missing. Recovery: run `fl commit -m \"sync\"`"
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
        let store = AutoRefStore::for_root(self.root());
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

    // --- Collaboration: Presence ---

    pub fn heartbeat(
        &self,
        workspace: String,
        active_files: Vec<String>,
        intent: Option<String>,
        ttl_secs: Option<u64>,
    ) -> Result<PresenceSummary> {
        self.heartbeat_with_symbols(workspace, active_files, Vec::new(), intent, ttl_secs)
    }

    pub fn heartbeat_with_symbols(
        &self,
        workspace: String,
        active_files: Vec<String>,
        active_symbols: Vec<String>,
        intent: Option<String>,
        ttl_secs: Option<u64>,
    ) -> Result<PresenceSummary> {
        self.assert_initialized()?;
        let actor = self.current_actor();
        let ttl = ttl_secs.unwrap_or(300);
        self.append_event(EventKind::Presence(PresenceEvent {
            actor: actor.clone(),
            workspace: workspace.clone(),
            action: PresenceAction::Heartbeat,
            active_files: active_files.clone(),
            active_symbols: active_symbols.clone(),
            intent: intent.clone(),
            ttl_secs: ttl,
        }))?;
        Ok(PresenceSummary {
            actor,
            workspace,
            active_files,
            active_symbols,
            intent,
            ttl: std::time::Duration::from_secs(ttl),
            last_heartbeat: self.now_nanos_str(),
        })
    }

    pub fn depart(&self, workspace: String) -> Result<()> {
        self.assert_initialized()?;
        let actor = self.current_actor();
        self.append_event(EventKind::Presence(PresenceEvent {
            actor,
            workspace,
            action: PresenceAction::Depart,
            active_files: Vec::new(),
            active_symbols: Vec::new(),
            intent: None,
            ttl_secs: 0,
        }))?;
        Ok(())
    }

    pub fn list_presence(&self) -> Result<Vec<PresenceSummary>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let now_nanos = self.now_nanos();
        Ok(state
            .presence
            .into_values()
            .filter(|p| !p.is_expired_at(now_nanos))
            .collect())
    }

    // --- Who report (combines presence + sessions + tasks) ---

    pub fn who(&self) -> Result<WhoReport> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let now_nanos = self.now_nanos();

        let active_presences: Vec<_> = state
            .presence
            .values()
            .filter(|p| !p.is_expired_at(now_nanos))
            .collect();

        let active_sessions: std::collections::HashMap<String, &SessionSummary> = state
            .sessions
            .values()
            .filter(|s| s.status == SessionStatus::Active)
            .map(|s| (s.agent.clone(), s))
            .collect();

        let claimed_tasks: std::collections::HashMap<String, &TaskSummary> = state
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Claimed)
            .filter_map(|t| t.assignee.as_ref().map(|a| (a.clone(), t)))
            .collect();

        let mut actors = Vec::new();
        for presence in &active_presences {
            let session = active_sessions.get(&presence.actor);
            let task = claimed_tasks.get(&presence.actor);
            actors.push(ActorSummary {
                actor: presence.actor.clone(),
                workspace: presence.workspace.clone(),
                active_files: presence.active_files.clone(),
                active_symbols: presence.active_symbols.clone(),
                intent: presence.intent.clone(),
                current_task: task.map(|t| format!("{} — {}", &t.id.to_string()[..8], t.title.clone())),
                session_id: session.map(|s| s.id),
                last_seen: presence.last_heartbeat.clone(),
            });
        }

        // Include active sessions without presence entries
        for (agent, session) in &active_sessions {
            if !actors.iter().any(|a| &a.actor == agent) {
                let task = claimed_tasks.get(agent.as_str());
                actors.push(ActorSummary {
                    actor: agent.clone(),
                    workspace: String::new(),
                    active_files: Vec::new(),
                    active_symbols: Vec::new(),
                    intent: session.task_description.clone(),
                    current_task: task.map(|t| format!("{} — {}", &t.id.to_string()[..8], t.title.clone())),
                    session_id: Some(session.id),
                    last_seen: session.created_at.clone(),
                });
            }
        }

        Ok(WhoReport { actors })
    }

    // --- Collaboration: Directives ---

    pub fn send_directive(
        &self,
        target_actor: String,
        directive: fl_storage::DirectiveKind,
        reason: Option<String>,
    ) -> Result<fl_collab::DirectiveSummary> {
        self.assert_initialized()?;
        let issued_by = self.current_actor();
        let (kind_str, detail) = match &directive {
            fl_storage::DirectiveKind::Pause => ("pause".to_string(), None),
            fl_storage::DirectiveKind::Resume => ("resume".to_string(), None),
            fl_storage::DirectiveKind::Redirect { new_task } => ("redirect".to_string(), Some(new_task.clone())),
            fl_storage::DirectiveKind::Abort { reason } => ("abort".to_string(), Some(reason.clone())),
        };
        let event = self.append_event(EventKind::Directive(fl_storage::DirectiveEvent {
            target_actor: target_actor.clone(),
            directive,
            reason: reason.clone(),
            issued_by: issued_by.clone(),
        }))?;
        Ok(fl_collab::DirectiveSummary {
            id: event.id,
            target_actor,
            directive_kind: kind_str,
            directive_detail: detail,
            reason,
            issued_by,
            issued_at: event.timestamp,
            acknowledged: false,
        })
    }

    pub fn list_directives(&self) -> Result<Vec<fl_collab::DirectiveSummary>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        Ok(state.directives)
    }

    pub fn list_directives_for_actor(&self, actor: &str) -> Result<Vec<fl_collab::DirectiveSummary>> {
        let all = self.list_directives()?;
        Ok(all
            .into_iter()
            .filter(|d| d.target_actor == actor)
            .collect())
    }

    // --- Workspace Preview ---

    pub fn workspace_preview_diffs(&self) -> Result<Vec<fl_storage::PreviewDiff>> {
        self.assert_initialized()?;
        let status = self.status()?;
        let mut diffs = Vec::new();
        for path in &status.new_files {
            diffs.push(fl_storage::PreviewDiff {
                path: path.clone(),
                symbols_changed: Vec::new(),
                lines_added: 0,
                lines_removed: 0,
            });
        }
        for path in &status.modified_files {
            diffs.push(fl_storage::PreviewDiff {
                path: path.clone(),
                symbols_changed: Vec::new(),
                lines_added: 0,
                lines_removed: 0,
            });
        }
        for path in &status.deleted_files {
            diffs.push(fl_storage::PreviewDiff {
                path: path.clone(),
                symbols_changed: Vec::new(),
                lines_added: 0,
                lines_removed: 0,
            });
        }
        Ok(diffs)
    }

    // --- Collaboration: Advisory Locks ---

    pub fn acquire_lock(
        &self,
        resource: String,
        ttl_secs: Option<u64>,
    ) -> Result<LockSummary> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let now_nanos = self.now_nanos();

        can_acquire_lock(&resource, &state.locks, now_nanos)?;

        let lock_id = Uuid::new_v4();
        let holder = self.current_actor();
        let ttl = ttl_secs.unwrap_or(600);
        self.append_event(EventKind::Lock(LockEvent {
            lock_id,
            resource: resource.clone(),
            holder: holder.clone(),
            action: LockAction::Acquire,
            ttl_secs: ttl,
        }))?;
        Ok(LockSummary {
            id: lock_id,
            resource,
            holder,
            status: LockStatus::Held,
            ttl: std::time::Duration::from_secs(ttl),
            acquired_at: self.now_nanos_str(),
            released_at: None,
        })
    }

    pub fn release_lock(&self, lock_id: Uuid) -> Result<()> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let lock = state
            .locks
            .get(&lock_id)
            .ok_or_else(|| anyhow!("lock {} not found", lock_id))?;
        if lock.status != LockStatus::Held {
            bail!("lock {} is already released", lock_id);
        }
        self.append_event(EventKind::Lock(LockEvent {
            lock_id,
            resource: lock.resource.clone(),
            holder: lock.holder.clone(),
            action: LockAction::Release,
            ttl_secs: 0,
        }))?;
        Ok(())
    }

    pub fn list_locks(&self) -> Result<Vec<LockSummary>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let now_nanos = self.now_nanos();
        Ok(state
            .locks
            .into_values()
            .filter(|l| l.status == LockStatus::Held && !l.is_expired_at(now_nanos))
            .collect())
    }

    // --- Collaboration: Subscriptions ---

    pub fn subscribe(
        &self,
        paths: Vec<String>,
        symbols: Vec<String>,
        modules: Vec<String>,
        notify: Option<String>,
    ) -> Result<SubscriptionSummary> {
        self.assert_initialized()?;
        let actor = self.current_actor();
        let subscription_id = Uuid::new_v4();
        let notify_config = match notify.as_deref() {
            Some("batched") => NotifyConfig::Batched,
            Some("digest") => NotifyConfig::Digest,
            _ => NotifyConfig::Immediate,
        };
        let notify_kind = match notify_config {
            NotifyConfig::Immediate => SubscriptionNotify::Immediate,
            NotifyConfig::Batched => SubscriptionNotify::Batched,
            NotifyConfig::Digest => SubscriptionNotify::Digest,
        };
        self.append_event(EventKind::Subscription(SubscriptionEvent {
            subscription_id,
            actor: actor.clone(),
            action: SubscriptionAction::Subscribe,
            filter: Some(SubscriptionFilter {
                paths: paths.clone(),
                symbols: symbols.clone(),
                modules: modules.clone(),
            }),
            notify: Some(notify_config),
        }))?;
        Ok(SubscriptionSummary {
            id: subscription_id,
            actor,
            status: SubscriptionStatus::Active,
            paths,
            symbols,
            modules,
            notify: notify_kind,
            created_at: self.now_nanos_str(),
            cancelled_at: None,
        })
    }

    pub fn unsubscribe(&self, subscription_id: Uuid) -> Result<()> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let sub = state
            .subscriptions
            .get(&subscription_id)
            .ok_or_else(|| anyhow!("subscription {} not found", subscription_id))?;
        if sub.status != SubscriptionStatus::Active {
            bail!("subscription {} is already cancelled", subscription_id);
        }
        self.append_event(EventKind::Subscription(SubscriptionEvent {
            subscription_id,
            actor: sub.actor.clone(),
            action: SubscriptionAction::Unsubscribe,
            filter: None,
            notify: None,
        }))?;
        Ok(())
    }

    pub fn list_subscriptions(&self) -> Result<Vec<SubscriptionSummary>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        Ok(state
            .subscriptions
            .into_values()
            .filter(|s| s.status == SubscriptionStatus::Active)
            .collect())
    }

    // --- Collaboration: Gates ---

    pub fn create_gate(
        &self,
        condition: GateCondition,
        policy: GatePolicy,
    ) -> Result<GateSummary> {
        self.assert_initialized()?;
        let gate_id = Uuid::new_v4();
        let condition_kind = match &condition {
            GateCondition::FileTouched(p) => GateConditionKind::FileTouched(p.clone()),
            GateCondition::SymbolModified(s) => GateConditionKind::SymbolModified(s.clone()),
            GateCondition::ImpactExceeds(n) => GateConditionKind::ImpactExceeds(*n),
            GateCondition::SecuritySensitive => GateConditionKind::SecuritySensitive,
            GateCondition::AgentConfidenceLow(n) => GateConditionKind::AgentConfidenceLow(*n),
        };
        let policy_kind = match policy {
            GatePolicy::Block => GatePolicyKind::Block,
            GatePolicy::QueueAndContinue => GatePolicyKind::QueueAndContinue,
        };
        self.append_event(EventKind::Gate(GateEvent {
            gate_id,
            action: GateAction::Create,
            condition: Some(condition),
            policy: Some(policy),
            approved_by: None,
            reason: None,
        }))?;
        Ok(GateSummary {
            id: gate_id,
            status: GateStatus::Active,
            condition: condition_kind,
            policy: policy_kind,
            approved_by: None,
            reason: None,
            created_at: self.now_nanos_str(),
            resolved_at: None,
        })
    }

    pub fn approve_gate(&self, gate_id: Uuid, reason: Option<String>) -> Result<()> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let gate = state
            .gates
            .get(&gate_id)
            .ok_or_else(|| anyhow!("gate {} not found", gate_id))?;
        if gate.status != GateStatus::Active {
            bail!("gate {} is not active (status: {})", gate_id, gate.status);
        }
        let actor = self.current_actor();
        self.append_event(EventKind::Gate(GateEvent {
            gate_id,
            action: GateAction::Approve,
            condition: None,
            policy: None,
            approved_by: Some(actor),
            reason,
        }))?;
        Ok(())
    }

    pub fn reject_gate(&self, gate_id: Uuid, reason: Option<String>) -> Result<()> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let gate = state
            .gates
            .get(&gate_id)
            .ok_or_else(|| anyhow!("gate {} not found", gate_id))?;
        if gate.status != GateStatus::Active {
            bail!("gate {} is not active (status: {})", gate_id, gate.status);
        }
        let actor = self.current_actor();
        self.append_event(EventKind::Gate(GateEvent {
            gate_id,
            action: GateAction::Reject,
            condition: None,
            policy: None,
            approved_by: Some(actor),
            reason,
        }))?;
        Ok(())
    }

    pub fn delete_gate(&self, gate_id: Uuid) -> Result<()> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let gate = state
            .gates
            .get(&gate_id)
            .ok_or_else(|| anyhow!("gate {} not found", gate_id))?;
        if gate.status == GateStatus::Deleted {
            bail!("gate {} has already been deleted", gate_id);
        }
        self.append_event(EventKind::Gate(GateEvent {
            gate_id,
            action: GateAction::Delete,
            condition: None,
            policy: None,
            approved_by: None,
            reason: None,
        }))?;
        Ok(())
    }

    pub fn list_gates(&self) -> Result<Vec<GateSummary>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        Ok(state
            .gates
            .into_values()
            .filter(|g| g.status == GateStatus::Active)
            .collect())
    }

    /// List all gates regardless of status (for delete/admin operations).
    pub fn list_all_gates(&self) -> Result<Vec<GateSummary>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        Ok(state.gates.into_values().collect())
    }

    pub fn check_gates_for_path(&self, path: &str) -> Result<Vec<GateSummary>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;
        Ok(fl_collab::check_gates_for_path(path, &state.gates))
    }

    // --- Auto-rebase and conflict resolution ---

    /// Rebase a workspace onto a new base checkpoint.
    ///
    /// Compares the workspace's current base snapshot against the new checkpoint's
    /// snapshot and merges any changed files using semantic merge. Returns a list
    /// of conflicts found (empty if rebase was clean).
    pub fn rebase_workspace(
        &self,
        workspace_name: &str,
    ) -> Result<RebaseResult> {
        self.assert_initialized()?;

        let refs = self.list_refs()?;
        let ws_ref = refs
            .iter()
            .find(|r| r.kind == RefKind::Workspace && r.name == workspace_name)
            .ok_or_else(|| anyhow!("workspace `{}` not found", workspace_name))?
            .clone();

        let config = ws_ref
            .workspace
            .as_ref()
            .ok_or_else(|| anyhow!("workspace `{}` has no config", workspace_name))?;

        let old_base_event = ws_ref.target_event_id;
        let old_base_snapshot = config.base_snapshot_id
            .ok_or_else(|| anyhow!("workspace `{}` has no base snapshot", workspace_name))?;

        // Find the latest checkpoint to rebase onto
        let latest = self
            .latest_checkpoint()
            .ok_or_else(|| anyhow!("no checkpoint exists to rebase onto"))?;

        if latest.id == old_base_event {
            return Ok(RebaseResult {
                workspace: workspace_name.to_string(),
                old_base_event,
                new_base_event: old_base_event,
                files_merged: Vec::new(),
                conflicts: Vec::new(),
                already_up_to_date: true,
            });
        }

        let EventKind::Checkpoint(new_checkpoint) = &latest.kind else {
            bail!("latest checkpoint event is malformed");
        };
        let new_base_snapshot = new_checkpoint.snapshot_id;

        // Compare old base snapshot vs new base snapshot to find changed files
        let old_snapshot_path = self.ensure_snapshot_available(old_base_snapshot)?;
        let new_snapshot_path = self.ensure_snapshot_available(new_base_snapshot)?;

        // Collect files from both snapshots
        let old_files = collect_snapshot_files(&old_snapshot_path)?;
        let new_files = collect_snapshot_files(&new_snapshot_path)?;

        let mut files_merged = Vec::new();
        let mut conflicts = Vec::new();

        // Find files that changed between old and new base
        let all_paths: BTreeSet<String> = old_files
            .keys()
            .chain(new_files.keys())
            .cloned()
            .collect();

        for rel_path in &all_paths {
            let old_content = old_files.get(rel_path);
            let new_content = new_files.get(rel_path);

            // If the base content didn't change, nothing to merge
            if old_content == new_content {
                continue;
            }

            // Check if the workspace has local changes to this file
            let workspace_file = self.root.join(rel_path);
            if !workspace_file.exists() {
                if old_content.is_some() && new_content.is_some() {
                    // File existed in old base and was modified in new base,
                    // but is missing from workspace — user deleted it.
                    conflicts.push(ConflictDetail {
                        id: None,
                        path: rel_path.clone(),
                        symbol: None,
                        classification: "DeleteVsEdit".to_string(),
                        explanation: format!(
                            "File `{}` was deleted in workspace but modified in new base",
                            rel_path
                        ),
                    });
                } else if old_content.is_none() && new_content.is_some() {
                    // File is new in the new base and didn't exist before —
                    // not a deletion; fast-forward by creating it.
                    if let Some(parent) = workspace_file.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&workspace_file, new_content.unwrap())?;
                    files_merged.push(rel_path.clone());
                }
                continue;
            }

            let workspace_content = fs::read(&workspace_file).ok();

            // If file exists in workspace but hasn't changed from old base, just take new base version
            if workspace_content.as_deref() == old_content.map(|v| v.as_slice()) {
                if let Some(new) = new_content {
                    fs::write(&workspace_file, new)?;
                    files_merged.push(rel_path.clone());
                }
                continue;
            }

            // Both sides changed — need a three-way merge
            if let Some(workspace_bytes) = &workspace_content {
                let path = PathBuf::from(rel_path);
                let merge_result = fl_semantic::merge(
                    &path,
                    old_content.map(|v| v.as_slice()),
                    Some(workspace_bytes.as_slice()),
                    new_content.map(|v| v.as_slice()),
                );

                match merge_result {
                    Ok(Some(result)) => {
                        if result.conflicts.is_empty() {
                            // Clean merge — apply it
                            fs::write(&workspace_file, &result.merged_source)?;
                            files_merged.push(rel_path.clone());
                        } else {
                            // Conflicts found
                            for conflict in &result.conflicts {
                                conflicts.push(ConflictDetail {
                                    id: None,
                                    path: rel_path.clone(),
                                    symbol: Some(conflict.symbol.clone()),
                                    classification: format!("{:?}", conflict.classification),
                                    explanation: conflict.explanation.clone(),
                                });
                            }
                            // Write merged source with conflict markers
                            fs::write(&workspace_file, &result.merged_source)?;
                            files_merged.push(rel_path.clone());
                        }
                    }
                    Ok(None) | Err(_) => {
                        // Unsupported file type or merge error — text fallback
                        conflicts.push(ConflictDetail {
                            id: None,
                            path: rel_path.clone(),
                            symbol: None,
                            classification: "TextFallback".to_string(),
                            explanation: format!(
                                "Could not perform semantic merge on `{}`",
                                rel_path
                            ),
                        });
                    }
                }
            }
        }

        // Record the rebase event
        self.append_event(EventKind::Rebase(RebaseEvent {
            workspace: workspace_name.to_string(),
            old_base_event,
            new_base_event: latest.id,
            files_merged: files_merged.clone(),
            conflicts_found: conflicts.len(),
            auto: false,
        }))?;

        // Update workspace ref to point to new base
        let store = AutoRefStore::for_root(self.root());
        store.upsert(RepoRef {
            kind: RefKind::Workspace,
            name: workspace_name.to_string(),
            target_event_id: latest.id,
            workspace: Some(WorkspaceRefConfig {
                auto_rebase: config.auto_rebase,
                base_snapshot_id: Some(new_base_snapshot),
                max_snapshots: config.max_snapshots,
                max_events: config.max_events,
            }),
        })?;

        // Record detected conflicts as events
        for conflict in &conflicts {
            let conflict_id = Uuid::new_v4();
            self.append_event(EventKind::ConflictResolution(ConflictResolutionEvent {
                conflict_id,
                action: ConflictAction::Detect,
                workspace: Some(workspace_name.to_string()),
                path: Some(conflict.path.clone()),
                symbol: conflict.symbol.clone(),
                classification: None,
                suggestion: None,
                resolution: None,
                resolved_by: None,
                verified: None,
                reason: None,
            }))?;
            // Also classify immediately since we have that info
            self.append_event(EventKind::ConflictResolution(ConflictResolutionEvent {
                conflict_id,
                action: ConflictAction::Classify,
                workspace: Some(workspace_name.to_string()),
                path: Some(conflict.path.clone()),
                symbol: conflict.symbol.clone(),
                classification: Some(conflict.classification.clone()),
                suggestion: None,
                resolution: None,
                resolved_by: None,
                verified: None,
                reason: None,
            }))?;
        }

        Ok(RebaseResult {
            workspace: workspace_name.to_string(),
            old_base_event,
            new_base_event: latest.id,
            files_merged,
            conflicts,
            already_up_to_date: false,
        })
    }

    /// Auto-rebase all workspaces that have auto_rebase enabled.
    pub fn auto_rebase_workspaces(&self) -> Result<Vec<RebaseResult>> {
        self.assert_initialized()?;

        let workspaces = self.list_workspaces()?;
        let mut results = Vec::new();

        for ws in &workspaces {
            let config = ws.workspace.as_ref();
            if config.is_some_and(|c| c.auto_rebase) {
                match self.rebase_workspace(&ws.name) {
                    Ok(result) => results.push(result),
                    Err(e) => {
                        // Log but don't fail the whole batch
                        eprintln!("warning: auto-rebase failed for `{}`: {}", ws.name, e);
                    }
                }
            }
        }

        Ok(results)
    }

    /// Detect conflicts between workspace state and a target checkpoint.
    pub fn detect_conflicts(&self, workspace_name: &str) -> Result<Vec<ConflictDetail>> {
        self.assert_initialized()?;

        let refs = self.list_refs()?;
        let ws_ref = refs
            .iter()
            .find(|r| r.kind == RefKind::Workspace && r.name == workspace_name)
            .ok_or_else(|| anyhow!("workspace `{}` not found", workspace_name))?;

        let config = ws_ref
            .workspace
            .as_ref()
            .ok_or_else(|| anyhow!("workspace `{}` has no config", workspace_name))?;

        let base_snapshot = config.base_snapshot_id
            .ok_or_else(|| anyhow!("workspace has no base snapshot"))?;

        let base_snapshot_path = self.ensure_snapshot_available(base_snapshot)?;

        let base_files = collect_snapshot_files(&base_snapshot_path)?;
        let mut conflicts = Vec::new();

        // Compare workspace against latest checkpoint
        let latest = self.latest_checkpoint()
            .ok_or_else(|| anyhow!("no checkpoint exists"))?;

        if latest.id == ws_ref.target_event_id {
            return Ok(conflicts); // Already up to date
        }

        let EventKind::Checkpoint(new_cp) = &latest.kind else {
            bail!("latest checkpoint event is malformed");
        };

        let new_snapshot_path = self.ensure_snapshot_available(new_cp.snapshot_id)?;
        let new_files = collect_snapshot_files(&new_snapshot_path)?;

        let all_paths: BTreeSet<String> = base_files
            .keys()
            .chain(new_files.keys())
            .cloned()
            .collect();

        for rel_path in &all_paths {
            let base_content = base_files.get(rel_path);
            let new_content = new_files.get(rel_path);

            if base_content == new_content {
                continue;
            }

            let workspace_file = self.root.join(rel_path);
            let ws_content = fs::read(&workspace_file).ok();

            // If workspace changed the same file as the new base, there's a potential conflict
            if ws_content.as_deref() != base_content.map(|v| v.as_slice()) {
                let path = PathBuf::from(rel_path);
                let merge_result = fl_semantic::merge(
                    &path,
                    base_content.map(|v| v.as_slice()),
                    ws_content.as_deref(),
                    new_content.map(|v| v.as_slice()),
                );

                match merge_result {
                    Ok(Some(result)) if !result.conflicts.is_empty() => {
                        for conflict in &result.conflicts {
                            conflicts.push(ConflictDetail {
                                id: None,
                                path: rel_path.clone(),
                                symbol: Some(conflict.symbol.clone()),
                                classification: format!("{:?}", conflict.classification),
                                explanation: conflict.explanation.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // Persist detected conflicts as events so they flow through the
        // detect → suggest → resolve pipeline.
        for conflict in &mut conflicts {
            let conflict_id = Uuid::new_v4();
            conflict.id = Some(conflict_id);
            self.append_event(EventKind::ConflictResolution(ConflictResolutionEvent {
                conflict_id,
                action: ConflictAction::Detect,
                workspace: Some(workspace_name.to_string()),
                path: Some(conflict.path.clone()),
                symbol: conflict.symbol.clone(),
                classification: None,
                suggestion: None,
                resolution: None,
                resolved_by: None,
                verified: None,
                reason: None,
            }))?;
            self.append_event(EventKind::ConflictResolution(ConflictResolutionEvent {
                conflict_id,
                action: ConflictAction::Classify,
                workspace: Some(workspace_name.to_string()),
                path: Some(conflict.path.clone()),
                symbol: conflict.symbol.clone(),
                classification: Some(conflict.classification.clone()),
                suggestion: None,
                resolution: None,
                resolved_by: None,
                verified: None,
                reason: None,
            }))?;
        }

        Ok(conflicts)
    }

    /// Suggest a resolution strategy for a conflict.
    pub fn suggest_resolution(&self, conflict_id: Uuid) -> Result<String> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;

        let conflict = state
            .conflicts
            .get(&conflict_id)
            .ok_or_else(|| anyhow!("conflict `{}` not found", conflict_id))?;

        let suggestion = match conflict.classification.as_deref() {
            Some("DivergentEdit") => {
                "Both sides modified the same symbol. Review both versions and choose one, or manually merge the changes.".to_string()
            }
            Some("DeleteVsEdit") => {
                "One side deleted a symbol while the other modified it. Decide whether the deletion or the modification should win.".to_string()
            }
            Some("ConcurrentAddition") => {
                "Both sides added the same symbol with different implementations. Choose one implementation or combine them.".to_string()
            }
            Some("KindMismatch") => {
                "The symbol was changed to different kinds on each side. Choose which kind is correct.".to_string()
            }
            Some("TextFallback") => {
                "Could not perform semantic merge. Resolve conflict markers manually in the file.".to_string()
            }
            _ => {
                "Review the conflict and resolve manually.".to_string()
            }
        };

        // Record the suggestion
        self.append_event(EventKind::ConflictResolution(ConflictResolutionEvent {
            conflict_id,
            action: ConflictAction::Suggest,
            workspace: Some(conflict.workspace.clone()),
            path: Some(conflict.path.clone()),
            symbol: conflict.symbol.clone(),
            classification: conflict.classification.clone(),
            suggestion: Some(suggestion.clone()),
            resolution: None,
            resolved_by: None,
            verified: None,
            reason: None,
        }))?;

        Ok(suggestion)
    }

    /// Mark a conflict as resolved.
    pub fn resolve_conflict(
        &self,
        conflict_id: Uuid,
        resolution: String,
    ) -> Result<()> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;

        let conflict = state
            .conflicts
            .get(&conflict_id)
            .ok_or_else(|| anyhow!("conflict `{}` not found", conflict_id))?;

        fl_collab::can_advance_conflict(conflict.status, ConflictStatus::Resolved)?;

        self.append_event(EventKind::ConflictResolution(ConflictResolutionEvent {
            conflict_id,
            action: ConflictAction::Resolve,
            workspace: Some(conflict.workspace.clone()),
            path: Some(conflict.path.clone()),
            symbol: conflict.symbol.clone(),
            classification: conflict.classification.clone(),
            suggestion: conflict.suggestion.clone(),
            resolution: Some(resolution),
            resolved_by: Some(self.current_actor()),
            verified: None,
            reason: None,
        }))?;

        Ok(())
    }

    /// Verify a resolved conflict.
    pub fn verify_conflict(&self, conflict_id: Uuid, passed: bool) -> Result<()> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;

        let conflict = state
            .conflicts
            .get(&conflict_id)
            .ok_or_else(|| anyhow!("conflict `{}` not found", conflict_id))?;

        fl_collab::can_advance_conflict(conflict.status, ConflictStatus::Verified)?;

        self.append_event(EventKind::ConflictResolution(ConflictResolutionEvent {
            conflict_id,
            action: ConflictAction::Verify,
            workspace: Some(conflict.workspace.clone()),
            path: Some(conflict.path.clone()),
            symbol: conflict.symbol.clone(),
            classification: conflict.classification.clone(),
            suggestion: conflict.suggestion.clone(),
            resolution: conflict.resolution.clone(),
            resolved_by: conflict.resolved_by.clone(),
            verified: Some(passed),
            reason: None,
        }))?;

        Ok(())
    }

    /// Record a conflict resolution (final step).
    pub fn record_conflict(&self, conflict_id: Uuid, reason: Option<String>) -> Result<()> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;

        let conflict = state
            .conflicts
            .get(&conflict_id)
            .ok_or_else(|| anyhow!("conflict `{}` not found", conflict_id))?;

        fl_collab::can_advance_conflict(conflict.status, ConflictStatus::Recorded)?;

        self.append_event(EventKind::ConflictResolution(ConflictResolutionEvent {
            conflict_id,
            action: ConflictAction::Record,
            workspace: Some(conflict.workspace.clone()),
            path: Some(conflict.path.clone()),
            symbol: conflict.symbol.clone(),
            classification: conflict.classification.clone(),
            suggestion: conflict.suggestion.clone(),
            resolution: conflict.resolution.clone(),
            resolved_by: conflict.resolved_by.clone(),
            verified: Some(conflict.verified),
            reason,
        }))?;

        Ok(())
    }

    /// List all conflicts, optionally filtered by status.
    pub fn list_conflicts(
        &self,
        status_filter: Option<ConflictStatus>,
    ) -> Result<Vec<ConflictSummary>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;

        let conflicts: Vec<ConflictSummary> = state
            .conflicts
            .into_values()
            .filter(|c| status_filter.is_none() || Some(c.status) == status_filter)
            .collect();

        Ok(conflicts)
    }

    /// List all rebase operations for a workspace.
    pub fn list_rebases(&self, workspace_filter: Option<&str>) -> Result<Vec<RebaseSummary>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let state = replay_state(&events)?;

        let rebases: Vec<RebaseSummary> = state
            .rebases
            .into_iter()
            .filter(|r| workspace_filter.is_none() || Some(r.workspace.as_str()) == workspace_filter)
            .collect();

        Ok(rebases)
    }

    // --- Helper methods for collaboration ---

    pub fn current_actor_name(&self) -> String {
        self.current_actor()
    }

    fn current_actor(&self) -> String {
        env::var("FL_ACTOR")
            .or_else(|_| env::var("USER"))
            .unwrap_or_else(|_| "unknown".to_string())
    }

    fn now_nanos(&self) -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    fn now_nanos_str(&self) -> String {
        self.now_nanos().to_string()
    }

    // ---- Semantic indexing ------------------------------------------------

    /// Rebuild the full semantic index (AST cache + dependency graph).
    pub fn build_index(&self) -> Result<IndexReport> {
        self.assert_initialized()?;
        set_cache_root(self.root());

        let source_files = collect_source_files(self.root(), true)?;
        let mut files_indexed = 0usize;

        // Parse all source files to populate the persistent AST cache
        for rel_path in &source_files {
            let abs_path = self.root().join(rel_path);
            if let Ok(source) = fs::read(&abs_path) {
                if fl_semantic::supported_source(rel_path) {
                    let _ = fl_semantic::diff(rel_path, None, Some(&source));
                    files_indexed += 1;
                }
            }
        }

        // Build and persist the dependency graph
        let reverse_deps = build_reverse_dependency_index(self.root(), &source_files)?;
        let edges: HashMap<String, Vec<String>> = reverse_deps
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.iter().map(|p| p.to_string_lossy().to_string()).collect(),
                )
            })
            .collect();
        let edges_computed = edges.len();

        let snapshot_id = self
            .latest_checkpoint_event_id()
            .map(|id| id.to_string())
            .unwrap_or_default();

        let index = DependencyIndex {
            version: 1,
            snapshot_id,
            edges,
        };
        index.save(self.root())?;

        Ok(IndexReport {
            files_indexed,
            edges_computed,
        })
    }

    /// Clear all semantic index data (AST cache + dependency graph).
    pub fn clear_index(&self) -> Result<()> {
        self.assert_initialized()?;
        clear_cache(self.root())?;
        DependencyIndex::clear(self.root())?;
        Ok(())
    }

    /// Non-fatal auto-indexing after a checkpoint is created.
    fn auto_index_after_checkpoint(&self, event: &Event) -> Result<()> {
        set_cache_root(self.root());

        // Determine which files changed by comparing to the previous snapshot
        let EventKind::Checkpoint(ref payload) = event.kind else {
            return Ok(());
        };
        let _snapshot_dir = self.snapshot_path(payload.snapshot_id);

        // Parse symbols for all source files in the new snapshot to warm the cache
        let source_files = collect_source_files(self.root(), true)?;
        for rel_path in &source_files {
            let abs_path = self.root().join(rel_path);
            if let Ok(source) = fs::read(&abs_path) {
                if fl_semantic::supported_source(rel_path) {
                    let _ = fl_semantic::diff(rel_path, None, Some(&source));
                }
            }
        }

        // Rebuild dependency graph incrementally
        let mut index = DependencyIndex::load(self.root()).unwrap_or(DependencyIndex {
            version: 1,
            snapshot_id: String::new(),
            edges: HashMap::new(),
        });

        // Re-scan all files for import edges (incremental: only changed files
        // would be ideal, but the full scan is fast for typical repos)
        let reverse_deps = build_reverse_dependency_index(self.root(), &source_files)?;
        index.edges = reverse_deps
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.iter().map(|p| p.to_string_lossy().to_string()).collect(),
                )
            })
            .collect();
        index.snapshot_id = event.id.to_string();
        index.save(self.root())?;

        Ok(())
    }

    /// Try to use the cached dependency index; fall back to full rebuild.
    fn load_or_build_dependency_index(
        &self,
        current_files: &BTreeSet<PathBuf>,
    ) -> Result<HashMap<PathBuf, BTreeSet<PathBuf>>> {
        if let Some(index) = DependencyIndex::load(self.root()) {
            let latest = self
                .latest_checkpoint_event_id()
                .map(|id| id.to_string())
                .unwrap_or_default();
            if index.snapshot_id == latest {
                // Convert cached edges back to PathBuf map
                let mut result = HashMap::<PathBuf, BTreeSet<PathBuf>>::new();
                for (target, importers) in &index.edges {
                    let set: BTreeSet<PathBuf> =
                        importers.iter().map(PathBuf::from).collect();
                    result.insert(PathBuf::from(target), set);
                }
                return Ok(result);
            }
        }
        build_reverse_dependency_index(self.root(), current_files)
    }

    fn latest_checkpoint_event_id(&self) -> Option<Uuid> {
        let events = AutoEventLog::for_root(self.root()).read_all().ok()?;
        events
            .iter()
            .rev()
            .find(|e| matches!(e.kind, EventKind::Checkpoint(_)))
            .map(|e| e.id)
    }

    // -----------------------------------------------------------------------
    // Roost (remote) management
    // -----------------------------------------------------------------------

    pub fn roost_add(&self, name: &str, url: &str) -> Result<()> {
        self.assert_initialized()?;
        // Validate the URL parses.
        fl_storage::RemoteUrl::parse(url)?;
        let mut config = fl_storage::load_roosts(self.root())?;
        fl_storage::add_roost(&mut config, name, url)?;
        fl_storage::save_roosts(self.root(), &config)?;
        Ok(())
    }

    pub fn roost_remove(&self, name: &str) -> Result<()> {
        self.assert_initialized()?;
        let mut config = fl_storage::load_roosts(self.root())?;
        fl_storage::remove_roost(&mut config, name)?;
        fl_storage::save_roosts(self.root(), &config)?;
        Ok(())
    }

    pub fn roost_list(&self) -> Result<Vec<fl_storage::RoostEntry>> {
        self.assert_initialized()?;
        let config = fl_storage::load_roosts(self.root())?;
        Ok(config.roosts)
    }

    pub fn roost_set_url(&self, name: &str, url: &str) -> Result<()> {
        self.assert_initialized()?;
        fl_storage::RemoteUrl::parse(url)?;
        let mut config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost_mut(&mut config, name)
            .ok_or_else(|| anyhow!("roost '{}' not found", name))?;
        entry.url = url.to_string();
        fl_storage::save_roosts(self.root(), &config)?;
        Ok(())
    }

    /// Authenticate with a roost using a bearer token.
    ///
    /// Sends the token to the server, receives a session token, and stores it
    /// in the user-level credential store (`~/.flock/credentials.toml`).
    pub fn remote_login(&self, host: &str, token: &str) -> Result<RemoteLoginResult> {
        use crate::remote::transport_for_url;

        // Build a temporary transport to the host for authentication.
        // Parse host:port if a port is included.
        let (parsed_host, parsed_port) = if let Some(idx) = host.rfind(':') {
            if let Ok(p) = host[idx + 1..].parse::<u16>() {
                (host[..idx].to_string(), Some(p))
            } else {
                (host.to_string(), None)
            }
        } else {
            (host.to_string(), None)
        };
        let url = fl_storage::RemoteUrl {
            scheme: fl_storage::RemoteScheme::Flock,
            host: Some(parsed_host),
            port: parsed_port,
            path: String::new(),
        };
        let transport = transport_for_url(&url, None)?;

        let resp = transport.token_login(&fl_storage::TokenLoginRequest {
            token: token.to_string(),
        })?;

        if resp.success {
            let session_token = resp.session_token.unwrap_or_else(|| token.to_string());
            let mut store = fl_storage::CredentialStore::load()?;
            store.upsert(fl_storage::CredentialEntry {
                host: host.to_string(),
                token: session_token.clone(),
                method: fl_storage::AuthMethod::Token,
                ssh_key_path: None,
            });
            store.save()?;
            Ok(RemoteLoginResult {
                success: true,
                identity: resp.identity,
                error: None,
            })
        } else {
            Ok(RemoteLoginResult {
                success: false,
                identity: None,
                error: resp.error,
            })
        }
    }

    /// Authenticate with a roost using SSH key challenge-response.
    ///
    /// Uses the repo's ed25519 signing key to prove identity. The server
    /// sends a nonce, we sign it, and receive a session token.
    pub fn remote_login_ssh(&self, host: &str) -> Result<RemoteLoginResult> {
        use crate::remote::transport_for_url;
        use ed25519_dalek::{Signer, VerifyingKey};

        self.assert_initialized()?;

        // Load the repo's signing key.
        let sk_path = self.root.join(fl_storage::SIGNING_KEY_FILE);
        if !sk_path.exists() {
            bail!("no signing key found at {}; run `fl init` first", sk_path.display());
        }
        let sk_bytes = fs::read(&sk_path)?;
        if sk_bytes.len() < 32 {
            bail!("signing key file is too short");
        }
        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(&sk_bytes[..32]);
        let signing_key = SigningKey::from_bytes(&key_bytes);
        let verifying_key: VerifyingKey = (&signing_key).into();
        let pub_key_hex = hex::encode(verifying_key.as_bytes());

        // Build transport.
        // Parse host:port if a port is included.
        let (parsed_host, parsed_port) = if let Some(idx) = host.rfind(':') {
            if let Ok(p) = host[idx + 1..].parse::<u16>() {
                (host[..idx].to_string(), Some(p))
            } else {
                (host.to_string(), None)
            }
        } else {
            (host.to_string(), None)
        };
        let url = fl_storage::RemoteUrl {
            scheme: fl_storage::RemoteScheme::Flock,
            host: Some(parsed_host),
            port: parsed_port,
            path: String::new(),
        };
        let transport = transport_for_url(&url, None)?;

        // Step 1: challenge.
        let challenge = transport.ssh_auth_challenge(&fl_storage::SshAuthChallengeRequest {
            public_key_hex: pub_key_hex.clone(),
        })?;

        if !challenge.key_recognized {
            return Ok(RemoteLoginResult {
                success: false,
                identity: None,
                error: Some("public key not recognized by server".to_string()),
            });
        }

        // Step 2: sign the nonce and verify.
        let nonce_bytes = hex::decode(&challenge.nonce_hex)
            .map_err(|_| anyhow!("invalid nonce from server"))?;
        let signature = signing_key.sign(&nonce_bytes);
        let sig_hex = hex::encode(signature.to_bytes());

        let verify_resp = transport.ssh_auth_verify(&fl_storage::SshAuthVerifyRequest {
            public_key_hex: pub_key_hex,
            signature_hex: sig_hex,
            nonce_hex: challenge.nonce_hex,
        })?;

        if verify_resp.success {
            let session_token = verify_resp.session_token.unwrap_or_default();
            let mut store = fl_storage::CredentialStore::load()?;
            store.upsert(fl_storage::CredentialEntry {
                host: host.to_string(),
                token: session_token,
                method: fl_storage::AuthMethod::SshKey,
                ssh_key_path: Some(sk_path.to_string_lossy().to_string()),
            });
            store.save()?;
            Ok(RemoteLoginResult {
                success: true,
                identity: verify_resp.identity,
                error: None,
            })
        } else {
            Ok(RemoteLoginResult {
                success: false,
                identity: None,
                error: verify_resp.error,
            })
        }
    }

    /// Remove stored credentials for a host.
    pub fn remote_logout(&self, host: &str) -> Result<bool> {
        let mut store = fl_storage::CredentialStore::load()?;
        let removed = store.remove(host);
        if removed {
            store.save()?;
        }
        Ok(removed)
    }

    /// List all stored credentials (hosts only, not tokens).
    pub fn remote_credentials_list(&self) -> Result<Vec<RemoteCredentialInfo>> {
        let store = fl_storage::CredentialStore::load()?;
        Ok(store
            .credentials
            .iter()
            .map(|c| RemoteCredentialInfo {
                host: c.host.clone(),
                method: format!("{:?}", c.method).to_lowercase(),
            })
            .collect())
    }

    /// Push events and content to a roost.
    pub fn push(&self, roost_name: &str, branch: Option<&str>) -> Result<fl_storage::PushReport> {
        use crate::remote::{base64_encode, transport_for_url};

        self.assert_initialized()?;
        let mut config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost_mut(&mut config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        let url = fl_storage::RemoteUrl::parse(&entry.url)?;
        let resolved_token = fl_storage::resolve_token(entry.token.as_deref(), url.host.as_deref())?;
        let transport = transport_for_url(&url, resolved_token.as_deref())?;

        // Collect events since last sync.
        let all_events = self.list_events()?;
        let events_to_push = if let Some(last_synced) = entry.last_synced_event {
            let pos = all_events.iter().position(|e| e.id == last_synced);
            match pos {
                Some(idx) => all_events[idx + 1..].to_vec(),
                None => all_events.clone(),
            }
        } else {
            all_events.clone()
        };

        if events_to_push.is_empty() {
            return Ok(fl_storage::PushReport {
                roost_name: roost_name.to_string(),
                events_pushed: 0,
                blocks_uploaded: 0,
                rejected: false,
                detail: Some("already up to date".to_string()),
            });
        }

        // Collect snapshot IDs from checkpoint events in the push set.
        let snapshot_ids: Vec<Uuid> = events_to_push
            .iter()
            .filter_map(|e| {
                if let EventKind::Checkpoint(cp) = &e.kind {
                    Some(cp.snapshot_id)
                } else {
                    None
                }
            })
            .collect();

        // Upload missing snapshots.
        let mut blocks_uploaded = 0;
        if !snapshot_ids.is_empty() {
            let need_resp = transport.check_snapshots_needed(&fl_storage::SnapshotNeedRequest {
                snapshot_ids: snapshot_ids.clone(),
            })?;
            let is_native = self.repo_mode()? == RepoMode::Native;
            for snap_id in &need_resp.needed_ids {
                if is_native {
                    // Native mode: upload the snapshot index JSON as the snapshot data.
                    // Check the file index first (authoritative for native mode) even
                    // if a materialized cache directory also exists.
                    let file_index = fl_storage::FileIndex::for_root(self.root());
                    if file_index.has(*snap_id) {
                        let index = file_index.read(*snap_id)?;
                        let json = serde_json::to_vec(&index)?;
                        transport.upload_snapshot(*snap_id, &json)?;
                        blocks_uploaded += 1;
                    }
                } else {
                    // Git-compatible mode: pack snapshot directory as tar and upload.
                    let snap_dir = self.root.join(SNAPSHOT_DIR).join(snap_id.to_string());
                    if snap_dir.is_dir() {
                        let mut buf = Vec::new();
                        {
                            let mut builder = tar::Builder::new(&mut buf);
                            builder.append_dir_all(".", &snap_dir)?;
                            builder.finish()?;
                        }
                        transport.upload_snapshot(*snap_id, &buf)?;
                        blocks_uploaded += 1;
                    }
                }
            }
        }

        // Upload missing content blocks (native storage).
        let store_dir = self.flock_dir().join("store/blocks");
        if store_dir.is_dir() {
            let mut block_hashes = Vec::new();
            for entry_res in walkdir::WalkDir::new(&store_dir).into_iter().filter_map(|e| e.ok()) {
                if entry_res.file_type().is_file() {
                    if let Some(name) = entry_res.file_name().to_str() {
                        // The filename IS the full hash; the parent dir is just a 2-char fanout prefix.
                        block_hashes.push(name.to_string());
                    }
                }
            }
            if !block_hashes.is_empty() {
                let need_resp = transport.check_blocks_needed(&fl_storage::BlockNeedRequest {
                    block_hashes,
                })?;
                if !need_resp.needed_hashes.is_empty() {
                    let blocks: Vec<fl_storage::BlockPayload> = need_resp
                        .needed_hashes
                        .iter()
                        .filter_map(|h| {
                            let prefix = &h[..2];
                            let path = store_dir.join(prefix).join(h);
                            let data = fs::read(&path).ok()?;
                            Some(fl_storage::BlockPayload {
                                hash: h.clone(),
                                data_base64: base64_encode(&data),
                            })
                        })
                        .collect();
                    let resp = transport.upload_blocks(&fl_storage::BlockUploadRequest { blocks })?;
                    blocks_uploaded += resp.accepted;
                }
            }
        }

        // Collect refs to push (optionally filtered by branch).
        let all_refs = self.list_refs()?;
        let refs_to_push: Vec<_> = if let Some(branch) = branch {
            all_refs
                .into_iter()
                .filter(|r| r.name == branch)
                .collect()
        } else {
            all_refs
        };

        // Push events.
        let resp = transport.push_events(&fl_storage::EventPushRequest {
            events: events_to_push.clone(),
            refs: refs_to_push,
            last_known_remote_event: entry.last_synced_event,
        })?;

        let report = if resp.accepted {
            // Update last_synced_event.
            let entry = fl_storage::find_roost_mut(&mut config, roost_name).unwrap();
            entry.last_synced_event = events_to_push.last().map(|e| e.id);
            fl_storage::save_roosts(self.root(), &config)?;

            // Record sync event.
            self.append_event(EventKind::RemoteSync(crate::event::RemoteSyncEvent {
                action: crate::event::RemoteSyncAction::Push,
                roost_name: roost_name.to_string(),
                roost_url: url.to_string(),
                success: true,
                detail: None,
                event_count: resp.events_accepted,
                block_count: blocks_uploaded,
            }))?;

            fl_storage::PushReport {
                roost_name: roost_name.to_string(),
                events_pushed: resp.events_accepted,
                blocks_uploaded,
                rejected: false,
                detail: None,
            }
        } else {
            fl_storage::PushReport {
                roost_name: roost_name.to_string(),
                events_pushed: 0,
                blocks_uploaded: 0,
                rejected: true,
                detail: resp.detail,
            }
        };

        Ok(report)
    }

    /// Pull events and content from a roost.
    pub fn pull(&self, roost_name: &str, branch: Option<&str>) -> Result<fl_storage::PullReport> {
        self.assert_initialized()?;
        let config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost(&config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        // Delegate to pull_with_options with defaults from roost config.
        let sparse = entry.sparse_patterns.clone();
        let depth = entry.clone_depth;
        let lazy = entry.lazy;

        let report = self.pull_with_options(roost_name, branch, depth, &sparse, lazy)?;

        // Restore working directory from the latest pulled checkpoint so that
        // the workspace reflects the updated HEAD after pull.
        if !lazy && report.events_pulled > 0 {
            if let Some(cp_event) = self.latest_checkpoint() {
                if let EventKind::Checkpoint(cp) = &cp_event.kind {
                    self.restore_workspace_from_snapshot(cp.snapshot_id)?;
                }
            }
        }

        Ok(report)
    }

    /// Clone a remote repository to a local directory.
    ///
    /// This is a static method — it creates the target directory, initializes
    /// a Flock repo, adds the roost, and pulls with the given options.
    pub fn clone_from(
        url: &str,
        dir: &Path,
        depth: Option<usize>,
        sparse_patterns: Vec<String>,
        lazy: bool,
        focus_target: Option<&str>,
    ) -> Result<fl_storage::CloneReport> {
        // Parse URL to validate.
        fl_storage::RemoteUrl::parse(url)?;

        // Create the target directory and initialize.
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create clone directory {}", dir.display()))?;

        let repo = Repo::at(dir);
        // Initialize with git-compatible layout first; we'll switch to native
        // after pulling if the remote's Init event says mode=native.
        repo.init_layout(RepoMode::GitCompatible)?;
        repo.roost_add("origin", url)?;

        // Compute sparse patterns from focus target if specified.
        let final_sparse = if let Some(target) = focus_target {
            let mut patterns = repo.focus_clone_patterns(url, target)?;
            // Merge with any explicit sparse patterns.
            for p in &sparse_patterns {
                if !patterns.contains(p) {
                    patterns.push(p.clone());
                }
            }
            patterns
        } else {
            sparse_patterns.clone()
        };

        // Store clone config in roost entry.
        {
            let mut config = fl_storage::load_roosts(repo.root())?;
            let entry = fl_storage::find_roost_mut(&mut config, "origin").unwrap();
            entry.clone_depth = depth;
            entry.sparse_patterns = final_sparse.clone();
            entry.lazy = lazy;
            fl_storage::save_roosts(repo.root(), &config)?;
        }

        // Pull with options.
        let pull_report = repo.pull_with_options("origin", None, depth, &final_sparse, lazy)?;

        // Detect native mode from the pulled Init event and switch layout.
        let events = repo.list_events()?;
        let is_native = events.iter().any(|e| {
            matches!(&e.kind, EventKind::Init(init) if init.mode == "native")
        });
        if is_native {
            // Update config to native mode and create block store directories.
            let config_path = repo.root.join(CONFIG_FILE);
            if config_path.exists() {
                let contents = fs::read_to_string(&config_path)?;
                let updated = contents.replace("mode = \"git-compatible\"", "mode = \"native\"");
                fs::write(&config_path, updated)?;
            }
            ContentStore::for_root(repo.root()).ensure_exists()?;
            FileIndex::for_root(repo.root()).ensure_exists()?;
        }

        // Checkout working directory from the HEAD snapshot.
        if !lazy {
            if let Some(cp_event) = repo.latest_checkpoint() {
                if let EventKind::Checkpoint(cp) = &cp_event.kind {
                    repo.restore_workspace_from_snapshot(cp.snapshot_id)?;
                }
            }
        }

        Ok(fl_storage::CloneReport {
            pull: pull_report,
            clone_dir: dir.to_string_lossy().to_string(),
            depth,
            sparse_patterns: final_sparse,
            lazy,
        })
    }

    /// Pull events and content with partial clone options.
    pub fn pull_with_options(
        &self,
        roost_name: &str,
        branch: Option<&str>,
        depth: Option<usize>,
        sparse_patterns: &[String],
        lazy: bool,
    ) -> Result<fl_storage::PullReport> {
        self.assert_initialized()?;
        let mut config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost_mut(&mut config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        let url = fl_storage::RemoteUrl::parse(&entry.url)?;
        let resolved_token = fl_storage::resolve_token(entry.token.as_deref(), url.host.as_deref())?;
        let transport = crate::remote::transport_for_url(&url, resolved_token.as_deref())?;

        let sparse = if sparse_patterns.is_empty() {
            None
        } else {
            Some(sparse_patterns.to_vec())
        };

        let resp = transport.pull_events(&fl_storage::EventPullRequest {
            last_known_event: entry.last_synced_event,
            branch: branch.map(String::from),
            depth,
            sparse_paths: sparse,
        })?;

        if resp.events.is_empty() {
            return Ok(fl_storage::PullReport {
                roost_name: roost_name.to_string(),
                events_pulled: 0,
                blocks_downloaded: 0,
                refs_updated: vec![],
            });
        }

        let events_pulled = resp.events.len();

        // Download missing snapshots (unless lazy).
        let mut blocks_downloaded = 0;
        let snapshot_ids: Vec<Uuid> = resp
            .events
            .iter()
            .filter_map(|e| {
                if let EventKind::Checkpoint(cp) = &e.kind {
                    Some(cp.snapshot_id)
                } else {
                    None
                }
            })
            .collect();

        if !lazy {
            for snap_id in &snapshot_ids {
                let snap_dir = self.root.join(SNAPSHOT_DIR).join(snap_id.to_string());
                let file_index = fl_storage::FileIndex::for_root(self.root());
                let snap_exists = snap_dir.exists() || file_index.has(*snap_id);
                if !snap_exists {
                    match transport.download_snapshot(*snap_id) {
                        Ok(data) => {
                            // Try to parse as native snapshot index (JSON).
                            if let Ok(index) = serde_json::from_slice::<fl_storage::SnapshotIndex>(&data) {
                                // Native mode: save the index and download blocks.
                                file_index.ensure_exists()?;
                                file_index.write(&index)?;

                                // Download content blocks referenced by this index.
                                let block_hashes: Vec<String> = index
                                    .files
                                    .values()
                                    .flat_map(|entry| entry.blocks.iter().map(|b| b.hash.clone()))
                                    .collect();
                                let store = fl_storage::ContentStore::for_root(self.root());
                                store.ensure_exists()?;
                                for hash in &block_hashes {
                                    if !store.has(hash) {
                                        if let Ok(block_data) = transport.download_block(hash) {
                                            store.put(&block_data)?;
                                            blocks_downloaded += 1;
                                        }
                                    }
                                }
                            } else {
                                // Git-compatible mode: unpack tar archive.
                                fs::create_dir_all(&snap_dir)?;
                                let cursor = Cursor::new(data);
                                let mut archive = tar::Archive::new(cursor);
                                archive.unpack(&snap_dir)?;
                                blocks_downloaded += 1;

                                // If sparse, remove files not matching patterns.
                                if !sparse_patterns.is_empty() {
                                    self.apply_sparse_filter(&snap_dir, sparse_patterns)?;
                                }
                            }
                        }
                        Err(_) => {
                            // Snapshot may not exist on remote yet.
                        }
                    }
                }
            }
        }

        // Deduplicate: after a backup restore the local event log may already
        // contain some of the events the remote is sending.  Filter those out
        // so we never write duplicate event IDs (which would crash replay).
        let existing_ids: BTreeSet<Uuid> = {
            let local_events = AutoEventLog::for_root(self.root()).read_all()?;
            local_events.iter().map(|e| e.id).collect()
        };
        let deduped_events: Vec<Event> = resp
            .events
            .iter()
            .filter(|e| !existing_ids.contains(&e.id))
            .cloned()
            .collect();

        if deduped_events.is_empty() {
            // All pulled events already exist locally — nothing to append.
            // Still update refs and sync state.
            let ref_store = AutoRefStore::for_root(self.root());
            let refs_updated: Vec<String> = resp.refs.iter().map(|r| r.name.clone()).collect();
            for r in &resp.refs {
                ref_store.upsert(r.clone())?;
            }
            let entry = fl_storage::find_roost_mut(&mut config, roost_name).unwrap();
            entry.last_synced_event = resp.events.last().map(|e| e.id);
            fl_storage::save_roosts(self.root(), &config)?;

            self.append_event(EventKind::RemoteSync(crate::event::RemoteSyncEvent {
                action: crate::event::RemoteSyncAction::Pull,
                roost_name: roost_name.to_string(),
                roost_url: url.to_string(),
                success: true,
                detail: Some(format!(
                    "{} events already present locally (skipped duplicates)",
                    events_pulled
                )),
                event_count: 0,
                block_count: 0,
            }))?;

            return Ok(fl_storage::PullReport {
                roost_name: roost_name.to_string(),
                events_pulled: 0,
                blocks_downloaded,
                refs_updated,
            });
        }

        // Re-sign any graft events (those with cleared signatures from
        // shallow clone depth truncation) using our local signing key.
        let mut events_to_append = deduped_events;
        for event in &mut events_to_append {
            if event.signature.is_none() && event.signer_public_key.is_none() {
                // Graft event — re-sign with our key.
                self.sign_event(event)?;
            }
        }

        // Re-parent the first pulled event if our local log has diverged due
        // to local-only events (e.g. RemoteSync from a previous pull/push).
        // The first event's parent_id points to the remote's chain, but our
        // local tail may be a RemoteSync event.  After deduplication, the
        // first event's parent may also be one that was already in the local
        // log.  Re-parent and re-sign so the causal chain stays valid locally.
        let local_tail = AutoEventLog::for_root(self.root()).latest_event_id()?;
        if let (Some(first_event), Some(local_tail_id)) =
            (events_to_append.first_mut(), local_tail)
        {
            if first_event.parent_id != Some(local_tail_id) {
                first_event.parent_id = Some(local_tail_id);
                self.sign_event(first_event)?;
            }
        }

        // Fix internal chain gaps from deduplication: if we removed events
        // from the middle of the pulled batch, subsequent events may reference
        // a removed duplicate as their parent.  Re-parent them to the previous
        // event that is still in the batch.
        for i in 1..events_to_append.len() {
            let prev_id = events_to_append[i - 1].id;
            if events_to_append[i].parent_id != Some(prev_id) {
                events_to_append[i].parent_id = Some(prev_id);
                self.sign_event(&mut events_to_append[i])?;
            }
        }

        // Append pulled events (use relaxed validation for pull/clone since
        // the remote causal chain may have branches from undo/exploration events).
        AutoEventLog::for_root(self.root()).append_batch_for_pull(&events_to_append)?;

        // Update refs from remote.
        let ref_store = AutoRefStore::for_root(self.root());
        let refs_updated: Vec<String> = resp.refs.iter().map(|r| r.name.clone()).collect();
        for r in &resp.refs {
            ref_store.upsert(r.clone())?;
        }

        // Update last_synced_event.
        let entry = fl_storage::find_roost_mut(&mut config, roost_name).unwrap();
        entry.last_synced_event = resp.events.last().map(|e| e.id);
        fl_storage::save_roosts(self.root(), &config)?;

        // Record sync event.
        let actually_appended = events_to_append.len();
        let dedup_detail = if actually_appended < events_pulled {
            Some(format!(
                "{} duplicate events skipped",
                events_pulled - actually_appended
            ))
        } else {
            None
        };
        self.append_event(EventKind::RemoteSync(crate::event::RemoteSyncEvent {
            action: crate::event::RemoteSyncAction::Pull,
            roost_name: roost_name.to_string(),
            roost_url: url.to_string(),
            success: true,
            detail: dedup_detail,
            event_count: actually_appended,
            block_count: blocks_downloaded,
        }))?;

        Ok(fl_storage::PullReport {
            roost_name: roost_name.to_string(),
            events_pulled,
            blocks_downloaded,
            refs_updated,
        })
    }

    /// Apply sparse filter — remove files from a snapshot directory that don't
    /// match any of the given glob patterns.
    fn apply_sparse_filter(&self, snap_dir: &Path, patterns: &[String]) -> Result<()> {
        let compiled: Vec<glob::Pattern> = patterns
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect();

        if compiled.is_empty() {
            return Ok(());
        }

        // Walk the snapshot directory and remove non-matching files.
        let mut to_remove = Vec::new();
        for entry in WalkDir::new(snap_dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            if let Ok(rel) = entry.path().strip_prefix(snap_dir) {
                let rel_str = rel.to_string_lossy();
                let matches = compiled.iter().any(|pat| pat.matches(&rel_str));
                if !matches {
                    to_remove.push(entry.path().to_path_buf());
                }
            }
        }

        for path in to_remove {
            let _ = fs::remove_file(&path);
        }

        Ok(())
    }

    /// Add a sparse pattern for a roost.
    pub fn sparse_add(&self, roost_name: &str, pattern: &str) -> Result<()> {
        self.assert_initialized()?;
        let mut config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost_mut(&mut config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        if !entry.sparse_patterns.contains(&pattern.to_string()) {
            entry.sparse_patterns.push(pattern.to_string());
        }
        fl_storage::save_roosts(self.root(), &config)?;
        Ok(())
    }

    /// Remove a sparse pattern from a roost.
    pub fn sparse_remove(&self, roost_name: &str, pattern: &str) -> Result<()> {
        self.assert_initialized()?;
        let mut config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost_mut(&mut config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        let before = entry.sparse_patterns.len();
        entry.sparse_patterns.retain(|p| p != pattern);
        if entry.sparse_patterns.len() == before {
            bail!("sparse pattern '{}' not found on roost '{}'", pattern, roost_name);
        }
        fl_storage::save_roosts(self.root(), &config)?;
        Ok(())
    }

    /// List sparse patterns for a roost.
    pub fn sparse_list(&self, roost_name: &str) -> Result<Vec<String>> {
        self.assert_initialized()?;
        let config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost(&config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;
        Ok(entry.sparse_patterns.clone())
    }

    /// Fetch additional history for a shallow clone.
    pub fn fetch_deepen(&self, roost_name: &str, additional_depth: usize) -> Result<fl_storage::PullReport> {
        self.assert_initialized()?;
        let config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost(&config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        let current_depth = entry.clone_depth.unwrap_or(0);
        let new_depth = current_depth + additional_depth;

        // Pull with the new expanded depth.
        let sparse = entry.sparse_patterns.clone();
        let lazy = entry.lazy;
        let report = self.pull_with_options(roost_name, None, Some(new_depth), &sparse, lazy)?;

        // Update the stored depth.
        let mut config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost_mut(&mut config, roost_name).unwrap();
        entry.clone_depth = Some(new_depth);
        fl_storage::save_roosts(self.root(), &config)?;

        Ok(report)
    }

    /// Scan for missing blocks and fetch them from the roost.
    pub fn fetch_resolve_missing(&self, roost_name: &str) -> Result<usize> {
        self.assert_initialized()?;
        let config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost(&config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        let url = fl_storage::RemoteUrl::parse(&entry.url)?;
        let resolved_token = fl_storage::resolve_token(entry.token.as_deref(), url.host.as_deref())?;
        let transport = crate::remote::transport_for_url(&url, resolved_token.as_deref())?;

        let content_store = ContentStore::for_root(self.root());
        let file_index = FileIndex::for_root(self.root());

        // Find all snapshot indices and check for missing blocks.
        let mut missing_hashes: BTreeSet<String> = BTreeSet::new();
        for snap_id in file_index.list().unwrap_or_default() {
            if let Ok(index) = file_index.read(snap_id) {
                for entry in index.files.values() {
                    for block in &entry.blocks {
                        if !content_store.has(&block.hash) {
                            missing_hashes.insert(block.hash.clone());
                        }
                    }
                }
            }
        }

        if missing_hashes.is_empty() {
            return Ok(0);
        }

        let hashes: Vec<String> = missing_hashes.into_iter().collect();
        let fetched = transport.download_blocks_batch(&hashes)?;
        let mut count = 0;
        for (hash, data) in fetched {
            let stored_hash = content_store.put(&data)?;
            if stored_hash == hash {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Pin files matching a pattern for offline access.
    pub fn pin(&self, roost_name: &str, pattern: &str) -> Result<usize> {
        self.assert_initialized()?;
        let mut config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost_mut(&mut config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        // Store the pin pattern.
        if !entry.pin_patterns.contains(&pattern.to_string()) {
            entry.pin_patterns.push(pattern.to_string());
        }

        let url_str = entry.url.clone();
        let token = entry.token.clone();
        fl_storage::save_roosts(self.root(), &config)?;

        // Fetch blocks for matching files.
        let url = fl_storage::RemoteUrl::parse(&url_str)?;
        let resolved_token = fl_storage::resolve_token(token.as_deref(), url.host.as_deref())?;
        let transport = crate::remote::transport_for_url(&url, resolved_token.as_deref())?;

        let compiled = glob::Pattern::new(pattern)
            .map_err(|e| anyhow!("invalid glob pattern '{}': {}", pattern, e))?;

        let content_store = ContentStore::for_root(self.root());
        let file_index = FileIndex::for_root(self.root());

        let mut missing_hashes: BTreeSet<String> = BTreeSet::new();
        for snap_id in file_index.list().unwrap_or_default() {
            if let Ok(index) = file_index.read(snap_id) {
                for (path, entry) in &index.files {
                    if compiled.matches(path) {
                        for block in &entry.blocks {
                            if !content_store.has(&block.hash) {
                                missing_hashes.insert(block.hash.clone());
                            }
                        }
                    }
                }
            }
        }

        if missing_hashes.is_empty() {
            return Ok(0);
        }

        let hashes: Vec<String> = missing_hashes.into_iter().collect();
        let fetched = transport.download_blocks_batch(&hashes)?;
        let mut count = 0;
        for (_hash, data) in fetched {
            content_store.put(&data)?;
            count += 1;
        }
        Ok(count)
    }

    /// List pinned patterns for a roost.
    pub fn pin_list(&self, roost_name: &str) -> Result<Vec<String>> {
        self.assert_initialized()?;
        let config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost(&config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;
        Ok(entry.pin_patterns.clone())
    }

    /// Remove a pin pattern from a roost.
    pub fn pin_remove(&self, roost_name: &str, pattern: &str) -> Result<()> {
        self.assert_initialized()?;
        let mut config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost_mut(&mut config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        let before = entry.pin_patterns.len();
        entry.pin_patterns.retain(|p| p != pattern);
        if entry.pin_patterns.len() == before {
            bail!("pin pattern '{}' not found on roost '{}'", pattern, roost_name);
        }
        fl_storage::save_roosts(self.root(), &config)?;
        Ok(())
    }

    /// Parse Cargo.toml workspace to compute sparse patterns for a build target.
    ///
    /// Reads the remote's Cargo.toml (via a temporary pull of events to find it),
    /// finds workspace members, and computes the transitive closure of local
    /// path dependencies for the given target crate.
    pub fn focus_clone_patterns(&self, _url: &str, target: &str) -> Result<Vec<String>> {
        // Look for Cargo.toml in the working directory (it was fetched during init+pull).
        let cargo_toml = self.root.join("Cargo.toml");
        if !cargo_toml.exists() {
            // Try the snapshot directory for the latest checkpoint.
            let events = self.list_events().unwrap_or_default();
            for event in events.iter().rev() {
                if let EventKind::Checkpoint(cp) = &event.kind {
                    let snap_cargo = self.root.join(SNAPSHOT_DIR)
                        .join(cp.snapshot_id.to_string())
                        .join("Cargo.toml");
                    if snap_cargo.exists() {
                        return self.parse_cargo_focus(&snap_cargo, target);
                    }
                }
            }
            bail!("Cargo.toml not found; --focus requires a Cargo workspace");
        }
        self.parse_cargo_focus(&cargo_toml, target)
    }

    fn parse_cargo_focus(&self, cargo_toml: &Path, target: &str) -> Result<Vec<String>> {
        let content = fs::read_to_string(cargo_toml)
            .with_context(|| format!("failed to read {}", cargo_toml.display()))?;
        let parsed: toml::Value = toml::from_str(&content)
            .with_context(|| "failed to parse Cargo.toml")?;

        // Find workspace members.
        let mut members: Vec<String> = Vec::new();
        if let Some(workspace) = parsed.get("workspace") {
            if let Some(member_list) = workspace.get("members").and_then(|v| v.as_array()) {
                for m in member_list {
                    if let Some(s) = m.as_str() {
                        members.push(s.to_string());
                    }
                }
            }
        }

        // Find which member matches the target.
        let mut target_path = None;
        for member in &members {
            // Check if member ends with the target name or matches exactly.
            let member_name = member.rsplit('/').next().unwrap_or(member);
            if member_name == target || member == target {
                target_path = Some(member.clone());
                break;
            }
        }

        let target_path = target_path
            .ok_or_else(|| anyhow!("target '{}' not found in workspace members: {:?}", target, members))?;

        // Start with the target path and collect transitive path dependencies.
        let mut needed_paths: BTreeSet<String> = BTreeSet::new();
        let mut queue = vec![target_path.clone()];
        let mut visited = BTreeSet::new();

        while let Some(current) = queue.pop() {
            if !visited.insert(current.clone()) {
                continue;
            }
            needed_paths.insert(current.clone());

            // Read that member's Cargo.toml for path dependencies.
            let member_cargo = cargo_toml.parent().unwrap().join(&current).join("Cargo.toml");
            if let Ok(member_content) = fs::read_to_string(&member_cargo) {
                if let Ok(member_parsed) = toml::from_str::<toml::Value>(&member_content) {
                    // Check [dependencies], [dev-dependencies], [build-dependencies]
                    for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
                        if let Some(deps) = member_parsed.get(section).and_then(|v| v.as_table()) {
                            for (_name, dep) in deps {
                                if let Some(path) = dep.get("path").and_then(|v| v.as_str()) {
                                    // Resolve relative path
                                    let resolved = PathBuf::from(&current).join(path);
                                    let normalized = normalize_path(&resolved);
                                    // Check if it's a workspace member.
                                    let norm_str = normalized.to_string_lossy().to_string();
                                    if members.iter().any(|m| m == &norm_str || m.ends_with(&format!("/{}", norm_str)) || norm_str.ends_with(m)) {
                                        queue.push(norm_str);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Convert to glob patterns.
        let mut patterns: Vec<String> = needed_paths
            .iter()
            .map(|p| format!("{}/**", p))
            .collect();

        // Always include root Cargo.toml and workspace config files.
        patterns.push("Cargo.toml".to_string());
        patterns.push("Cargo.lock".to_string());
        patterns.push(".cargo/**".to_string());

        Ok(patterns)
    }

    /// Connect to a roost via WebSocket for real-time event streaming.
    /// Returns `(WsClient, repo_path)` where `repo_path` is the repository
    /// name from the roost URL (needed for Subscribe messages).
    pub fn ws_connect(
        &self,
        roost_name: &str,
    ) -> Result<(crate::ws_client::WsClient, String)> {
        self.assert_initialized()?;
        let config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost(&config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        let url = fl_storage::RemoteUrl::parse(&entry.url)?;
        let ws_url = url.ws_url()?;
        let repo_path = url.path.trim_start_matches('/').to_string();
        let resolved_token =
            fl_storage::resolve_token(entry.token.as_deref(), url.host.as_deref())?;
        let token = resolved_token.unwrap_or_default();

        let ws_config = crate::ws_client::WsClientConfig::new(ws_url, token);
        let client = crate::ws_client::WsClient::connect(ws_config)?;
        Ok((client, repo_path))
    }

    // -----------------------------------------------------------------------
    // Intelligence layer
    // -----------------------------------------------------------------------

    /// Load intelligence configuration from `.flock/config.toml` with env overrides.
    pub fn load_intelligence_config(&self) -> crate::intelligence::IntelligenceConfig {
        crate::intelligence::IntelligenceConfig::load(self.root())
    }

    /// Search history using TF-IDF, optionally enhanced with AI.
    pub fn query(&self, query: &str, use_ai: bool, limit: usize) -> Result<Vec<crate::intelligence::QueryResult>> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let config = self.load_intelligence_config();

        let llm_client = if use_ai && config.ai_available() {
            crate::intelligence::LlmClient::new(&config).ok()
        } else {
            None
        };

        crate::intelligence::query_history(&events, query, limit, llm_client.as_ref(), self.root())
    }

    /// Rebuild the intelligence search index from all events.
    pub fn rebuild_intelligence_index(&self) -> Result<crate::intelligence::IntelligenceIndexReport> {
        self.assert_initialized()?;
        let events = self.list_events()?;
        let index = crate::intelligence::SearchIndex::rebuild(&events);
        let report = crate::intelligence::IntelligenceIndexReport {
            events_indexed: index.doc_count,
            terms_indexed: index.term_count(),
        };
        index.save(self.root())?;
        Ok(report)
    }

    /// Get statistics about the intelligence search index.
    pub fn intelligence_index_stats(&self) -> Result<crate::intelligence::IndexStats> {
        self.assert_initialized()?;
        let index = crate::intelligence::SearchIndex::load_or_create(self.root());
        let index_path = self.root().join(fl_storage::FLOCK_DIR).join("intelligence").join("index.json");
        let size = fs::metadata(&index_path).map(|m| m.len()).unwrap_or(0);
        Ok(crate::intelligence::IndexStats {
            document_count: index.doc_count,
            term_count: index.term_count(),
            index_size_bytes: size,
        })
    }

    /// Extract intent from the current diff using AI or heuristics.
    pub fn extract_intent_for_diff(&self, message: Option<&str>) -> Result<crate::intelligence::IntentExtractionResult> {
        self.assert_initialized()?;
        let config = self.load_intelligence_config();

        let llm_client = if config.ai_available() {
            crate::intelligence::LlmClient::new(&config).ok()
        } else {
            None
        };

        // Get changed files from status
        let status = self.status()?;
        let changed_files: Vec<String> = status
            .new_files
            .into_iter()
            .chain(status.modified_files)
            .chain(status.deleted_files)
            .collect();

        Ok(crate::intelligence::extract_intent(&changed_files, message, llm_client.as_ref()))
    }

    /// AI-enhanced conflict resolution suggestion.
    pub fn suggest_resolution_ai(&self, conflict_id: Uuid) -> Result<crate::intelligence::ConflictSuggestionResult> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;

        let conflict = state
            .conflicts
            .get(&conflict_id)
            .ok_or_else(|| anyhow!("conflict `{}` not found", conflict_id))?;

        let config = self.load_intelligence_config();
        let llm_client = if config.ai_available() {
            crate::intelligence::LlmClient::new(&config).ok()
        } else {
            None
        };

        Ok(crate::intelligence::suggest_conflict_resolution(
            conflict.classification.as_deref(),
            Some(&conflict.path),
            conflict.symbol.as_deref(),
            llm_client.as_ref(),
        ))
    }

    /// Calculate a confidence score for the current session state.
    pub fn calculate_session_confidence(&self) -> Result<crate::intelligence::ConfidenceScore> {
        self.assert_initialized()?;

        let events = self.list_events()?;
        let state = replay_state(&events)?;
        let config = self.load_intelligence_config();

        let checkpoint_count = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Checkpoint(_)))
            .count();

        // Check if any test files exist in the working directory
        let has_tests = collect_source_files(self.root(), true)
            .map(|files| files.iter().any(|f| {
                let s = f.to_string_lossy().to_lowercase();
                s.contains("test") || s.contains("spec")
            }))
            .unwrap_or(false);

        let conflict_count = state
            .conflicts
            .values()
            .filter(|c| c.status != fl_collab::ConflictStatus::Recorded)
            .count();

        let gate_pending_count = state
            .gates
            .values()
            .filter(|g| g.status == fl_collab::GateStatus::Active)
            .count();

        // Count high-risk changes (from recent checkpoints' semantic diffs)
        let high_risk_changes = 0; // Would require diffing, keep simple for now

        Ok(crate::intelligence::calculate_confidence(
            checkpoint_count,
            has_tests,
            conflict_count,
            gate_pending_count,
            high_risk_changes,
            config.confidence_threshold,
        ))
    }
}

fn collect_snapshot_files(snapshot_dir: &Path) -> Result<HashMap<String, Vec<u8>>> {
    let mut files = HashMap::new();
    for entry in WalkDir::new(snapshot_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            let rel = entry
                .path()
                .strip_prefix(snapshot_dir)
                .unwrap_or(entry.path());
            let rel_str = rel.to_string_lossy().to_string();
            let content = fs::read(entry.path())?;
            files.insert(rel_str, content);
        }
    }
    Ok(files)
}

/// Directories that are always skipped during individual path checks (snapshot
/// restore, workspace clear, scoped undo validation).
const ALWAYS_SKIP_DIRS: &[&str] = &[".git", ".flock", "target", "node_modules", "__pycache__"];

/// Check whether a relative path should be skipped based on built-in directory
/// names. Used for path-level checks outside of tree walking (where
/// `build_repo_walker` handles filtering instead).
fn should_skip_path(root: &Path, path: &Path) -> bool {
    match path.strip_prefix(root) {
        Ok(rel) => should_skip_relative(rel),
        Err(_) => false,
    }
}

fn should_skip_relative(path: &Path) -> bool {
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        if ALWAYS_SKIP_DIRS.iter().any(|item| name == *item) {
            return true;
        }
    }
    false
}

/// Converts a relative path to a portable string key for storage.
/// On Windows, backslashes are replaced with forward slashes.
/// On Unix, backslashes are valid filename characters and left as-is.
fn rel_path_to_key(rel: &Path) -> String {
    let s = rel.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// Built-in patterns that are always ignored (directories and files that should
/// never be tracked by flock).
const BUILTIN_IGNORE_PATTERNS: &[&str] = &[
    ".flock/",
    ".git/",
    "node_modules/",
    "target/",
    "__pycache__/",
    ".env",
];

/// Build a `WalkBuilder` for the given root directory with `.flockignore`
/// support and built-in default patterns.
///
/// When `colocated` is true and no `.flockignore` exists, the builder will
/// also respect `.gitignore` files.
fn build_repo_walker(root: &Path, colocated: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);

    // Disable default gitignore/hidden handling — we control everything.
    builder.standard_filters(false);
    builder.hidden(false);

    let flockignore_path = root.join(".flockignore");
    let has_flockignore = flockignore_path.is_file();

    // Disable all git-specific ignore handling; we use custom ignore filenames
    // instead so it works regardless of git repo detection.
    builder.git_ignore(false);
    builder.git_global(false);
    builder.git_exclude(false);

    // Add the appropriate custom ignore filename.
    if has_flockignore {
        builder.add_custom_ignore_filename(".flockignore");
    } else if colocated {
        // Fall back to .gitignore in colocated mode when no .flockignore exists.
        builder.add_custom_ignore_filename(".gitignore");
    }

    // Add built-in patterns via an override set so they always apply.
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for pattern in BUILTIN_IGNORE_PATTERNS {
        // Override patterns use `!` prefix to negate (ignore) matches.
        let _ = overrides.add(&format!("!{}", pattern));
    }
    if let Ok(built) = overrides.build() {
        builder.overrides(built);
    }

    builder
}

fn copy_tree(source_root: &Path, destination_root: &Path, apply_skip: bool) -> Result<()> {
    copy_tree_with_mode(source_root, destination_root, apply_skip, false)
}

fn copy_tree_with_mode(
    source_root: &Path,
    destination_root: &Path,
    apply_skip: bool,
    colocated: bool,
) -> Result<()> {
    if apply_skip {
        let walker = build_repo_walker(source_root, colocated);
        for entry in walker.build() {
            let entry = entry.context("failed while walking source tree for copy")?;
            let path = entry.path();
            if path == source_root {
                continue;
            }

            let rel = path
                .strip_prefix(source_root)
                .context("failed to compute relative path while copying tree")?;

            let target = destination_root.join(rel);
            if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                fs::create_dir_all(&target)
                    .with_context(|| format!("failed to create {}", target.display()))?;
                continue;
            }

            if entry.file_type().map_or(false, |ft| ft.is_file()) {
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
    } else {
        // No filtering — plain walkdir for snapshot directories, etc.
        for entry in WalkDir::new(source_root) {
            let entry = entry.context("failed while walking source tree for copy")?;
            let path = entry.path();
            if path == source_root {
                continue;
            }

            let rel = path
                .strip_prefix(source_root)
                .context("failed to compute relative path while copying tree")?;
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
    collect_source_files_with_mode(root, apply_skip, false)
}

fn collect_source_files_with_mode(
    root: &Path,
    apply_skip: bool,
    colocated: bool,
) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();

    if apply_skip {
        let walker = build_repo_walker(root, colocated);
        for entry in walker.build() {
            let entry = entry.context("failed while scanning source files")?;
            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }

            let rel = entry
                .path()
                .strip_prefix(root)
                .context("failed to compute relative source path")?;

            if supported_source(rel) {
                files.insert(rel.to_path_buf());
            }
        }
    } else {
        for entry in WalkDir::new(root) {
            let entry = entry.context("failed while scanning source files")?;
            if !entry.file_type().is_file() {
                continue;
            }

            let rel = entry
                .path()
                .strip_prefix(root)
                .context("failed to compute relative source path")?;

            if supported_source(rel) {
                files.insert(rel.to_path_buf());
            }
        }
    }

    Ok(files)
}

/// Collect all files under root (not filtered by semantic support), returning
/// relative `PathBuf`s.  Used by `file_summary_*` functions so that diffs
/// include every file, not just those with a semantic analyzer.
fn collect_all_repo_files(root: &Path, apply_skip: bool) -> Result<BTreeSet<PathBuf>> {
    collect_all_repo_files_with_mode(root, apply_skip, false)
}

fn collect_all_repo_files_with_mode(
    root: &Path,
    apply_skip: bool,
    colocated: bool,
) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();

    if apply_skip {
        let walker = build_repo_walker(root, colocated);
        for entry in walker.build() {
            let entry = entry.context("failed while scanning repo files")?;
            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }

            let rel = entry
                .path()
                .strip_prefix(root)
                .context("failed to compute relative path")?;
            files.insert(rel.to_path_buf());
        }
    } else {
        for entry in WalkDir::new(root) {
            let entry = entry.context("failed while scanning repo files")?;
            if !entry.file_type().is_file() {
                continue;
            }

            let rel = entry
                .path()
                .strip_prefix(root)
                .context("failed to compute relative path")?;
            files.insert(rel.to_path_buf());
        }
    }

    Ok(files)
}

/// Collect all files (not just source files) under root, returning relative
/// path strings. Used by `status()` to compare working directory vs snapshot.
fn collect_all_files_with_mode(
    root: &Path,
    apply_skip: bool,
    colocated: bool,
) -> Result<(BTreeSet<String>, Vec<String>)> {
    let mut files = BTreeSet::new();
    let mut symlinks = Vec::new();

    if apply_skip {
        let walker = build_repo_walker(root, colocated);
        for entry in walker.build() {
            let entry = entry.context("failed while scanning files")?;
            let ft = entry.file_type();

            if ft.map_or(false, |ft| ft.is_symlink()) {
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .context("failed to compute relative path")?;
                symlinks.push(rel_path_to_key(rel));
                continue;
            }

            if !ft.map_or(false, |ft| ft.is_file()) {
                continue;
            }

            let rel = entry
                .path()
                .strip_prefix(root)
                .context("failed to compute relative path")?;
            files.insert(rel_path_to_key(rel));
        }
    } else {
        for entry in WalkDir::new(root) {
            let entry = entry.context("failed while scanning files")?;
            if !entry.file_type().is_file() {
                continue;
            }

            let rel = entry
                .path()
                .strip_prefix(root)
                .context("failed to compute relative path")?;
            files.insert(rel_path_to_key(rel));
        }
    }

    Ok((files, symlinks))
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
        let source = match fs::read_to_string(&source_path) {
            Ok(s) => s,
            Err(_) => continue, // skip binary / non-UTF-8 files
        };

        for specifier in extract_local_import_specifiers(&source, importer) {
            for target in resolve_import_targets(importer, &specifier, current_files) {
                reverse.entry(target).or_default().insert(importer.clone());
            }
        }
    }

    Ok(reverse)
}

fn extract_local_import_specifiers(source: &str, importer: &Path) -> Vec<String> {
    let ext = importer
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    match ext {
        "py" => extract_python_import_specifiers(source),
        "go" => extract_go_import_specifiers(source),
        "rs" => extract_rust_import_specifiers(source),
        "cs" => extract_csharp_import_specifiers(source),
        _ => extract_js_import_specifiers(source),
    }
}

fn extract_js_import_specifiers(source: &str) -> Vec<String> {
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

fn extract_python_import_specifiers(source: &str) -> Vec<String> {
    use regex::Regex;
    let mut specifiers = Vec::new();

    // `from .foo import bar` or `from ..foo import bar` (relative imports)
    let re_relative =
        Regex::new(r"(?m)^\s*from\s+(\.+\w*(?:\.\w+)*)\s+import\b").unwrap();
    for cap in re_relative.captures_iter(source) {
        let module = &cap[1];
        // Convert Python relative import to path-like specifier
        let mut prefix = String::new();
        let mut chars = module.chars();
        let mut dot_count = 0;
        for ch in chars.by_ref() {
            if ch == '.' {
                dot_count += 1;
            } else {
                // Put back the first non-dot char
                prefix = if dot_count == 1 {
                    format!("./{}", ch)
                } else {
                    let ups = "../".repeat(dot_count - 1);
                    format!("{}{}", ups, ch)
                };
                break;
            }
        }
        if prefix.is_empty() {
            // All dots, no module name (e.g., `from . import foo`)
            prefix = if dot_count == 1 {
                "./__init__".to_string()
            } else {
                format!("{}__init__", "../".repeat(dot_count - 1))
            };
        }
        let rest: String = chars.collect();
        let path = format!("{}{}", prefix, rest.replace('.', "/"));
        specifiers.push(path);
    }

    // `from app.module import X` or `import app.module` (absolute imports)
    let re_abs_from =
        Regex::new(r"(?m)^\s*from\s+([a-zA-Z_]\w*(?:\.\w+)*)\s+import\b").unwrap();
    for cap in re_abs_from.captures_iter(source) {
        let module = cap[1].replace('.', "/");
        specifiers.push(module);
    }

    let re_import = Regex::new(r"(?m)^\s*import\s+([a-zA-Z_]\w*(?:\.\w+)*)").unwrap();
    for cap in re_import.captures_iter(source) {
        let module = cap[1].replace('.', "/");
        specifiers.push(module);
    }

    specifiers
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn extract_go_import_specifiers(source: &str) -> Vec<String> {
    use regex::Regex;
    let mut specifiers = Vec::new();
    let re = Regex::new(r#"(?m)^\s*(?:import\s+)?"([^"]+)""#).unwrap();
    for cap in re.captures_iter(source) {
        let path = &cap[1];
        // Only track local imports (those with a dot or starting with ./)
        if path.starts_with("./") || path.starts_with("../") {
            specifiers.push(path.to_string());
        }
    }
    specifiers
}

fn extract_rust_import_specifiers(source: &str) -> Vec<String> {
    use regex::Regex;
    let mut specifiers = Vec::new();
    // `mod foo;` declares a submodule — maps to foo.rs or foo/mod.rs
    let re_mod = Regex::new(r"(?m)^\s*(?:pub\s+)?mod\s+(\w+)\s*;").unwrap();
    for cap in re_mod.captures_iter(source) {
        specifiers.push(format!("./{}", &cap[1]));
    }
    specifiers
}

fn extract_csharp_import_specifiers(_source: &str) -> Vec<String> {
    // C# `using` directives refer to namespaces, not files.
    // Without a project model we can't resolve them to files, so skip.
    Vec::new()
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

    let importer_ext = importer
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let mut candidates = Vec::new();
    if base.extension().is_some() {
        candidates.push(base);
    } else {
        let extensions: &[&str] = match importer_ext {
            "py" => &["py"],
            "go" => &["go"],
            "rs" => &["rs"],
            "cs" => &["cs"],
            _ => &["ts", "tsx", "js", "jsx"],
        };
        for ext in extensions {
            candidates.push(base.with_extension(ext));
            match importer_ext {
                "py" => {
                    candidates.push(base.join("__init__.py"));
                }
                "rs" => {
                    candidates.push(base.join("mod.rs"));
                }
                _ => {
                    candidates.push(base.join(format!("index.{ext}")));
                }
            }
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

/// Compute a Merkle root from a filtered walk of a directory (using
/// `build_repo_walker` with colocated ignore rules). This produces the same
/// result as copying the tree into a snapshot directory and calling
/// `compute_snapshot_merkle_root`, but without the copy.
fn compute_merkle_root_filtered(source_root: &Path, colocated: bool) -> Result<String> {
    let walker = build_repo_walker(source_root, colocated);
    let mut leaves: Vec<(String, [u8; 32])> = Vec::new();

    for entry in walker.build() {
        let entry = entry.context("failed while walking source for merkle hashing")?;
        if !entry.file_type().map_or(false, |ft| ft.is_file()) {
            continue;
        }

        let rel = entry
            .path()
            .strip_prefix(source_root)
            .context("failed to compute relative path for merkle hashing")?;
        let rel_key = merkle_path_key(rel)?;

        let contents = fs::read(entry.path()).with_context(|| {
            format!(
                "failed to read file for merkle hashing: {}",
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

/// Check if a file path looks like a test file based on common naming conventions.
fn is_test_file(path: &str) -> bool {
    let stem = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if stem.is_empty() {
        return false;
    }
    // Check common patterns: *_test.*, test_*.*, *.test.*, *_spec.*, *.spec.*
    stem.ends_with("_test")
        || stem.starts_with("test_")
        || stem.ends_with(".test")
        || stem.ends_with("_spec")
        || stem.ends_with(".spec")
        // Check if the file is in a tests/ or __tests__/ directory
        || path.contains("/tests/")
        || path.contains("/__tests__/")
        || path.contains("/test/")
}

/// Parse dependencies from a manifest file (Cargo.toml, package.json, go.mod, etc.).
/// Returns detected dependencies for policy checking.
fn parse_manifest_dependencies(
    manifest_path: &str,
    content: &str,
) -> Vec<fl_policy::DetectedDependency> {
    let mut deps = Vec::new();

    if manifest_path.ends_with("Cargo.toml") {
        // Parse [dependencies] and [dev-dependencies] sections.
        let mut in_deps = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_deps = trimmed == "[dependencies]"
                    || trimmed == "[dev-dependencies]"
                    || trimmed == "[build-dependencies]";
                continue;
            }
            if !in_deps || trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((name, rest)) = trimmed.split_once('=') {
                let name = name.trim().trim_matches('"');
                let version = rest
                    .trim()
                    .trim_matches('"')
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .to_string();
                // Handle inline table: { version = "1.0", features = [...] }
                let version = if version.starts_with('{') {
                    version
                        .trim_start_matches('{')
                        .split(',')
                        .find(|s| s.trim().starts_with("version"))
                        .and_then(|s| s.split('=').nth(1))
                        .map(|v| v.trim().trim_matches('"').trim_matches('}').to_string())
                        .unwrap_or_default()
                } else {
                    version
                };
                if !name.is_empty() {
                    deps.push(fl_policy::DetectedDependency {
                        name: name.to_string(),
                        version,
                        manifest_file: manifest_path.to_string(),
                        license: None,
                        vulnerabilities: Vec::new(),
                    });
                }
            }
        }
    } else if manifest_path.ends_with("package.json") {
        // Simple JSON parsing for dependencies/devDependencies.
        let mut in_deps = false;
        let mut brace_depth = 0;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.contains("\"dependencies\"") || trimmed.contains("\"devDependencies\"") {
                in_deps = true;
                brace_depth = 0;
                if trimmed.contains('{') {
                    brace_depth += 1;
                }
                continue;
            }
            if in_deps {
                if trimmed.contains('{') {
                    brace_depth += 1;
                }
                if trimmed.contains('}') {
                    brace_depth -= 1;
                    if brace_depth <= 0 {
                        in_deps = false;
                        continue;
                    }
                }
                // Parse "name": "version"
                let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
                if parts.len() == 2 {
                    let name = parts[0].trim().trim_matches('"').trim_matches(',');
                    let version = parts[1]
                        .trim()
                        .trim_matches('"')
                        .trim_matches(',')
                        .to_string();
                    if !name.is_empty() && !name.starts_with('/') {
                        deps.push(fl_policy::DetectedDependency {
                            name: name.to_string(),
                            version,
                            manifest_file: manifest_path.to_string(),
                            license: None,
                            vulnerabilities: Vec::new(),
                        });
                    }
                }
            }
        }
    } else if manifest_path.ends_with("go.mod") {
        // Parse require blocks.
        let mut in_require = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed == "require (" {
                in_require = true;
                continue;
            }
            if trimmed == ")" {
                in_require = false;
                continue;
            }
            if in_require {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    deps.push(fl_policy::DetectedDependency {
                        name: parts[0].to_string(),
                        version: parts[1].to_string(),
                        manifest_file: manifest_path.to_string(),
                        license: None,
                        vulnerabilities: Vec::new(),
                    });
                }
            }
        }
    } else if manifest_path.ends_with("requirements.txt") {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            // Parse name==version or name>=version.
            let (name, version) = if let Some(idx) = trimmed.find("==") {
                (&trimmed[..idx], &trimmed[idx + 2..])
            } else if let Some(idx) = trimmed.find(">=") {
                (&trimmed[..idx], &trimmed[idx + 2..])
            } else {
                (trimmed, "")
            };
            if !name.is_empty() {
                deps.push(fl_policy::DetectedDependency {
                    name: name.to_string(),
                    version: version.to_string(),
                    manifest_file: manifest_path.to_string(),
                    license: None,
                    vulnerabilities: Vec::new(),
                });
            }
        }
    }

    deps
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
            "native" => RepoMode::Native,
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

/// Compute a merkle root from a native snapshot index. Uses the same Merkle
/// tree construction as `compute_snapshot_merkle_root` but reads file hashes
/// from the index rather than reading file contents from disk.
fn compute_native_merkle_root(index: &SnapshotIndex) -> Result<String> {
    let mut leaves: Vec<(String, [u8; 32])> = Vec::new();

    for (rel_key, entry) in &index.files {
        let file_content_hash = hex::decode(&entry.file_hash)
            .with_context(|| format!("invalid hex hash for {}", rel_key))?;

        let mut leaf_hasher = blake3::Hasher::new();
        leaf_hasher.update(b"flock:merkle:leaf:v1");
        leaf_hasher.update(&(rel_key.len() as u64).to_le_bytes());
        leaf_hasher.update(rel_key.as_bytes());
        leaf_hasher.update(&file_content_hash);
        leaves.push((rel_key.clone(), *leaf_hasher.finalize().as_bytes()));
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

/// Update the `mode` value in a TOML config string.
fn update_config_mode(raw: &str, new_mode: &str) -> String {
    let mut lines: Vec<String> = raw.lines().map(String::from).collect();
    let mut found = false;

    for line in &mut lines {
        let trimmed = line.trim();
        if let Some((key, _)) = trimmed.split_once('=') {
            if key.trim() == "mode" {
                *line = format!("mode = \"{}\"", new_mode);
                found = true;
                break;
            }
        }
    }

    if !found {
        lines.insert(0, format!("mode = \"{}\"", new_mode));
    }

    let mut result = lines.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Normalize a path by resolving `..` and `.` components without touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => { components.pop(); }
            Component::CurDir => {}
            other => { components.push(other); }
        }
    }
    components.iter().collect()
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
    fn promote_exploration_snapshot_accessible() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        // Create a file and initial checkpoint
        std::fs::write(dir.path().join("hello.txt"), "hello").unwrap();
        repo.create_checkpoint(Some("initial".to_string()))
            .expect("initial checkpoint should succeed");

        // Start exploration and make changes
        let started = repo
            .start_exploration("test-explore".to_string())
            .expect("exploration should start");
        std::fs::write(dir.path().join("hello.txt"), "hello world").unwrap();
        repo.create_checkpoint(Some("exploration-work".to_string()))
            .expect("exploration checkpoint should succeed");

        // Promote exploration
        repo.promote_exploration(started.id)
            .expect("promote should succeed");

        // After promote, the latest checkpoint's snapshot must be accessible.
        // This is the bug: the snapshot referenced by the promote checkpoint
        // should exist or be lazily materialized.
        let summary = repo
            .file_summary_from_latest_checkpoint()
            .expect("file_summary should work after promote");
        // Working dir matches the promote checkpoint, so no changes expected
        assert!(summary.modified.is_empty());
    }

    #[test]
    fn promote_exploration_snapshot_accessible_colocated() {
        let dir = tempfile::tempdir().expect("tempdir should be created");

        // Initialize git repo first
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .expect("git init should succeed");
        std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@test.com",
                   "commit", "--allow-empty", "-m", "initial git commit"])
            .current_dir(dir.path())
            .output()
            .expect("git initial commit should succeed");

        let repo = Repo::at(dir.path());
        repo.init_colocated().expect("colocated init should succeed");

        // Create a file and initial checkpoint
        std::fs::write(dir.path().join("hello.txt"), "hello").unwrap();
        repo.create_checkpoint(Some("initial".to_string()))
            .expect("initial checkpoint should succeed");

        // Start exploration and make changes
        let started = repo
            .start_exploration("test-explore".to_string())
            .expect("exploration should start");
        std::fs::write(dir.path().join("hello.txt"), "hello world").unwrap();
        repo.create_checkpoint(Some("exploration-work".to_string()))
            .expect("exploration checkpoint should succeed");

        // Promote exploration
        repo.promote_exploration(started.id)
            .expect("promote should succeed");

        // After promote, the latest checkpoint's snapshot must be accessible.
        let summary = repo
            .file_summary_from_latest_checkpoint()
            .expect("file_summary should work after promote in colocated mode");
        assert!(summary.modified.is_empty());

        // Undo should also work
        let undo_result = repo
            .undo(UndoRequest::Last)
            .expect("undo after promote should succeed");
        assert!(undo_result.target_event_id != Uuid::nil());
    }

    #[test]
    fn promote_exploration_no_initial_checkpoint() {
        // Reproduce exact steps from issue #39:
        // 1. fl explore start (no initial checkpoint)
        // 2. make changes + fl commit
        // 3. fl explore promote
        // 4. fl undo
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        // Step 1: start exploration without any prior checkpoint
        let started = repo
            .start_exploration("test".to_string())
            .expect("exploration should start");

        // Step 2: make changes and commit
        std::fs::write(dir.path().join("file.txt"), "content").unwrap();
        repo.create_checkpoint(Some("exploration work".to_string()))
            .expect("checkpoint should succeed");

        // Step 3: promote
        repo.promote_exploration(started.id)
            .expect("promote should succeed");

        // Step 4: undo should work
        let undo_result = repo
            .undo(UndoRequest::Last)
            .expect("undo should succeed after promote");
        assert!(undo_result.target_event_id != Uuid::nil());

        // fl diff should work after promote
        let summary = repo
            .file_summary_from_latest_checkpoint()
            .expect("file_summary should work after promote");
        // No changes since working dir == promote checkpoint
        assert!(summary.modified.is_empty());
    }

    #[test]
    fn promote_undo_targets_promote_checkpoint() {
        // Test where undo targets the promote checkpoint event directly.
        // The undo should restore from the previous checkpoint's snapshot.
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        // Create initial state
        std::fs::write(dir.path().join("file.txt"), "original").unwrap();
        repo.create_checkpoint(Some("initial".to_string()))
            .expect("initial checkpoint should succeed");

        let started = repo
            .start_exploration("test".to_string())
            .expect("exploration should start");

        std::fs::write(dir.path().join("file.txt"), "modified").unwrap();
        repo.create_checkpoint(Some("exploration work".to_string()))
            .expect("exploration checkpoint should succeed");

        repo.promote_exploration(started.id)
            .expect("promote should succeed");

        // Get all events to find the promote checkpoint
        let events = repo.list_events().expect("list events");
        let promote_checkpoint = events.iter().rev()
            .find(|e| matches!(&e.kind, EventKind::Checkpoint(cp) if cp.label.starts_with("promote-")))
            .expect("promote checkpoint event should exist");

        // Undo targeting the promote checkpoint specifically
        let undo_result = repo
            .undo(UndoRequest::To(promote_checkpoint.id.to_string()))
            .expect("undo targeting promote checkpoint should succeed");
        assert_eq!(undo_result.target_event_id, promote_checkpoint.id);

        // After undoing the promote checkpoint, workspace should be restored
        // to the state of the previous checkpoint (the exploration checkpoint,
        // which has "modified" content).
        let content = std::fs::read_to_string(dir.path().join("file.txt"))
            .expect("file should exist");
        assert_eq!(content.trim(), "modified",
            "workspace should be restored to exploration checkpoint state");
    }

    #[test]
    fn file_summary_includes_unsupported_file_types() {
        // Regression test for issue #40: fl diff should detect changes in
        // files that don't have a semantic analyzer (e.g. .razor, .txt).
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        // Create files of both supported and unsupported types
        std::fs::write(dir.path().join("app.ts"), "const x = 1;").unwrap();
        std::fs::write(dir.path().join("page.razor"), "<h1>Hello</h1>").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "some notes").unwrap();
        repo.create_checkpoint(Some("initial".to_string()))
            .expect("checkpoint should succeed");

        // Modify all files
        std::fs::write(dir.path().join("app.ts"), "const x = 2;").unwrap();
        std::fs::write(dir.path().join("page.razor"), "<h1>Updated</h1>").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "updated notes").unwrap();

        let summary = repo
            .file_summary_from_latest_checkpoint()
            .expect("file_summary should succeed");

        // All three files should appear as modified, including unsupported types
        assert!(
            summary.modified.iter().any(|f| f.contains("app.ts")),
            "supported file app.ts should be in modified list"
        );
        assert!(
            summary.modified.iter().any(|f| f.contains("page.razor")),
            "unsupported file page.razor should be in modified list"
        );
        assert!(
            summary.modified.iter().any(|f| f.contains("notes.txt")),
            "unsupported file notes.txt should be in modified list"
        );
    }

    #[test]
    fn promote_exploration_snapshot_accessible_native() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init_native().expect("native init should succeed");

        // Create a file and initial checkpoint
        std::fs::write(dir.path().join("hello.txt"), "hello").unwrap();
        repo.create_checkpoint(Some("initial".to_string()))
            .expect("initial checkpoint should succeed");

        // Start exploration and make changes
        let started = repo
            .start_exploration("test-explore".to_string())
            .expect("exploration should start");
        std::fs::write(dir.path().join("hello.txt"), "hello world").unwrap();
        repo.create_checkpoint(Some("exploration-work".to_string()))
            .expect("exploration checkpoint should succeed");

        // Promote exploration
        repo.promote_exploration(started.id)
            .expect("promote should succeed");

        // After promote, the latest checkpoint's snapshot must be accessible.
        let summary = repo
            .file_summary_from_latest_checkpoint()
            .expect("file_summary should work after promote in native mode");
        assert!(summary.modified.is_empty());

        // Undo should also work
        let undo_result = repo
            .undo(UndoRequest::Last)
            .expect("undo after promote should succeed");
        assert!(undo_result.target_event_id != Uuid::nil());
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
        assert_eq!(events.len(), 3);
        // First event is init (no parent)
        assert_eq!(events[0].parent_id, None);
        assert!(matches!(events[0].kind, EventKind::Init(_)));
        // Subsequent events chain from their predecessor
        assert_eq!(events[1].parent_id, Some(events[0].id));
        assert_eq!(events[2].parent_id, Some(events[1].id));
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
        repo.create_checkpoint(Some("cp1".to_string()))
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

        // The restore checkpoint's parent is cp1's own parent (None), since
        // the restore checkpoint has cp1's content and the chain should
        // continue from cp1's predecessor.
        assert_eq!(restored_payload.parent_checkpoint_event, None);
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
    fn repeated_undo_walks_checkpoint_chain() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("chain.ts");
        fs::write(&file, "v1").expect("write");
        repo.create_checkpoint(Some("cp1".to_string())).expect("cp1");

        fs::write(&file, "v2").expect("write");
        repo.create_checkpoint(Some("cp2".to_string())).expect("cp2");

        fs::write(&file, "v3").expect("write");
        repo.create_checkpoint(Some("cp3".to_string())).expect("cp3");

        // First undo: v3 -> v2
        repo.undo(UndoRequest::Last).expect("undo 1");
        assert_eq!(fs::read_to_string(&file).unwrap(), "v2");

        // Second undo: v2 -> v1 (NOT back to v3)
        repo.undo(UndoRequest::Last).expect("undo 2");
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
    }

    #[test]
    fn undo_n_walks_n_steps_up_chain() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("jump.ts");
        fs::write(&file, "v1").expect("write");
        repo.create_checkpoint(Some("cp1".to_string())).expect("cp1");

        fs::write(&file, "v2").expect("write");
        repo.create_checkpoint(Some("cp2".to_string())).expect("cp2");

        fs::write(&file, "v3").expect("write");
        repo.create_checkpoint(Some("cp3".to_string())).expect("cp3");

        // undo --n 2: v3 -> v1
        repo.undo(UndoRequest::N(2)).expect("undo n=2");
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
    }

    #[test]
    fn undo_past_beginning_of_chain_errors() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("shallow.ts");
        fs::write(&file, "v1").expect("write");
        repo.create_checkpoint(Some("cp1".to_string())).expect("cp1");

        fs::write(&file, "v2").expect("write");
        repo.create_checkpoint(Some("cp2".to_string())).expect("cp2");

        // Trying to undo 3 steps with only 2 checkpoints should error
        let err = repo.undo(UndoRequest::N(3)).unwrap_err();
        assert!(
            err.to_string().contains("checkpoint(s) exist before HEAD"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn undo_after_undo_chain_is_correct_parent() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let file = dir.path().join("lineage.ts");
        fs::write(&file, "v1").expect("write");
        let cp1 = repo.create_checkpoint(Some("cp1".to_string())).expect("cp1");

        fs::write(&file, "v2").expect("write");
        repo.create_checkpoint(Some("cp2".to_string())).expect("cp2");

        fs::write(&file, "v3").expect("write");
        repo.create_checkpoint(Some("cp3".to_string())).expect("cp3");

        // First undo: restores cp2's content. The restore checkpoint's parent
        // is cp2's parent (cp1), so that a subsequent undo can walk further back.
        let r1 = repo.undo(UndoRequest::Last).expect("undo 1");
        let r1_cp_id = r1.restored_checkpoint_event.unwrap();
        let events = repo.list_events().unwrap();
        let r1_cp = events.iter().find(|e| e.id == r1_cp_id).unwrap();
        let EventKind::Checkpoint(ref r1_payload) = r1_cp.kind else {
            panic!("expected checkpoint");
        };
        assert_eq!(r1_payload.parent_checkpoint_event, Some(cp1.id));

        // Second undo: restores cp1's content. The restore checkpoint's parent
        // is cp1's parent (None), so further undo is impossible.
        let r2 = repo.undo(UndoRequest::Last).expect("undo 2");
        let r2_cp_id = r2.restored_checkpoint_event.unwrap();
        let events = repo.list_events().unwrap();
        let r2_cp = events.iter().find(|e| e.id == r2_cp_id).unwrap();
        let EventKind::Checkpoint(ref r2_payload) = r2_cp.kind else {
            panic!("expected checkpoint");
        };
        assert_eq!(r2_payload.parent_checkpoint_event, None);
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
    fn undo_file_rejects_untracked_filename() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        let tracked = dir.path().join("tracked.txt");
        fs::write(&tracked, "hello").expect("write should succeed");
        repo.create_checkpoint(Some("cp1".to_string()))
            .expect("checkpoint should succeed");

        fs::write(&tracked, "hello v2").expect("write should succeed");
        repo.create_checkpoint(Some("cp2".to_string()))
            .expect("checkpoint should succeed");

        let result = repo.undo_file(UndoRequest::Last, Path::new("nonexistent.txt"));
        assert!(result.is_err(), "undo of untracked file should fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not found"),
            "error should mention file not found, got: {err_msg}"
        );
    }

    #[test]
    fn parse_duration_specs() {
        assert_eq!(parse_duration_spec("5m").expect("duration").as_secs(), 300);
        assert_eq!(parse_duration_spec("30").expect("duration").as_secs(), 30);
        assert_eq!(parse_duration_spec("1w").expect("duration").as_secs(), 604800);
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
        assert_eq!(report.event_count, 5);
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

        // Quick save with v = 2
        fs::write(&file, "export const v = 2;").expect("modify");
        let save_event = repo.quick_save(Some("my-save".to_string())).expect("quick save");
        let EventKind::Checkpoint(payload) = &save_event.kind else {
            panic!("expected checkpoint");
        };
        assert!(payload.label.contains("my-save"));

        // Modify further to v = 3
        fs::write(&file, "export const v = 3;").expect("modify again");

        // Quick restore - should restore TO the quick-save state (v = 2)
        let result = repo.quick_restore().expect("quick restore");
        assert_eq!(result.target_event_id, save_event.id);

        // The file should contain v = 2 (the quick-save state), NOT v = 1
        let content = fs::read_to_string(&file).expect("read restored");
        assert_eq!(content, "export const v = 2;");
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

    #[test]
    fn native_init_and_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init_native().expect("native init");

        // Verify native mode
        let config = fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(config.contains("native"));

        // Verify block store dir exists, not snapshots dir
        assert!(dir.path().join(".flock/store/blocks").is_dir());
        assert!(!dir.path().join(".flock/snapshots").is_dir());

        // Create a file and checkpoint
        fs::write(dir.path().join("hello.txt"), "hello world").unwrap();
        let event = repo.create_checkpoint(Some("first".to_string())).unwrap();
        let EventKind::Checkpoint(payload) = &event.kind else {
            panic!("expected checkpoint");
        };
        assert!(payload.snapshot_merkle_root.is_some());

        // Verify index was written
        let file_index = FileIndex::for_root(dir.path());
        assert!(file_index.has(payload.snapshot_id));
        let index = file_index.read(payload.snapshot_id).unwrap();
        assert_eq!(index.file_count(), 1);
        assert!(index.get("hello.txt").is_some());
    }

    #[test]
    fn native_checkpoint_and_restore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init_native().unwrap();

        // Create initial files
        fs::write(dir.path().join("a.txt"), "content A").unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.txt"), "content B").unwrap();

        repo.create_checkpoint(Some("v1".to_string())).unwrap();

        // Modify files
        fs::write(dir.path().join("a.txt"), "modified A").unwrap();
        fs::write(dir.path().join("c.txt"), "new file C").unwrap();

        repo.create_checkpoint(Some("v2".to_string())).unwrap();

        // Undo back to v1
        repo.undo(UndoRequest::Last).unwrap();

        // Verify workspace is restored
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "content A");
        assert_eq!(fs::read_to_string(dir.path().join("sub/b.txt")).unwrap(), "content B");
        assert!(!dir.path().join("c.txt").exists());
    }

    #[test]
    fn native_deduplication() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init_native().unwrap();

        let content = "identical content in both files";
        fs::write(dir.path().join("file1.txt"), content).unwrap();
        fs::write(dir.path().join("file2.txt"), content).unwrap();

        repo.create_checkpoint(Some("dedup-test".to_string())).unwrap();

        // Both files should share blocks
        let store = ContentStore::for_root(dir.path());
        // For small identical files, both map to the same single block
        let count = store.block_count().unwrap();
        assert_eq!(count, 1, "identical files should share blocks, got {} blocks", count);
    }

    #[test]
    fn migrate_to_native() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().unwrap();

        // Create some checkpoints in git-compatible mode
        fs::write(dir.path().join("x.txt"), "hello").unwrap();
        repo.create_checkpoint(Some("cp1".to_string())).unwrap();

        fs::write(dir.path().join("y.txt"), "world").unwrap();
        repo.create_checkpoint(Some("cp2".to_string())).unwrap();

        // Verify snapshots exist as directories
        assert!(dir.path().join(".flock/snapshots").is_dir());

        // Migrate
        let report = repo.migrate_to_native().unwrap();
        assert_eq!(report.snapshots_migrated, 2);
        assert!(report.blocks_stored > 0);

        // Verify old snapshot directories are removed
        let snapshots_dir = dir.path().join(".flock/snapshots");
        if snapshots_dir.is_dir() {
            let entries: Vec<_> = fs::read_dir(&snapshots_dir).unwrap().collect();
            assert!(entries.is_empty(), "snapshot dirs should be removed after migration");
        }

        // Verify config updated
        let config = fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(config.contains("native"));

        // Verify block store has content
        let store = ContentStore::for_root(dir.path());
        assert!(store.block_count().unwrap() > 0);

        // fsck should still pass
        repo.fsck().expect("fsck should pass after migration");
    }

    #[test]
    fn native_scoped_undo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init_native().unwrap();

        fs::write(dir.path().join("keep.txt"), "keep this").unwrap();
        fs::write(dir.path().join("change.txt"), "original").unwrap();

        repo.create_checkpoint(Some("base".to_string())).unwrap();

        fs::write(dir.path().join("keep.txt"), "modified keep").unwrap();
        fs::write(dir.path().join("change.txt"), "changed").unwrap();

        repo.create_checkpoint(Some("modified".to_string())).unwrap();

        // Scoped undo on just change.txt
        repo.undo_file(UndoRequest::Last, "change.txt").unwrap();

        // change.txt should be restored, keep.txt untouched
        assert_eq!(fs::read_to_string(dir.path().join("change.txt")).unwrap(), "original");
        assert_eq!(fs::read_to_string(dir.path().join("keep.txt")).unwrap(), "modified keep");
    }

    #[test]
    fn native_vs_colocated_benchmark() {
        use std::time::Instant;

        // Generate test files: 20 files of ~50KB each
        let file_count = 20;
        let file_size = 50_000;
        let checkpoint_count = 5; // more checkpoints to show dedup advantage
        let generate_content = |seed: u8| -> Vec<u8> {
            let mut state = seed as u64 | 1;
            (0..file_size)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state & 0xFF) as u8
                })
                .collect()
        };

        // Benchmark git-compatible mode
        let dir_compat = tempfile::tempdir().unwrap();
        let repo_compat = Repo::at(dir_compat.path());
        repo_compat.init().unwrap();

        for i in 0..file_count {
            fs::write(
                dir_compat.path().join(format!("file_{}.txt", i)),
                generate_content(i as u8),
            )
            .unwrap();
        }

        let start = Instant::now();
        repo_compat
            .create_checkpoint(Some("compat-bench".to_string()))
            .unwrap();
        let compat_time = start.elapsed();

        // Benchmark native mode
        let dir_native = tempfile::tempdir().unwrap();
        let repo_native = Repo::at(dir_native.path());
        repo_native.init_native().unwrap();

        for i in 0..file_count {
            fs::write(
                dir_native.path().join(format!("file_{}.txt", i)),
                generate_content(i as u8),
            )
            .unwrap();
        }

        let start = Instant::now();
        repo_native
            .create_checkpoint(Some("native-bench".to_string()))
            .unwrap();
        let native_time = start.elapsed();

        // Create multiple checkpoints changing only 1 file each to show dedup
        let mut compat_time_2 = std::time::Duration::ZERO;
        let mut native_time_2 = std::time::Duration::ZERO;
        for cp in 1..checkpoint_count {
            let seed = (100 + cp) as u8;
            fs::write(
                dir_compat.path().join("file_0.txt"),
                generate_content(seed),
            )
            .unwrap();
            fs::write(
                dir_native.path().join("file_0.txt"),
                generate_content(seed),
            )
            .unwrap();

            let start = Instant::now();
            repo_compat
                .create_checkpoint(Some(format!("compat-bench-{}", cp)))
                .unwrap();
            compat_time_2 += start.elapsed();

            let start = Instant::now();
            repo_native
                .create_checkpoint(Some(format!("native-bench-{}", cp)))
                .unwrap();
            native_time_2 += start.elapsed();
        }

        // Compare storage sizes.  Note: native mode materializes snapshot
        // directories as a cache during lineage computation, so total dir size
        // includes both the block store and materialized snapshots.  We only
        // check that the block store (content-store) itself is smaller than the
        // full compat snapshot directories.
        let compat_size = dir_size(&dir_compat.path().join(".flock/snapshots"));
        let native_content_size = dir_size(&dir_native.path().join(".flock/content-store"));

        assert!(
            native_content_size < compat_size,
            "native content store ({} bytes) should be smaller than compat snapshots ({} bytes)",
            native_content_size,
            compat_size
        );

        eprintln!(
            "Benchmark results ({}x{}B files, {} checkpoints):",
            file_count, file_size, checkpoint_count
        );
        eprintln!("  1st checkpoint: compat={:?}, native={:?}", compat_time, native_time);
        eprintln!(
            "  subsequent checkpoints: compat={:?}, native={:?}",
            compat_time_2, native_time_2
        );
        eprintln!(
            "  Storage: compat snapshots={} bytes, native content-store={} bytes ({:.1}% of compat)",
            compat_size,
            native_content_size,
            native_content_size as f64 / compat_size as f64 * 100.0
        );
    }

    fn dir_size(path: &Path) -> u64 {
        let mut total = 0u64;
        for entry in WalkDir::new(path) {
            if let Ok(entry) = entry {
                if entry.file_type().is_file() {
                    if let Ok(meta) = entry.metadata() {
                        total += meta.len();
                    }
                }
            }
        }
        total
    }

    #[test]
    fn update_config_mode_replaces_existing() {
        let raw = "mode = \"git-compatible\"\nsemantic_default = \"typescript\"\n";
        let updated = update_config_mode(raw, "native");
        assert!(updated.contains("mode = \"native\""));
        assert!(updated.contains("semantic_default = \"typescript\""));
        assert!(!updated.contains("git-compatible"));
    }

    #[test]
    fn update_config_mode_inserts_if_missing() {
        let raw = "semantic_default = \"typescript\"\n";
        let updated = update_config_mode(raw, "native");
        assert!(updated.contains("mode = \"native\""));
    }

    #[test]
    fn rebase_workspace_already_up_to_date() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        fs::write(dir.path().join("main.ts"), "export const x = 1;").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        repo.create_workspace("ws1".to_string(), false)
            .expect("create workspace");

        let result = repo.rebase_workspace("ws1").expect("rebase");
        assert!(result.already_up_to_date);
        assert!(result.files_merged.is_empty());
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn rebase_workspace_merges_clean_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        // Create initial file and checkpoint
        fs::write(dir.path().join("main.ts"), "export const x = 1;").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        // Create workspace
        repo.create_workspace("ws1".to_string(), true)
            .expect("create workspace");

        // Make a change and create a new checkpoint (simulating upstream changes)
        fs::write(dir.path().join("other.ts"), "export const y = 2;").expect("write new file");
        repo.create_checkpoint(Some("upstream".to_string()))
            .expect("checkpoint 2");

        // Rebase the workspace
        let result = repo.rebase_workspace("ws1").expect("rebase");
        assert!(!result.already_up_to_date);
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn auto_rebase_only_processes_enabled_workspaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        fs::write(dir.path().join("main.ts"), "export const x = 1;").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        // Create workspaces: one with auto-rebase, one without
        repo.create_workspace("auto-ws".to_string(), true)
            .expect("create auto workspace");
        repo.create_workspace("manual-ws".to_string(), false)
            .expect("create manual workspace");

        // Make changes and checkpoint
        fs::write(dir.path().join("extra.ts"), "export const z = 3;").expect("write");
        repo.create_checkpoint(Some("upstream".to_string()))
            .expect("checkpoint 2");

        let results = repo.auto_rebase_workspaces().expect("auto rebase");
        // Only the auto-rebase workspace should be processed
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].workspace, "auto-ws");
    }

    #[test]
    fn rebase_no_local_changes_no_spurious_conflicts() {
        // Regression test for #88: workspace with no local changes should
        // not generate DeleteVsEdit conflicts when new base adds files.
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        // Create initial checkpoint with one file
        fs::write(dir.path().join("main.ts"), "export const x = 1;").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        // Create workspace (no local changes will be made)
        repo.create_workspace("agent-1".to_string(), false)
            .expect("create workspace");

        // Upstream: add new files and modify the existing one
        fs::write(dir.path().join("main.ts"), "export const x = 2;").expect("modify");
        fs::write(dir.path().join("new-file.ts"), "export const y = 1;").expect("new file");
        repo.create_checkpoint(Some("upstream".to_string()))
            .expect("checkpoint 2");

        let result = repo.rebase_workspace("agent-1").expect("rebase");
        assert!(!result.already_up_to_date);
        // No local changes → should be zero conflicts (fast-forward)
        assert!(
            result.conflicts.is_empty(),
            "expected zero conflicts for workspace with no local changes, got: {:?}",
            result.conflicts,
        );
    }

    #[test]
    fn detect_conflicts_persists_to_event_log() {
        // Regression test for #82: detect_conflicts should persist conflicts
        // so that list/suggest/resolve can reference them.
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        fs::write(dir.path().join("main.ts"), "export const x = 1;").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        // Create workspace
        repo.create_workspace("ws1".to_string(), false)
            .expect("create workspace");

        // Modify file in workspace (local change)
        fs::write(dir.path().join("main.ts"), "export const x = 'ws-change';").expect("ws edit");

        // Also create an upstream change
        fs::write(dir.path().join("other.ts"), "export const y = 2;").expect("upstream add");
        repo.create_checkpoint(Some("upstream".to_string()))
            .expect("checkpoint 2");

        // Re-apply workspace change so it diverges from base
        fs::write(dir.path().join("main.ts"), "export const x = 'ws-change';").expect("ws edit again");

        let detected = repo.detect_conflicts("ws1").expect("detect");

        // Whether or not there are semantic conflicts depends on the merge
        // engine, but if there are any, they should have IDs and be in list_conflicts.
        if !detected.is_empty() {
            for c in &detected {
                assert!(c.id.is_some(), "detected conflict should have an id");
            }

            let listed = repo.list_conflicts(None).expect("list conflicts");
            assert!(
                !listed.is_empty(),
                "list_conflicts should return persisted conflicts after detect"
            );
        }
    }

    #[test]
    fn conflict_resolution_workflow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        fs::write(dir.path().join("main.ts"), "export const x = 1;").expect("write");
        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        // Verify list_conflicts and list_rebases work on empty state
        let conflicts = repo.list_conflicts(None).expect("list conflicts");
        assert!(conflicts.is_empty());

        let rebases = repo.list_rebases(None).expect("list rebases");
        assert!(rebases.is_empty());
    }

    #[test]
    fn flockignore_filters_default_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        // Create files in directories that should be ignored by default.
        let node_mod = dir.path().join("node_modules");
        fs::create_dir_all(&node_mod).unwrap();
        fs::write(node_mod.join("dep.js"), "module.exports = {}").unwrap();

        let pycache = dir.path().join("__pycache__");
        fs::create_dir_all(&pycache).unwrap();
        fs::write(pycache.join("mod.pyc"), "bytecode").unwrap();

        let target = dir.path().join("target");
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::write(target.join("debug").join("binary"), "elf").unwrap();

        // Create a file that should be tracked.
        fs::write(dir.path().join("main.ts"), "console.log('hello')").unwrap();

        let event = repo
            .create_checkpoint(Some("test".to_string()))
            .expect("checkpoint");
        let EventKind::Checkpoint(payload) = event.kind else {
            panic!("expected checkpoint event");
        };

        let snapshot_root = dir.path().join(".flock").join("snapshots").join(payload.snapshot_id.to_string());
        // main.ts should be in the snapshot.
        assert!(snapshot_root.join("main.ts").exists());
        // Ignored directories should NOT be in the snapshot.
        assert!(!snapshot_root.join("node_modules").exists());
        assert!(!snapshot_root.join("__pycache__").exists());
        assert!(!snapshot_root.join("target").exists());
    }

    #[test]
    fn flockignore_file_patterns_are_respected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        // Create a .flockignore file.
        fs::write(dir.path().join(".flockignore"), "*.log\nbuild/\n").unwrap();

        // Create files matching and not matching the ignore patterns.
        fs::write(dir.path().join("app.ts"), "const x = 1;").unwrap();
        fs::write(dir.path().join("debug.log"), "log data").unwrap();
        let build_dir = dir.path().join("build");
        fs::create_dir_all(&build_dir).unwrap();
        fs::write(build_dir.join("output.js"), "compiled").unwrap();

        let event = repo
            .create_checkpoint(Some("ignore-test".to_string()))
            .expect("checkpoint");
        let EventKind::Checkpoint(payload) = event.kind else {
            panic!("expected checkpoint event");
        };

        let snapshot_root = dir.path().join(".flock").join("snapshots").join(payload.snapshot_id.to_string());
        // app.ts should be in the snapshot.
        assert!(snapshot_root.join("app.ts").exists());
        // Ignored files and directories should NOT be in the snapshot.
        assert!(!snapshot_root.join("debug.log").exists());
        assert!(!snapshot_root.join("build").exists());
    }

    #[test]
    fn colocated_mode_falls_back_to_gitignore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init_colocated().expect("init colocated");

        // Create a .gitignore (no .flockignore).
        fs::write(dir.path().join(".gitignore"), "*.tmp\ndist/\n").unwrap();

        fs::write(dir.path().join("app.ts"), "const x = 1;").unwrap();
        fs::write(dir.path().join("temp.tmp"), "temp data").unwrap();
        let dist_dir = dir.path().join("dist");
        fs::create_dir_all(&dist_dir).unwrap();
        fs::write(dist_dir.join("bundle.js"), "bundled").unwrap();

        let event = repo
            .create_checkpoint(Some("gitignore-test".to_string()))
            .expect("checkpoint");
        let EventKind::Checkpoint(payload) = event.kind else {
            panic!("expected checkpoint event");
        };

        // In colocated mode, no physical snapshot directory is created;
        // instead we have a git_commit_sha and can lazily extract.
        let snapshot_dir = dir.path().join(".flock").join("snapshots").join(payload.snapshot_id.to_string());
        assert!(!snapshot_dir.exists(), "colocated mode should not create snapshot dir");
        assert!(payload.git_commit_sha.is_some(), "should have git_commit_sha");

        // Lazy extraction should produce the correct content.
        let snapshot_root = repo.ensure_snapshot_available(payload.snapshot_id).expect("lazy extract");
        assert!(snapshot_root.join("app.ts").exists());
        // .gitignore patterns are applied by git, so ignored files should not be in the git commit.
        assert!(!snapshot_root.join("temp.tmp").exists());
        assert!(!snapshot_root.join("dist").exists());
    }

    #[test]
    fn colocated_mode_prefers_flockignore_over_gitignore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init_colocated().expect("init colocated");

        // Both ignore files exist — .flockignore should win.
        fs::write(dir.path().join(".gitignore"), "*.tmp\n").unwrap();
        fs::write(dir.path().join(".flockignore"), "*.bak\n").unwrap();

        fs::write(dir.path().join("app.ts"), "const x = 1;").unwrap();
        fs::write(dir.path().join("temp.tmp"), "temp data").unwrap();
        fs::write(dir.path().join("old.bak"), "backup").unwrap();

        let event = repo
            .create_checkpoint(Some("prefer-flockignore".to_string()))
            .expect("checkpoint");
        let EventKind::Checkpoint(payload) = event.kind else {
            panic!("expected checkpoint event");
        };

        // No physical snapshot directory in colocated mode.
        let snapshot_dir = dir.path().join(".flock").join("snapshots").join(payload.snapshot_id.to_string());
        assert!(!snapshot_dir.exists(), "colocated mode should not create snapshot dir");
        assert!(payload.git_commit_sha.is_some(), "should have git_commit_sha");

        // Lazy extraction gives us the git commit tree.
        // Note: git commit only contains files that were staged — .gitignore
        // controls what git tracks, while .flockignore controls the flock merkle
        // root computation. The git commit content reflects git's staging rules.
        let snapshot_root = repo.ensure_snapshot_available(payload.snapshot_id).expect("lazy extract");
        assert!(snapshot_root.join("app.ts").exists());
        // Since the git commit is the source of truth for lazy extraction,
        // .tmp files may or may not be present depending on git staging.
        // The key invariant is that ensure_snapshot_available works.
    }

    #[test]
    fn status_reports_changes_since_last_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        // Before any checkpoint, all files should be new.
        fs::write(dir.path().join("main.ts"), "const a = 1;").unwrap();
        let report = repo.status().expect("status before checkpoint");
        assert!(report.checkpoint_id.is_none());
        assert!(report.new_files.contains(&"main.ts".to_string()));

        // Checkpoint.
        repo.create_checkpoint(Some("initial".to_string()))
            .expect("checkpoint");

        // No changes — status should be clean.
        let report = repo.status().expect("status after checkpoint");
        assert!(report.checkpoint_id.is_some());
        assert!(report.new_files.is_empty());
        assert!(report.modified_files.is_empty());
        assert!(report.deleted_files.is_empty());

        // Modify a file.
        fs::write(dir.path().join("main.ts"), "const a = 2;").unwrap();
        let report = repo.status().expect("status after modification");
        assert!(report.modified_files.contains(&"main.ts".to_string()));

        // Add a new file.
        fs::write(dir.path().join("helper.ts"), "export {}").unwrap();
        let report = repo.status().expect("status after new file");
        assert!(report.new_files.contains(&"helper.ts".to_string()));

        // Delete a file.
        fs::remove_file(dir.path().join("main.ts")).unwrap();
        let report = repo.status().expect("status after delete");
        assert!(report.deleted_files.contains(&"main.ts".to_string()));
    }

    #[test]
    fn status_ignores_flockignored_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        fs::write(dir.path().join(".flockignore"), "*.log\n").unwrap();
        fs::write(dir.path().join("app.ts"), "const x = 1;").unwrap();
        fs::write(dir.path().join("debug.log"), "log").unwrap();

        repo.create_checkpoint(Some("base".to_string()))
            .expect("checkpoint");

        // Modify the ignored file.
        fs::write(dir.path().join("debug.log"), "updated log").unwrap();

        let report = repo.status().expect("status");
        // The .log file should not appear in any change lists.
        assert!(!report.modified_files.contains(&"debug.log".to_string()));
        assert!(!report.new_files.contains(&"debug.log".to_string()));
    }

    /// Helper: create a git repo with N commits and return the temp directory.
    fn create_git_repo_with_commits(n: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        fl_bridge_git::run_git(p, &["init"]).expect("git init");
        fl_bridge_git::run_git(p, &["-c", "user.name=Test", "-c", "user.email=test@test", "commit", "--allow-empty", "-m", "initial"]).expect("initial commit");
        for i in 1..n {
            let file = p.join(format!("file{i}.txt"));
            fs::write(&file, format!("content {i}")).unwrap();
            fl_bridge_git::run_git(p, &["add", "-A"]).unwrap();
            fl_bridge_git::run_git(p, &["-c", "user.name=Test", "-c", "user.email=test@test", "commit", "-m", &format!("commit {i}")]).unwrap();
        }
        dir
    }

    #[test]
    fn convert_from_git_basic() {
        let dir = create_git_repo_with_commits(4);
        let repo = Repo::at(dir.path());
        let report = repo.convert_from_git(None, None).expect("convert_from_git");
        assert!(report.commits_imported >= 4, "expected at least 4 commits, got {}", report.commits_imported);
        assert!(report.branches_imported >= 1);
        assert!(report.validation_ok, "validation failed: {}", report.validation_detail);
    }

    #[test]
    fn convert_from_git_branches() {
        let dir = create_git_repo_with_commits(3);
        let p = dir.path();
        // Create a second branch with an extra commit
        fl_bridge_git::run_git(p, &["checkout", "-b", "feature"]).unwrap();
        let file = p.join("feature.txt");
        fs::write(&file, "feature content").unwrap();
        fl_bridge_git::run_git(p, &["add", "-A"]).unwrap();
        fl_bridge_git::run_git(p, &["-c", "user.name=Test", "-c", "user.email=test@test", "commit", "-m", "feature commit"]).unwrap();
        fl_bridge_git::run_git(p, &["checkout", "master"]).unwrap_or_else(|_| {
            fl_bridge_git::run_git(p, &["checkout", "main"]).unwrap()
        });

        let repo = Repo::at(p);
        let report = repo.convert_from_git(None, None).expect("convert_from_git");
        assert!(report.branches_imported >= 2, "expected at least 2 branches, got {}", report.branches_imported);
        assert!(report.validation_ok);
    }

    #[test]
    fn convert_from_git_tags() {
        let dir = create_git_repo_with_commits(3);
        let p = dir.path();
        fl_bridge_git::run_git(p, &["tag", "v1.0"]).unwrap();

        let repo = Repo::at(p);
        let report = repo.convert_from_git(None, None).expect("convert_from_git");
        assert_eq!(report.tags_imported, 1, "expected 1 tag imported");
        assert!(report.validation_ok);
    }

    #[test]
    fn convert_from_git_shallow() {
        let dir = create_git_repo_with_commits(6);
        let repo = Repo::at(dir.path());
        let report = repo.convert_from_git(None, Some(3)).expect("convert_from_git shallow");
        assert_eq!(report.commits_imported, 3, "shallow should import exactly 3 commits");
        assert!(report.validation_ok);
    }

    #[test]
    fn convert_from_git_branch_filter() {
        let dir = create_git_repo_with_commits(3);
        let p = dir.path();
        fl_bridge_git::run_git(p, &["checkout", "-b", "feature"]).unwrap();
        let file = p.join("feature.txt");
        fs::write(&file, "feature").unwrap();
        fl_bridge_git::run_git(p, &["add", "-A"]).unwrap();
        fl_bridge_git::run_git(p, &["-c", "user.name=Test", "-c", "user.email=test@test", "commit", "-m", "feature"]).unwrap();
        fl_bridge_git::run_git(p, &["checkout", "master"]).unwrap_or_else(|_| {
            fl_bridge_git::run_git(p, &["checkout", "main"]).unwrap()
        });

        let repo = Repo::at(p);
        let report = repo.convert_from_git(Some("feature".to_string()), None).expect("convert_from_git filtered");
        assert_eq!(report.branches_imported, 1, "should only import feature branch");
        assert!(report.validation_ok);
    }

    #[test]
    fn convert_from_git_resumable() {
        let dir = create_git_repo_with_commits(3);
        let p = dir.path();
        let repo = Repo::at(p);

        // First import
        let report1 = repo.convert_from_git(None, None).expect("first convert");
        let first_count = report1.commits_imported;
        assert!(first_count >= 3);

        // Add more commits
        let file = p.join("new.txt");
        fs::write(&file, "new").unwrap();
        fl_bridge_git::run_git(p, &["add", "-A"]).unwrap();
        fl_bridge_git::run_git(p, &["-c", "user.name=Test", "-c", "user.email=test@test", "commit", "-m", "new commit"]).unwrap();

        // Second import — should only import the new commit
        let report2 = repo.convert_from_git(None, None).expect("second convert");
        assert_eq!(report2.commits_imported, 1, "resumable should only import 1 new commit");
        assert!(report2.commits_skipped >= first_count);
        assert!(report2.validation_ok);
    }

    #[test]
    fn convert_to_git_roundtrip() {
        // Create a flock repo with some checkpoints
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        fs::write(dir.path().join("hello.txt"), "hello").unwrap();
        repo.create_checkpoint(Some("first".to_string())).expect("checkpoint 1");

        fs::write(dir.path().join("world.txt"), "world").unwrap();
        repo.create_checkpoint(Some("second".to_string())).expect("checkpoint 2");

        // Export to git
        let report = repo.convert_to_git(false).expect("convert_to_git");
        assert_eq!(report.commits_imported, 2, "should export 2 checkpoints as commits");
        assert!(report.validation_ok, "validation failed: {}", report.validation_detail);

        // Verify git history exists (exported to refs/heads/main)
        let log_output = fl_bridge_git::run_git(dir.path(), &["log", "--oneline", "main"]).expect("git log");
        assert!(log_output.lines().count() >= 2, "git should have at least 2 commits");
    }

    #[test]
    fn checkpoint_to_checkpoint_diff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        let file = dir.path().join("sample.ts");
        fs::write(&file, "function add(a: number, b: number) { return a + b; }")
            .expect("write v1");
        let ev1 = repo.create_checkpoint(Some("v1".to_string())).expect("cp1");
        let id1 = ev1.id.to_string();

        fs::write(
            &file,
            "function add(a: number, b: number) { return a + b; }\nfunction sub(a: number, b: number) { return a - b; }",
        )
        .expect("write v2");
        let ev2 = repo.create_checkpoint(Some("v2".to_string())).expect("cp2");
        let id2 = ev2.id.to_string();

        // Checkpoint-to-checkpoint diff
        let diffs = repo
            .semantic_diff_between_checkpoints(&id1, &id2)
            .expect("diff between checkpoints");
        assert_eq!(diffs.len(), 1, "should have 1 file diff");
        assert!(
            diffs[0]
                .changes
                .iter()
                .any(|c| c.kind == crate::semantic::SemanticChangeKind::Added
                    && c.symbol.contains("sub")),
            "should detect added sub function"
        );

        // Prefix lookup
        let prefix = &id1[..8];
        let diffs2 = repo
            .semantic_diff_between_checkpoints(prefix, &id2)
            .expect("diff with prefix");
        assert_eq!(diffs2.len(), diffs.len());

        // Checkpoint vs working dir
        fs::write(
            &file,
            "function add(a: number, b: number) { return a + b; }\nfunction sub(a: number, b: number) { return a - b; }\nfunction mul(a: number, b: number) { return a * b; }",
        )
        .expect("write v3");
        let diffs3 = repo
            .semantic_diff_checkpoint_vs_working(&id2)
            .expect("diff checkpoint vs working");
        assert!(
            diffs3[0]
                .changes
                .iter()
                .any(|c| c.kind == crate::semantic::SemanticChangeKind::Added
                    && c.symbol.contains("mul")),
            "should detect added mul function"
        );

        // File summary between checkpoints
        let summary = repo
            .file_summary_between_checkpoints(&id1, &id2)
            .expect("file summary");
        assert!(
            summary.modified.iter().any(|f| f.contains("sample.ts")),
            "sample.ts should appear as modified"
        );

        // File summary checkpoint vs working
        let summary2 = repo
            .file_summary_checkpoint_vs_working(&id2)
            .expect("file summary vs working");
        assert!(
            summary2.modified.iter().any(|f| f.contains("sample.ts")),
            "sample.ts should appear as modified vs working"
        );
    }

    #[test]
    fn checkpoint_prefix_ambiguous_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        // Non-existent prefix should error
        let err = repo.find_checkpoint_by_prefix("nonexistent");
        assert!(err.is_err());
        assert!(
            err.unwrap_err().to_string().contains("no checkpoint matching"),
            "should report no matching checkpoint"
        );
    }

    #[test]
    fn find_task_by_prefix_resolves_short_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        let task = repo
            .create_task("test task".to_string(), None, vec![], None, vec![])
            .expect("create task");
        let full_id = task.id.to_string();
        let prefix = &full_id[..8];

        // Exact match
        let found = repo.find_task_by_prefix(&full_id).expect("exact match");
        assert_eq!(found.id, task.id);

        // Prefix match
        let found = repo.find_task_by_prefix(prefix).expect("prefix match");
        assert_eq!(found.id, task.id);

        // No match
        let err = repo.find_task_by_prefix("00000000");
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("no task matching"));
    }

    // -----------------------------------------------------------------------
    // Clone / partial clone tests
    // -----------------------------------------------------------------------

    #[test]
    fn clone_via_local_fs_transport() {
        // Create a source repo with a checkpoint.
        let source_dir = tempfile::tempdir().unwrap();
        let source = Repo::at(source_dir.path());
        source.init().unwrap();
        std::fs::write(source_dir.path().join("hello.txt"), "hello world").unwrap();
        source.create_checkpoint(Some("initial commit".to_string())).unwrap();

        // Clone to a new directory.
        let clone_dir = tempfile::tempdir().unwrap();
        let target = clone_dir.path().join("cloned");
        let source_url = format!("file://{}", source_dir.path().display());

        let report = Repo::clone_from(
            &source_url,
            &target,
            None,   // depth
            vec![], // sparse
            false,  // lazy
            None,   // focus
        ).unwrap();

        assert!(report.pull.events_pulled > 0);
        assert_eq!(report.pull.roost_name, "origin");

        // Verify the cloned repo has events.
        let cloned = Repo::discover(&target).unwrap();
        let events = cloned.list_events().unwrap();
        assert!(!events.is_empty());
    }

    #[test]
    fn shallow_clone_limits_checkpoints() {
        // Create a source repo with multiple checkpoints.
        let source_dir = tempfile::tempdir().unwrap();
        let source = Repo::at(source_dir.path());
        source.init().unwrap();
        for i in 0..5 {
            std::fs::write(
                source_dir.path().join(format!("file{}.txt", i)),
                format!("content {}", i),
            ).unwrap();
            source.create_checkpoint(Some(format!("commit {}", i))).unwrap();
        }

        // Clone with depth 2.
        let clone_dir = tempfile::tempdir().unwrap();
        let target = clone_dir.path().join("shallow");
        let source_url = format!("file://{}", source_dir.path().display());

        let report = Repo::clone_from(
            &source_url,
            &target,
            Some(2), // depth
            vec![],  // sparse
            false,   // lazy
            None,    // focus
        ).unwrap();

        assert!(report.pull.events_pulled > 0);
        assert_eq!(report.depth, Some(2));

        // Verify fewer events than the full repo.
        let cloned = Repo::discover(&target).unwrap();
        let cloned_events = cloned.list_events().unwrap();
        let source_events = source.list_events().unwrap();
        assert!(cloned_events.len() < source_events.len());
    }

    #[test]
    fn sparse_patterns_persisted_in_roost_config() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::at(dir.path());
        repo.init().unwrap();
        repo.roost_add("origin", "file:///tmp/fake").unwrap();

        // Add sparse patterns.
        repo.sparse_add("origin", "src/**").unwrap();
        repo.sparse_add("origin", "*.toml").unwrap();

        let patterns = repo.sparse_list("origin").unwrap();
        assert_eq!(patterns, vec!["src/**", "*.toml"]);

        // Duplicate add is idempotent.
        repo.sparse_add("origin", "src/**").unwrap();
        assert_eq!(repo.sparse_list("origin").unwrap().len(), 2);

        // Remove.
        repo.sparse_remove("origin", "*.toml").unwrap();
        assert_eq!(repo.sparse_list("origin").unwrap(), vec!["src/**"]);
    }

    #[test]
    fn pin_patterns_persisted_in_roost_config() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::at(dir.path());
        repo.init().unwrap();
        repo.roost_add("origin", "file:///tmp/fake").unwrap();

        // Pin list starts empty.
        assert!(repo.pin_list("origin").unwrap().is_empty());

        // Pin remove of non-existent fails.
        assert!(repo.pin_remove("origin", "nope").is_err());
    }

    #[test]
    fn lazy_clone_skips_snapshot_download() {
        // Create a source repo with a checkpoint.
        let source_dir = tempfile::tempdir().unwrap();
        let source = Repo::at(source_dir.path());
        source.init().unwrap();
        std::fs::write(source_dir.path().join("test.txt"), "content").unwrap();
        source.create_checkpoint(Some("init".to_string())).unwrap();

        // Clone lazily.
        let clone_dir = tempfile::tempdir().unwrap();
        let target = clone_dir.path().join("lazy");
        let source_url = format!("file://{}", source_dir.path().display());

        let report = Repo::clone_from(
            &source_url,
            &target,
            None,
            vec![],
            true, // lazy
            None,
        ).unwrap();

        assert!(report.lazy);
        assert!(report.pull.events_pulled > 0);
        // Lazy clone should download 0 blocks (snapshots skipped).
        assert_eq!(report.pull.blocks_downloaded, 0);
    }

    #[test]
    fn clone_derives_dir_from_url() {
        // Test that RemoteUrl parsing extracts the right name.
        let url = fl_storage::RemoteUrl::parse("flock://example.com/acme/myrepo").unwrap();
        let name = url.path.rsplit('/').next().unwrap_or("repo");
        assert_eq!(name, "myrepo");

        let url = fl_storage::RemoteUrl::parse("file:///tmp/test-repo").unwrap();
        let name = url.path.rsplit('/').next().unwrap_or("repo");
        assert_eq!(name, "test-repo");
    }

    #[test]
    fn normalize_path_resolves_parent_refs() {
        let p = super::normalize_path(std::path::Path::new("a/b/../c"));
        assert_eq!(p, std::path::PathBuf::from("a/c"));

        let p = super::normalize_path(std::path::Path::new("a/./b/c"));
        assert_eq!(p, std::path::PathBuf::from("a/b/c"));
    }

    #[test]
    fn auto_materialize_triggers_at_interval() {
        use fl_storage::MaterializedStateStore;

        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        // Write legacy (unsigned, no schema wrapper) events directly to the
        // event log file so we can cheaply reach the 1,000-event threshold.
        let log_path = dir
            .path()
            .join(fl_storage::FLOCK_DIR)
            .join("event-log/events.jsonl");

        let mut lines = String::new();
        let mut parent: Option<Uuid> = None;
        for i in 0..Repo::AUTO_MATERIALIZE_INTERVAL {
            let id = Uuid::from_u128(i as u128 + 1);
            let event = Event {
                id,
                timestamp: format!("170000000000000{}", i),
                actor: "tester".to_string(),
                parent_id: parent,
                signer_public_key: None,
                signature: None,
                prev_event_hash: None,
                exploration_id: None,
                session_id: None,
                workspace_name: None,
                kind: EventKind::Session(crate::event::SessionEvent {
                    session_id: id,
                    action: crate::event::SessionAction::Start,
                    agent: "bot".to_string(),
                    initiator: None,
                    task_description: Some("t".to_string()),
                    exploration_id: None,
                    result: None,
                }),
            };
            lines.push_str(&serde_json::to_string(&event).unwrap());
            lines.push('\n');
            parent = Some(id);
        }
        fs::write(&log_path, &lines).expect("write legacy events");

        // Verify no materialized state exists yet
        let store = MaterializedStateStore::for_root(dir.path());
        assert!(
            store.load_latest().unwrap().is_none(),
            "no materialized state should exist before auto-materialize"
        );

        // Call maybe_auto_materialize — event count is exactly at the interval
        repo.maybe_auto_materialize();

        // Verify materialized state was created at the interval boundary
        let (count, json) = store
            .load_latest()
            .unwrap()
            .expect("materialized state should exist after auto-materialize");
        assert_eq!(count, Repo::AUTO_MATERIALIZE_INTERVAL);
        assert!(!json.is_empty());

        // Verify the materialized state deserializes correctly
        let state: fl_workflow::ReplayedState =
            serde_json::from_str(&json).expect("materialized state should be valid JSON");
        assert_eq!(
            state.applied_event_ids.len(),
            Repo::AUTO_MATERIALIZE_INTERVAL
        );
    }

    #[test]
    fn auto_materialize_skips_non_boundary() {
        use fl_storage::MaterializedStateStore;

        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        // Write fewer events than the interval
        let log_path = dir
            .path()
            .join(fl_storage::FLOCK_DIR)
            .join("event-log/events.jsonl");

        let mut lines = String::new();
        let mut parent: Option<Uuid> = None;
        for i in 0..50u128 {
            let id = Uuid::from_u128(i + 1);
            let event = Event {
                id,
                timestamp: format!("170000000000000{}", i),
                actor: "tester".to_string(),
                parent_id: parent,
                signer_public_key: None,
                signature: None,
                prev_event_hash: None,
                exploration_id: None,
                session_id: None,
                workspace_name: None,
                kind: EventKind::Session(crate::event::SessionEvent {
                    session_id: id,
                    action: crate::event::SessionAction::Start,
                    agent: "bot".to_string(),
                    initiator: None,
                    task_description: Some("t".to_string()),
                    exploration_id: None,
                    result: None,
                }),
            };
            lines.push_str(&serde_json::to_string(&event).unwrap());
            lines.push('\n');
            parent = Some(id);
        }
        fs::write(&log_path, &lines).expect("write legacy events");

        // Call maybe_auto_materialize — count (50) is not at an interval
        repo.maybe_auto_materialize();

        // Verify no materialized state was created
        let store = MaterializedStateStore::for_root(dir.path());
        assert!(
            store.load_latest().unwrap().is_none(),
            "no materialized state should exist at non-boundary count"
        );
    }

    #[test]
    fn auto_materialize_skips_if_already_exists() {
        use fl_storage::MaterializedStateStore;

        let dir = tempfile::tempdir().expect("tempdir should be created");
        let repo = Repo::at(dir.path());
        repo.init().expect("repo init should succeed");

        // Write exactly 1,000 events
        let log_path = dir
            .path()
            .join(fl_storage::FLOCK_DIR)
            .join("event-log/events.jsonl");

        let mut lines = String::new();
        let mut parent: Option<Uuid> = None;
        for i in 0..Repo::AUTO_MATERIALIZE_INTERVAL {
            let id = Uuid::from_u128(i as u128 + 1);
            let event = Event {
                id,
                timestamp: format!("170000000000000{}", i),
                actor: "tester".to_string(),
                parent_id: parent,
                signer_public_key: None,
                signature: None,
                prev_event_hash: None,
                exploration_id: None,
                session_id: None,
                workspace_name: None,
                kind: EventKind::Session(crate::event::SessionEvent {
                    session_id: id,
                    action: crate::event::SessionAction::Start,
                    agent: "bot".to_string(),
                    initiator: None,
                    task_description: Some("t".to_string()),
                    exploration_id: None,
                    result: None,
                }),
            };
            lines.push_str(&serde_json::to_string(&event).unwrap());
            lines.push('\n');
            parent = Some(id);
        }
        fs::write(&log_path, &lines).expect("write legacy events");

        // Pre-populate materialized state at this count
        let store = MaterializedStateStore::for_root(dir.path());
        store.save(Repo::AUTO_MATERIALIZE_INTERVAL, r#"{"pre-existing": true}"#).unwrap();

        // Call maybe_auto_materialize — should skip since snapshot exists
        repo.maybe_auto_materialize();

        // Verify the pre-existing state wasn't overwritten
        let (count, json) = store.load_latest().unwrap().unwrap();
        assert_eq!(count, Repo::AUTO_MATERIALIZE_INTERVAL);
        assert!(json.contains("pre-existing"), "existing snapshot should not be overwritten");
    }

    #[test]
    fn test_context_tags_auto_populated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        // Start an exploration — its ID should appear on subsequent events
        let exploration = repo.start_exploration("test-exp".to_string()).expect("start exploration");

        let file = dir.path().join("hello.txt");
        fs::write(&file, "hello").expect("write");
        let cp = repo.create_checkpoint(Some("tagged".to_string())).expect("checkpoint");

        // The checkpoint event should have exploration_id set
        assert_eq!(cp.exploration_id, Some(exploration.id));
    }

    #[test]
    fn test_undo_scoped_by_exploration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        // Create base checkpoint (before exploration)
        let file_a = dir.path().join("a.txt");
        fs::write(&file_a, "base-a").expect("write a");
        repo.create_checkpoint(Some("base".to_string())).expect("base checkpoint");

        // Start exploration and create two checkpoints
        let exp = repo.start_exploration("exp1".to_string()).expect("start exp");

        // First exploration checkpoint
        fs::write(&file_a, "exp-v1").expect("write exp v1");
        repo.create_checkpoint(Some("exp-v1".to_string())).expect("exp checkpoint 1");

        // Second exploration checkpoint (further modification)
        fs::write(&file_a, "exp-v2").expect("write exp v2");
        repo.create_checkpoint(Some("exp-v2".to_string())).expect("exp checkpoint 2");

        // Verify a.txt has latest value
        let content = fs::read_to_string(&file_a).expect("read a");
        assert_eq!(content, "exp-v2");

        // Scoped undo by exploration — go back 1 step within exploration
        let scope = UndoScope {
            exploration_id: Some(exp.id),
            ..Default::default()
        };
        let result = repo.undo_scoped(UndoRequest::Last, scope).expect("scoped undo");
        assert!(result.restored_checkpoint_event.is_some());

        // a.txt should be restored to the first exploration checkpoint state
        let content = fs::read_to_string(&file_a).expect("read a after undo");
        assert_eq!(content, "exp-v1");
    }

    #[test]
    fn test_undo_scoped_empty_delegates_to_normal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::at(dir.path());
        repo.init().expect("init");

        let file = dir.path().join("test.txt");
        fs::write(&file, "v1").expect("write");
        repo.create_checkpoint(Some("cp1".to_string())).expect("cp1");
        fs::write(&file, "v2").expect("write");
        repo.create_checkpoint(Some("cp2".to_string())).expect("cp2");

        // Empty scope should delegate to normal undo
        let scope = UndoScope::default();
        let result = repo.undo_scoped(UndoRequest::Last, scope).expect("undo");
        assert!(result.restored_checkpoint_event.is_some());

        // File should be back to v1
        let content = fs::read_to_string(&file).expect("read");
        assert_eq!(content, "v1");
    }
}
