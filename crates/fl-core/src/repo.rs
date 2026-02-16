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
    EventKind, ExplorationAction, ExplorationEvent, GateAction, GateCondition, GateEvent,
    GatePolicy, GitBridgeAction, GitBridgeEvent, LockAction, LockEvent, NotifyConfig,
    PresenceAction, PresenceEvent, RebaseEvent, ResourceUsageEvent, SessionAction, SessionEvent,
    SubscriptionAction, SubscriptionEvent, SubscriptionFilter, TaskAction, TaskEvent, UndoEvent,
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
    resolve_target_event, to_undo_mode,
};

pub use fl_collab::{
    ConflictStatus, ConflictSummary, GateConditionKind, GatePolicyKind, GateStatus, GateSummary,
    LockStatus, LockSummary, PresenceSummary, RebaseSummary, SubscriptionNotify,
    SubscriptionStatus, SubscriptionSummary,
};
pub use fl_workflow::parse_duration_spec;
pub use fl_workflow::{
    DecisionSummary, ExplorationStatus, ExplorationSummary, ReplayedState, ResourceUsageTotals,
    SessionStatus, SessionSummary, TaskEdge, TaskGraph, TaskRelation, TaskStatus, TaskSummary,
    UndoRequest, UndoResult,
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
}

#[derive(Debug, Clone)]
pub struct FileSummary {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
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
        self.create_checkpoint_with_options(message, false, false)
    }

    pub fn create_checkpoint_with_options(
        &self,
        message: Option<String>,
        allow_secrets: bool,
        skip_hooks: bool,
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

        let label = message
            .as_deref()
            .map(normalize_label)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("checkpoint-{}", Uuid::new_v4().simple()));

        let event = self.create_checkpoint_with_lineage(label, message, None)?;

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
                        // Directory-based mode
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
        let current_files = collect_all_files_with_mode(self.root(), true, colocated)?;

        let checkpoint = self.latest_checkpoint();
        if checkpoint.is_none() {
            // No checkpoint yet — everything is new.
            return Ok(StatusReport {
                branch,
                checkpoint_id: None,
                new_files: current_files.into_iter().collect(),
                modified_files: Vec::new(),
                deleted_files: Vec::new(),
            });
        }
        let checkpoint = checkpoint.unwrap();
        let checkpoint_id = checkpoint.id.to_string();
        let EventKind::Checkpoint(payload) = checkpoint.kind else {
            bail!("latest checkpoint event had unexpected kind");
        };

        let snapshot_root = self.snapshot_path(payload.snapshot_id);
        let snapshot_files = collect_all_files_with_mode(&snapshot_root, false, false)?;

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

    /// Semantic diff between two checkpoints identified by ID/prefix.
    pub fn semantic_diff_between_checkpoints(
        &self,
        from_prefix: &str,
        to_prefix: &str,
    ) -> Result<Vec<SemanticFileDiff>> {
        self.assert_initialized()?;

        let (_, from_payload) = self.find_checkpoint_by_prefix(from_prefix)?;
        let (_, to_payload) = self.find_checkpoint_by_prefix(to_prefix)?;

        let from_root = self.snapshot_path(from_payload.snapshot_id);
        let to_root = self.snapshot_path(to_payload.snapshot_id);

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
        let snapshot_root = self.snapshot_path(payload.snapshot_id);
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

        let from_root = self.snapshot_path(from_payload.snapshot_id);
        let to_root = self.snapshot_path(to_payload.snapshot_id);

        let from_files = collect_source_files(&from_root, false)?;
        let to_files = collect_source_files(&to_root, false)?;

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
        let snapshot_root = self.snapshot_path(payload.snapshot_id);

        let snapshot_files = collect_source_files(&snapshot_root, false)?;
        let current_files = collect_source_files(self.root(), true)?;

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
            let snapshot_root = self.snapshot_path(checkpoint.snapshot_id);
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

        if self.repo_mode()? == RepoMode::Native {
            // Native mode: store file contents as blocks, no directory copy
            self.create_native_snapshot(source_root, apply_skip, snapshot_id)?;
            self.create_checkpoint_event_with_native_merkle(
                snapshot_id,
                label,
                message,
                parent_checkpoint_event,
                git_commit_mapping,
            )
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
            ai_intent: None,
            intent_confidence: None,
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
        if self.repo_mode()? == RepoMode::Native {
            return self.restore_workspace_file_from_native_snapshot(snapshot_id, rel_path);
        }

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

    /// Create a checkpoint event using the merkle root computed from the native
    /// snapshot index (rather than walking a snapshot directory).
    fn create_checkpoint_event_with_native_merkle(
        &self,
        snapshot_id: Uuid,
        label: String,
        message: Option<String>,
        parent_checkpoint_event: Option<Uuid>,
        git_commit_mapping: Option<String>,
    ) -> Result<Event> {
        let file_index = FileIndex::for_root(self.root());
        let index = file_index.read(snapshot_id)?;

        // Compute merkle root from the file index entries (same algorithm as
        // directory-based, but using the stored file hashes)
        let snapshot_merkle_root = compute_native_merkle_root(&index)?;

        let parent_checkpoint_event =
            parent_checkpoint_event.or_else(|| self.latest_checkpoint().map(|event| event.id));
        let event = self.append_event(EventKind::Checkpoint(CheckpointEvent {
            label,
            message: message.clone(),
            snapshot_id,
            parent_checkpoint_event,
            snapshot_merkle_root: Some(snapshot_merkle_root),
            ai_intent: None,
            intent_confidence: None,
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

            let rel_key = rel_path.to_string_lossy().replace('\\', "/");
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

        let rel_key = rel_path.to_string_lossy().replace('\\', "/");
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

                    let rel_key = rel_path.to_string_lossy().replace('\\', "/");
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

        let mut event = Event {
            id: Uuid::new_v4(),
            timestamp: unix_timestamp_nanos()?,
            actor: current_actor(),
            parent_id: self.latest_event_id()?,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            kind,
        };
        self.sign_event(&mut event)?;
        AutoEventLog::for_root(self.root()).append(&event)?;

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
        self.assert_initialized()?;
        let actor = self.current_actor();
        let ttl = ttl_secs.unwrap_or(300);
        self.append_event(EventKind::Presence(PresenceEvent {
            actor: actor.clone(),
            workspace: workspace.clone(),
            action: PresenceAction::Heartbeat,
            active_files: active_files.clone(),
            intent: intent.clone(),
            ttl_secs: ttl,
        }))?;
        Ok(PresenceSummary {
            actor,
            workspace,
            active_files,
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
        let _gate = state
            .gates
            .get(&gate_id)
            .ok_or_else(|| anyhow!("gate {} not found", gate_id))?;
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
        let old_snapshot_path = self.snapshot_path(old_base_snapshot);
        let new_snapshot_path = self.snapshot_path(new_base_snapshot);

        if !old_snapshot_path.exists() || !new_snapshot_path.exists() {
            bail!("snapshot directories missing for rebase");
        }

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
            let workspace_file = self.root.parent().unwrap_or(&self.root).join(rel_path);
            if !workspace_file.exists() {
                // File was deleted in workspace; if new base added/changed it, that's a conflict
                if new_content.is_some() && old_content.is_some() {
                    conflicts.push(ConflictDetail {
                        path: rel_path.clone(),
                        symbol: None,
                        classification: "DeleteVsEdit".to_string(),
                        explanation: format!(
                            "File `{}` was deleted in workspace but modified in new base",
                            rel_path
                        ),
                    });
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

        let base_snapshot_path = self.snapshot_path(base_snapshot);
        if !base_snapshot_path.exists() {
            bail!("base snapshot directory does not exist");
        }

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

        let new_snapshot_path = self.snapshot_path(new_cp.snapshot_id);
        if !new_snapshot_path.exists() {
            bail!("target snapshot directory does not exist");
        }
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

            let workspace_file = self.root.parent().unwrap_or(&self.root).join(rel_path);
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
        let url = fl_storage::RemoteUrl {
            scheme: fl_storage::RemoteScheme::Flock,
            host: Some(host.to_string()),
            port: None,
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
        let url = fl_storage::RemoteUrl {
            scheme: fl_storage::RemoteScheme::Flock,
            host: Some(host.to_string()),
            port: None,
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
            for snap_id in &need_resp.needed_ids {
                let snap_dir = self.root.join(SNAPSHOT_DIR).join(snap_id.to_string());
                if snap_dir.is_dir() {
                    // Pack as tar and upload.
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

        // Upload missing content blocks (native storage).
        let store_dir = self.root.join("store/blocks");
        if store_dir.is_dir() {
            let mut block_hashes = Vec::new();
            for entry_res in walkdir::WalkDir::new(&store_dir).into_iter().filter_map(|e| e.ok()) {
                if entry_res.file_type().is_file() {
                    if let Some(name) = entry_res.file_name().to_str() {
                        if let Some(prefix) = entry_res.path().parent().and_then(|p| p.file_name()).and_then(|p| p.to_str()) {
                            block_hashes.push(format!("{prefix}{name}"));
                        }
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
                            let rest = &h[2..];
                            let path = store_dir.join(prefix).join(rest);
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

        self.pull_with_options(roost_name, branch, depth, &sparse, lazy)
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
        repo.init()?;
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
                if !snap_dir.exists() {
                    match transport.download_snapshot(*snap_id) {
                        Ok(data) => {
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
                        Err(_) => {
                            // Snapshot may not exist on remote (native mode).
                        }
                    }
                }
            }
        }

        // Re-sign any graft events (those with cleared signatures from
        // shallow clone depth truncation) using our local signing key.
        let mut events_to_append = resp.events.clone();
        for event in &mut events_to_append {
            if event.signature.is_none() && event.signer_public_key.is_none() {
                // Graft event — re-sign with our key.
                self.sign_event(event)?;
            }
        }

        // Append pulled events.
        AutoEventLog::for_root(self.root()).append_batch(&events_to_append)?;

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
        self.append_event(EventKind::RemoteSync(crate::event::RemoteSyncEvent {
            action: crate::event::RemoteSyncAction::Pull,
            roost_name: roost_name.to_string(),
            roost_url: url.to_string(),
            success: true,
            detail: None,
            event_count: events_pulled,
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
    pub fn ws_connect(
        &self,
        roost_name: &str,
    ) -> Result<crate::ws_client::WsClient> {
        self.assert_initialized()?;
        let config = fl_storage::load_roosts(self.root())?;
        let entry = fl_storage::find_roost(&config, roost_name)
            .ok_or_else(|| anyhow!("roost '{}' not found", roost_name))?;

        let url = fl_storage::RemoteUrl::parse(&entry.url)?;
        let ws_url = url.ws_url()?;
        let resolved_token =
            fl_storage::resolve_token(entry.token.as_deref(), url.host.as_deref())?;
        let token = resolved_token.unwrap_or_default();

        let ws_config = crate::ws_client::WsClientConfig::new(ws_url, token);
        crate::ws_client::WsClient::connect(ws_config)
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

/// Collect all files (not just source files) under root, returning relative
/// path strings. Used by `status()` to compare working directory vs snapshot.
fn collect_all_files_with_mode(
    root: &Path,
    apply_skip: bool,
    colocated: bool,
) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();

    if apply_skip {
        let walker = build_repo_walker(root, colocated);
        for entry in walker.build() {
            let entry = entry.context("failed while scanning files")?;
            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }

            let rel = entry
                .path()
                .strip_prefix(root)
                .context("failed to compute relative path")?;
            files.insert(rel.to_string_lossy().replace('\\', "/"));
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
            files.insert(rel.to_string_lossy().replace('\\', "/"));
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

        // Generate test files: 10 files of ~10KB each
        let file_count = 10;
        let file_size = 10_000;
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

        // Now test deduplication advantage: create a second checkpoint with
        // only 1 file changed
        fs::write(
            dir_compat.path().join("file_0.txt"),
            generate_content(100),
        )
        .unwrap();
        fs::write(
            dir_native.path().join("file_0.txt"),
            generate_content(100),
        )
        .unwrap();

        let start = Instant::now();
        repo_compat
            .create_checkpoint(Some("compat-bench-2".to_string()))
            .unwrap();
        let compat_time_2 = start.elapsed();

        let start = Instant::now();
        repo_native
            .create_checkpoint(Some("native-bench-2".to_string()))
            .unwrap();
        let native_time_2 = start.elapsed();

        // Native mode should store less data across 2 checkpoints due to dedup
        let compat_size = dir_size(dir_compat.path());
        let native_size = dir_size(dir_native.path());

        // The native store should be smaller than 2 full copies
        // (9 files are identical between the two checkpoints)
        assert!(
            native_size < compat_size,
            "native store ({} bytes) should be smaller than compat ({} bytes)",
            native_size,
            compat_size
        );

        eprintln!(
            "Benchmark results ({}x{}B files):",
            file_count, file_size
        );
        eprintln!("  1st checkpoint: compat={:?}, native={:?}", compat_time, native_time);
        eprintln!(
            "  2nd checkpoint: compat={:?}, native={:?}",
            compat_time_2, native_time_2
        );
        eprintln!(
            "  Total storage:  compat={} bytes, native={} bytes ({:.1}% of compat)",
            compat_size,
            native_size,
            native_size as f64 / compat_size as f64 * 100.0
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

        let snapshot_root = dir.path().join(".flock").join("snapshots").join(payload.snapshot_id.to_string());
        assert!(snapshot_root.join("app.ts").exists());
        // .gitignore patterns should be respected in colocated mode.
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

        let snapshot_root = dir.path().join(".flock").join("snapshots").join(payload.snapshot_id.to_string());
        assert!(snapshot_root.join("app.ts").exists());
        // .tmp should NOT be ignored (gitignore is disabled when flockignore exists).
        assert!(snapshot_root.join("temp.tmp").exists());
        // .bak should be ignored (flockignore rule).
        assert!(!snapshot_root.join("old.bak").exists());
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

        // Verify git history exists
        let log_output = fl_bridge_git::run_git(dir.path(), &["log", "--oneline"]).expect("git log");
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
            .create_task("test task".to_string(), None, vec![], None)
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
}
