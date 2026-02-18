use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use fl_collab::{
    ConflictStatus, ConflictSummary, GateConditionKind, GatePolicyKind, GateStatus, GateSummary,
    LockStatus, LockSummary, PresenceSummary, RebaseSummary, SubscriptionNotify,
    SubscriptionStatus, SubscriptionSummary,
};
use fl_storage::{
    ApiCallRecord, BlockRef, ConflictAction, DecisionAction, Event, EventKind, ExplorationAction,
    GateAction, GateCondition, GatePolicy, LockAction, NotifyConfig, PresenceAction, SessionAction,
    SubscriptionAction, TaskAction, UndoMode,
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplorationSummary {
    pub id: Uuid,
    pub title: String,
    pub status: ExplorationStatus,
    pub base_checkpoint_event: Option<Uuid>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Completed,
    Failed,
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Completed => f.write_str("completed"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub agent: String,
    pub initiator: Option<String>,
    pub task_description: Option<String>,
    pub status: SessionStatus,
    pub explorations: Vec<Uuid>,
    pub decisions: Vec<DecisionSummary>,
    pub resource_usage: ResourceUsageTotals,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub result: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionSummary {
    pub exploration_id: Uuid,
    pub action: DecisionAction,
    pub reason: String,
    pub confidence: f64,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResourceUsageTotals {
    pub total_tokens: u64,
    pub total_runtime_ms: u64,
    pub api_calls: Vec<ApiCallRecord>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Open,
    Claimed,
    Completed,
    Failed,
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => f.write_str("open"),
            Self::Claimed => f.write_str("claimed"),
            Self::Completed => f.write_str("completed"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub dependencies: Vec<Uuid>,
    pub dependents: Vec<Uuid>,
    pub assignee: Option<String>,
    pub created_at: String,
    pub claimed_at: Option<String>,
    pub completed_at: Option<String>,
    pub result: Option<String>,
    pub linked_events: Vec<Uuid>,
    pub discovered_from: Option<Uuid>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
}

impl TaskSummary {
    /// Returns true if all dependency tasks are completed.
    pub fn is_ready(&self, tasks: &BTreeMap<Uuid, TaskSummary>) -> bool {
        if self.status != TaskStatus::Open {
            return false;
        }
        self.dependencies.iter().all(|dep_id| {
            tasks
                .get(dep_id)
                .is_some_and(|t| t.status == TaskStatus::Completed)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGraph {
    pub tasks: Vec<TaskSummary>,
    pub edges: Vec<TaskEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEdge {
    pub from_task: Uuid,
    pub to_task: Uuid,
    pub relation: TaskRelation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskRelation {
    DependsOn,
    DiscoveredFrom,
}

/// Tracks budget usage per task for policy enforcement.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PolicyBudgetTracker {
    pub task_id: Uuid,
    pub files_modified: BTreeSet<String>,
    pub lines_changed: u32,
    /// Per-exploration file tracking.
    pub exploration_files: BTreeMap<Uuid, BTreeSet<String>>,
    /// Total semantic changes across the task.
    #[serde(default)]
    pub semantic_changes: u32,
    /// Per-exploration semantic change tracking.
    #[serde(default)]
    pub exploration_semantic_changes: BTreeMap<Uuid, u32>,
}

/// Tracks rate limit usage per task for policy enforcement.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PolicyRateLimitTracker {
    pub task_id: Uuid,
    pub explorations_started: u32,
    /// Undo count per exploration.
    pub undo_counts: BTreeMap<Uuid, u32>,
    /// Checkpoint timestamps (nanosecond strings) for windowed rate limiting.
    pub checkpoint_timestamps: Vec<String>,
}

/// A recorded policy decision for audit purposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionRecord {
    pub policy_name: String,
    pub policy_category: String,
    pub verdict: String,
    pub operation: String,
    pub reason: Option<String>,
    pub task_id: Option<Uuid>,
    pub exploration_id: Option<Uuid>,
    pub affected_files: Vec<String>,
    pub timestamp: String,
}

/// Per-file state tracked from FileWrite/FileDelete/FileRename events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub event_id: Uuid,
    pub content_hash: String,
    pub blocks: Vec<BlockRef>,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayedState {
    pub latest_checkpoint_event_id: Option<Uuid>,
    pub latest_checkpoint_snapshot_id: Option<Uuid>,
    pub explorations: BTreeMap<Uuid, ExplorationSummary>,
    pub sessions: BTreeMap<Uuid, SessionSummary>,
    pub tasks: BTreeMap<Uuid, TaskSummary>,
    pub presence: BTreeMap<String, PresenceSummary>,
    pub locks: BTreeMap<Uuid, LockSummary>,
    pub subscriptions: BTreeMap<Uuid, SubscriptionSummary>,
    pub gates: BTreeMap<Uuid, GateSummary>,
    pub rebases: Vec<RebaseSummary>,
    pub conflicts: BTreeMap<Uuid, ConflictSummary>,
    pub applied_event_ids: Vec<Uuid>,
    #[serde(default)]
    pub directives: Vec<fl_collab::DirectiveSummary>,
    #[serde(default)]
    pub policy_budgets: BTreeMap<Uuid, PolicyBudgetTracker>,
    #[serde(default)]
    pub policy_rate_limits: BTreeMap<Uuid, PolicyRateLimitTracker>,
    #[serde(default)]
    pub policy_decisions: Vec<PolicyDecisionRecord>,
    #[serde(default)]
    pub file_states: BTreeMap<String, FileState>,
}

#[derive(Debug, Clone, Default)]
struct ReplayAccumulator {
    latest_checkpoint_event_id: Option<Uuid>,
    latest_checkpoint_snapshot_id: Option<Uuid>,
    explorations: BTreeMap<Uuid, ExplorationSummary>,
    sessions: BTreeMap<Uuid, SessionSummary>,
    tasks: BTreeMap<Uuid, TaskSummary>,
    presence: BTreeMap<String, PresenceSummary>,
    locks: BTreeMap<Uuid, LockSummary>,
    subscriptions: BTreeMap<Uuid, SubscriptionSummary>,
    gates: BTreeMap<Uuid, GateSummary>,
    rebases: Vec<RebaseSummary>,
    conflicts: BTreeMap<Uuid, ConflictSummary>,
    applied_event_ids: Vec<Uuid>,
    directives: Vec<fl_collab::DirectiveSummary>,
    policy_budgets: BTreeMap<Uuid, PolicyBudgetTracker>,
    policy_rate_limits: BTreeMap<Uuid, PolicyRateLimitTracker>,
    policy_decisions: Vec<PolicyDecisionRecord>,
    file_states: BTreeMap<String, FileState>,
}

impl From<ReplayedState> for ReplayAccumulator {
    fn from(state: ReplayedState) -> Self {
        Self {
            latest_checkpoint_event_id: state.latest_checkpoint_event_id,
            latest_checkpoint_snapshot_id: state.latest_checkpoint_snapshot_id,
            explorations: state.explorations,
            sessions: state.sessions,
            tasks: state.tasks,
            presence: state.presence,
            locks: state.locks,
            subscriptions: state.subscriptions,
            gates: state.gates,
            rebases: state.rebases,
            conflicts: state.conflicts,
            applied_event_ids: state.applied_event_ids,
            directives: state.directives,
            policy_budgets: state.policy_budgets,
            policy_rate_limits: state.policy_rate_limits,
            policy_decisions: state.policy_decisions,
            file_states: state.file_states,
        }
    }
}

impl ReplayAccumulator {
    fn into_state(self) -> ReplayedState {
        // Compute dependents from dependencies
        let mut tasks = self.tasks;
        let dep_pairs: Vec<(Uuid, Uuid)> = tasks
            .values()
            .flat_map(|t| t.dependencies.iter().map(move |dep| (*dep, t.id)))
            .collect();
        for (dep_id, dependent_id) in dep_pairs {
            if let Some(dep_task) = tasks.get_mut(&dep_id) {
                if !dep_task.dependents.contains(&dependent_id) {
                    dep_task.dependents.push(dependent_id);
                }
            }
        }

        ReplayedState {
            latest_checkpoint_event_id: self.latest_checkpoint_event_id,
            latest_checkpoint_snapshot_id: self.latest_checkpoint_snapshot_id,
            explorations: self.explorations,
            sessions: self.sessions,
            tasks,
            presence: self.presence,
            locks: self.locks,
            subscriptions: self.subscriptions,
            gates: self.gates,
            rebases: self.rebases,
            conflicts: self.conflicts,
            applied_event_ids: self.applied_event_ids,
            directives: self.directives,
            policy_budgets: self.policy_budgets,
            policy_rate_limits: self.policy_rate_limits,
            policy_decisions: self.policy_decisions,
            file_states: self.file_states,
        }
    }

    /// Find the task ID claimed by the given actor, if any.
    fn find_claimed_task_for_actor(&self, actor: &str) -> Option<Uuid> {
        self.tasks
            .values()
            .find(|t| {
                t.status == TaskStatus::Claimed
                    && t.assignee.as_deref() == Some(actor)
            })
            .map(|t| t.id)
    }

    fn apply_event(
        &mut self,
        event: &Event,
        checkpoints: &BTreeMap<Uuid, Uuid>,
        state_before_event: &BTreeMap<Uuid, ReplayAccumulator>,
    ) -> Result<()> {
        match &event.kind {
            EventKind::Checkpoint(checkpoint) => {
                self.latest_checkpoint_event_id = Some(event.id);
                self.latest_checkpoint_snapshot_id = Some(checkpoint.snapshot_id);
                // Track checkpoint timestamps for rate limiting — attribute to the
                // task whose actor matches the event actor (if any).
                let actor_task = self.find_claimed_task_for_actor(&event.actor);
                if let Some(tid) = actor_task {
                    if let Some(tracker) = self.policy_rate_limits.get_mut(&tid) {
                        tracker.checkpoint_timestamps.push(event.timestamp.clone());
                    }
                    // Budget file/line/semantic tracking from file change metadata.
                    if let Some(files_changed) = &checkpoint.files_changed {
                        if let Some(tracker) = self.policy_budgets.get_mut(&tid) {
                            for fc in files_changed {
                                tracker.files_modified.insert(fc.path.clone());
                                tracker.lines_changed += fc.lines_added + fc.lines_removed;
                                // Accumulate semantic changes.
                                if let Some(sc) = fc.semantic_changes_count {
                                    tracker.semantic_changes += sc;
                                }
                            }
                            // Track per-exploration files and semantic changes.
                            let active_exploration = self.explorations.values()
                                .find(|e| e.status == ExplorationStatus::Active)
                                .map(|e| e.id);
                            if let Some(exp_id) = active_exploration {
                                let exp_files = tracker.exploration_files.entry(exp_id).or_default();
                                for fc in files_changed {
                                    exp_files.insert(fc.path.clone());
                                    if let Some(sc) = fc.semantic_changes_count {
                                        *tracker.exploration_semantic_changes.entry(exp_id).or_default() += sc;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            EventKind::Exploration(exploration) => match exploration.action {
                ExplorationAction::Start => {
                    self.explorations.insert(
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
                    // Track exploration starts for rate limiting — attribute to
                    // the task whose actor matches the event actor.
                    let actor_task = self.find_claimed_task_for_actor(&event.actor);
                    if let Some(tid) = actor_task {
                        if let Some(tracker) = self.policy_rate_limits.get_mut(&tid) {
                            tracker.explorations_started += 1;
                        }
                    }
                }
                ExplorationAction::Promote => {
                    if let Some(entry) = self.explorations.get_mut(&exploration.exploration_id) {
                        entry.status = ExplorationStatus::Promoted;
                        entry.updated_at = event.timestamp.clone();
                    }
                }
                ExplorationAction::Abandon => {
                    if let Some(entry) = self.explorations.get_mut(&exploration.exploration_id) {
                        entry.status = ExplorationStatus::Abandoned;
                        entry.updated_at = event.timestamp.clone();
                    }
                }
                ExplorationAction::Prune => {
                    self.explorations.remove(&exploration.exploration_id);
                }
            },
            EventKind::Undo(undo) => {
                if undo.target_event_id == event.id {
                    bail!("undo event {} cannot target itself", event.id);
                }

                if undo.file_scope.is_none() {
                    if let Some(rewound) = state_before_event
                        .get(&undo.target_event_id)
                        .cloned()
                    {
                        *self = rewound;
                    } else {
                        // The target event is missing from the replay
                        // history.  This can happen when incremental
                        // replay state was materialized between the
                        // target and this undo, or after heavy undo
                        // operations that rewrite effective history.
                        // Skip the state rewind — the restored
                        // checkpoint (if any) will still be applied
                        // below, keeping the latest snapshot correct.
                        eprintln!(
                            "warning: undo event {} targets unknown event {}; skipping state rewind",
                            event.id, undo.target_event_id
                        );
                    }
                }

                if let Some(restored_checkpoint_event) = undo.restored_checkpoint_event {
                    if let Some(&snapshot_id) = checkpoints.get(&restored_checkpoint_event) {
                        self.latest_checkpoint_event_id = Some(restored_checkpoint_event);
                        self.latest_checkpoint_snapshot_id = Some(snapshot_id);
                        if !self.applied_event_ids.contains(&restored_checkpoint_event) {
                            self.applied_event_ids.push(restored_checkpoint_event);
                        }
                    } else {
                        eprintln!(
                            "warning: undo event {} references unknown restored checkpoint {}; skipping",
                            event.id, restored_checkpoint_event
                        );
                    }
                }
                // Track undo counts for rate limiting — attribute to the
                // task whose actor matches the event actor.
                let active_exploration = self.explorations.values()
                    .find(|e| e.status == ExplorationStatus::Active)
                    .map(|e| e.id);
                if let Some(exp_id) = active_exploration {
                    let actor_task = self.find_claimed_task_for_actor(&event.actor);
                    if let Some(tid) = actor_task {
                        if let Some(tracker) = self.policy_rate_limits.get_mut(&tid) {
                            *tracker.undo_counts.entry(exp_id).or_insert(0) += 1;
                        }
                    }
                }
            }
            EventKind::GitBridge(_) => {}
            EventKind::Hook(_) => {}
            EventKind::RemoteSync(_) => {}
            EventKind::Intelligence(_) => {}
            EventKind::FileWrite(fw) => {
                self.file_states.insert(
                    fw.path.clone(),
                    FileState {
                        event_id: event.id,
                        content_hash: fw.content_hash.clone(),
                        blocks: fw.blocks.clone(),
                        size: fw.size,
                    },
                );
            }
            EventKind::FileDelete(fd) => {
                self.file_states.remove(&fd.path);
            }
            EventKind::FileRename(fr) => {
                self.file_states.remove(&fr.old_path);
                self.file_states.insert(
                    fr.new_path.clone(),
                    FileState {
                        event_id: event.id,
                        content_hash: fr.content_hash.clone(),
                        blocks: fr.blocks.clone(),
                        size: fr.size,
                    },
                );
            }
            EventKind::Policy(policy) => {
                let verdict_str = match policy.verdict {
                    fl_storage::PolicyVerdictKind::Allow => "Allow",
                    fl_storage::PolicyVerdictKind::Gate => "Gate",
                    fl_storage::PolicyVerdictKind::Block => "Block",
                };
                self.policy_decisions.push(PolicyDecisionRecord {
                    policy_name: policy.policy_name.clone(),
                    policy_category: policy.policy_category.clone(),
                    verdict: verdict_str.to_string(),
                    operation: policy.operation.clone(),
                    reason: policy.reason.clone(),
                    task_id: policy.task_id,
                    exploration_id: policy.exploration_id,
                    affected_files: policy.affected_files.clone(),
                    timestamp: event.timestamp.clone(),
                });
            }
            EventKind::Directive(directive) => {
                let (kind_str, detail) = match &directive.directive {
                    fl_storage::DirectiveKind::Pause => ("pause".to_string(), None),
                    fl_storage::DirectiveKind::Resume => ("resume".to_string(), None),
                    fl_storage::DirectiveKind::Redirect { new_task } => ("redirect".to_string(), Some(new_task.clone())),
                    fl_storage::DirectiveKind::Abort { reason } => ("abort".to_string(), Some(reason.clone())),
                };
                self.directives.push(fl_collab::DirectiveSummary {
                    id: event.id,
                    target_actor: directive.target_actor.clone(),
                    directive_kind: kind_str,
                    directive_detail: detail,
                    reason: directive.reason.clone(),
                    issued_by: directive.issued_by.clone(),
                    issued_at: event.timestamp.clone(),
                    acknowledged: false,
                });
            }
            EventKind::Session(session) => match session.action {
                SessionAction::Start => {
                    self.sessions.insert(
                        session.session_id,
                        SessionSummary {
                            id: session.session_id,
                            agent: session.agent.clone(),
                            initiator: session.initiator.clone(),
                            task_description: session.task_description.clone(),
                            status: SessionStatus::Active,
                            explorations: Vec::new(),
                            decisions: Vec::new(),
                            resource_usage: ResourceUsageTotals::default(),
                            created_at: event.timestamp.clone(),
                            completed_at: None,
                            result: None,
                        },
                    );
                }
                SessionAction::Link => {
                    if let Some(entry) = self.sessions.get_mut(&session.session_id) {
                        if let Some(exploration_id) = session.exploration_id {
                            if !entry.explorations.contains(&exploration_id) {
                                entry.explorations.push(exploration_id);
                            }
                        }
                    }
                }
                SessionAction::Unlink => {
                    if let Some(entry) = self.sessions.get_mut(&session.session_id) {
                        if let Some(exploration_id) = session.exploration_id {
                            entry.explorations.retain(|id| *id != exploration_id);
                        }
                    }
                }
                SessionAction::Complete => {
                    if let Some(entry) = self.sessions.get_mut(&session.session_id) {
                        entry.status = SessionStatus::Completed;
                        entry.completed_at = Some(event.timestamp.clone());
                        entry.result = session.result.clone();
                    }
                }
                SessionAction::Fail => {
                    if let Some(entry) = self.sessions.get_mut(&session.session_id) {
                        entry.status = SessionStatus::Failed;
                        entry.completed_at = Some(event.timestamp.clone());
                        entry.result = session.result.clone();
                    }
                }
            },
            EventKind::Decision(decision) => {
                if let Some(entry) = self.sessions.get_mut(&decision.session_id) {
                    entry.decisions.push(DecisionSummary {
                        exploration_id: decision.exploration_id,
                        action: decision.action.clone(),
                        reason: decision.reason.clone(),
                        confidence: decision.confidence,
                        timestamp: event.timestamp.clone(),
                    });
                }
            }
            EventKind::ResourceUsage(usage) => {
                if let Some(entry) = self.sessions.get_mut(&usage.session_id) {
                    if let Some(tokens) = usage.tokens_consumed {
                        entry.resource_usage.total_tokens += tokens;
                    }
                    if let Some(runtime) = usage.runtime_ms {
                        entry.resource_usage.total_runtime_ms += runtime;
                    }
                    if let Some(calls) = &usage.api_calls {
                        entry.resource_usage.api_calls.extend(calls.iter().cloned());
                    }
                }
            }
            EventKind::Presence(presence) => match presence.action {
                PresenceAction::Heartbeat => {
                    let key = format!("{}@{}", presence.actor, presence.workspace);
                    self.presence.insert(
                        key,
                        PresenceSummary {
                            actor: presence.actor.clone(),
                            workspace: presence.workspace.clone(),
                            active_files: presence.active_files.clone(),
                            active_symbols: presence.active_symbols.clone(),
                            intent: presence.intent.clone(),
                            ttl: Duration::from_secs(presence.ttl_secs),
                            last_heartbeat: event.timestamp.clone(),
                        },
                    );
                }
                PresenceAction::Depart => {
                    let key = format!("{}@{}", presence.actor, presence.workspace);
                    self.presence.remove(&key);
                }
            },
            EventKind::Lock(lock) => match lock.action {
                LockAction::Acquire => {
                    self.locks.insert(
                        lock.lock_id,
                        LockSummary {
                            id: lock.lock_id,
                            resource: lock.resource.clone(),
                            holder: lock.holder.clone(),
                            status: LockStatus::Held,
                            ttl: Duration::from_secs(lock.ttl_secs),
                            acquired_at: event.timestamp.clone(),
                            released_at: None,
                        },
                    );
                }
                LockAction::Release => {
                    if let Some(entry) = self.locks.get_mut(&lock.lock_id) {
                        entry.status = LockStatus::Released;
                        entry.released_at = Some(event.timestamp.clone());
                    }
                }
            },
            EventKind::Subscription(sub) => match sub.action {
                SubscriptionAction::Subscribe => {
                    let filter = sub.filter.as_ref();
                    let notify = match sub.notify.as_ref() {
                        Some(NotifyConfig::Immediate) | None => SubscriptionNotify::Immediate,
                        Some(NotifyConfig::Batched) => SubscriptionNotify::Batched,
                        Some(NotifyConfig::Digest) => SubscriptionNotify::Digest,
                    };
                    self.subscriptions.insert(
                        sub.subscription_id,
                        SubscriptionSummary {
                            id: sub.subscription_id,
                            actor: sub.actor.clone(),
                            status: SubscriptionStatus::Active,
                            paths: filter.map(|f| f.paths.clone()).unwrap_or_default(),
                            symbols: filter.map(|f| f.symbols.clone()).unwrap_or_default(),
                            modules: filter.map(|f| f.modules.clone()).unwrap_or_default(),
                            notify,
                            created_at: event.timestamp.clone(),
                            cancelled_at: None,
                        },
                    );
                }
                SubscriptionAction::Unsubscribe => {
                    if let Some(entry) = self.subscriptions.get_mut(&sub.subscription_id) {
                        entry.status = SubscriptionStatus::Cancelled;
                        entry.cancelled_at = Some(event.timestamp.clone());
                    }
                }
            },
            EventKind::Gate(gate) => match gate.action {
                GateAction::Create => {
                    let condition = match &gate.condition {
                        Some(GateCondition::FileTouched(p)) => GateConditionKind::FileTouched(p.clone()),
                        Some(GateCondition::SymbolModified(s)) => GateConditionKind::SymbolModified(s.clone()),
                        Some(GateCondition::ImpactExceeds(n)) => GateConditionKind::ImpactExceeds(*n),
                        Some(GateCondition::SecuritySensitive) => GateConditionKind::SecuritySensitive,
                        Some(GateCondition::AgentConfidenceLow(n)) => GateConditionKind::AgentConfidenceLow(*n),
                        None => GateConditionKind::SecuritySensitive,
                    };
                    let policy = match gate.policy {
                        Some(GatePolicy::QueueAndContinue) => GatePolicyKind::QueueAndContinue,
                        Some(GatePolicy::Block) | None => GatePolicyKind::Block,
                    };
                    self.gates.insert(
                        gate.gate_id,
                        GateSummary {
                            id: gate.gate_id,
                            status: GateStatus::Active,
                            condition,
                            policy,
                            approved_by: None,
                            reason: None,
                            created_at: event.timestamp.clone(),
                            resolved_at: None,
                        },
                    );
                }
                GateAction::Approve => {
                    if let Some(entry) = self.gates.get_mut(&gate.gate_id) {
                        entry.status = GateStatus::Approved;
                        entry.approved_by = gate.approved_by.clone();
                        entry.reason = gate.reason.clone();
                        entry.resolved_at = Some(event.timestamp.clone());
                    }
                }
                GateAction::Reject => {
                    if let Some(entry) = self.gates.get_mut(&gate.gate_id) {
                        entry.status = GateStatus::Rejected;
                        entry.approved_by = gate.approved_by.clone();
                        entry.reason = gate.reason.clone();
                        entry.resolved_at = Some(event.timestamp.clone());
                    }
                }
                GateAction::Delete => {
                    if let Some(entry) = self.gates.get_mut(&gate.gate_id) {
                        entry.status = GateStatus::Deleted;
                        entry.resolved_at = Some(event.timestamp.clone());
                    }
                }
            },
            EventKind::Rebase(rebase) => {
                self.rebases.push(RebaseSummary {
                    workspace: rebase.workspace.clone(),
                    old_base_event: rebase.old_base_event,
                    new_base_event: rebase.new_base_event,
                    files_merged: rebase.files_merged.clone(),
                    conflicts_found: rebase.conflicts_found,
                    auto: rebase.auto,
                    timestamp: event.timestamp.clone(),
                });
            }
            EventKind::ConflictResolution(cr) => match cr.action {
                ConflictAction::Detect => {
                    self.conflicts.insert(
                        cr.conflict_id,
                        ConflictSummary {
                            id: cr.conflict_id,
                            workspace: cr.workspace.clone().unwrap_or_default(),
                            path: cr.path.clone().unwrap_or_default(),
                            symbol: cr.symbol.clone(),
                            classification: None,
                            suggestion: None,
                            resolution: None,
                            resolved_by: None,
                            verified: false,
                            status: ConflictStatus::Detected,
                            detected_at: event.timestamp.clone(),
                            resolved_at: None,
                        },
                    );
                }
                ConflictAction::Classify => {
                    if let Some(entry) = self.conflicts.get_mut(&cr.conflict_id) {
                        entry.classification = cr.classification.clone();
                        entry.status = ConflictStatus::Classified;
                    }
                }
                ConflictAction::Suggest => {
                    if let Some(entry) = self.conflicts.get_mut(&cr.conflict_id) {
                        entry.suggestion = cr.suggestion.clone();
                        entry.status = ConflictStatus::Suggested;
                    }
                }
                ConflictAction::Resolve => {
                    if let Some(entry) = self.conflicts.get_mut(&cr.conflict_id) {
                        entry.resolution = cr.resolution.clone();
                        entry.resolved_by = cr.resolved_by.clone();
                        entry.status = ConflictStatus::Resolved;
                        entry.resolved_at = Some(event.timestamp.clone());
                    }
                }
                ConflictAction::Verify => {
                    if let Some(entry) = self.conflicts.get_mut(&cr.conflict_id) {
                        entry.verified = cr.verified.unwrap_or(false);
                        entry.status = ConflictStatus::Verified;
                    }
                }
                ConflictAction::Record => {
                    if let Some(entry) = self.conflicts.get_mut(&cr.conflict_id) {
                        entry.status = ConflictStatus::Recorded;
                    }
                }
            },
            EventKind::Task(task) => match task.action {
                TaskAction::Create => {
                    self.tasks.insert(
                        task.task_id,
                        TaskSummary {
                            id: task.task_id,
                            title: task.title.clone(),
                            description: task.description.clone(),
                            status: TaskStatus::Open,
                            dependencies: task.dependencies.clone(),
                            dependents: Vec::new(),
                            assignee: None,
                            created_at: event.timestamp.clone(),
                            claimed_at: None,
                            completed_at: None,
                            result: None,
                            linked_events: task.linked_events.clone(),
                            discovered_from: task.discovered_from,
                            allowed_paths: task.allowed_paths.clone(),
                        },
                    );
                }
                TaskAction::Claim => {
                    if let Some(entry) = self.tasks.get_mut(&task.task_id) {
                        entry.status = TaskStatus::Claimed;
                        entry.assignee = task
                            .assignee
                            .clone()
                            .or_else(|| Some(event.actor.clone()));
                        entry.claimed_at = Some(event.timestamp.clone());
                    }
                    // Initialize policy trackers for the claimed task.
                    self.policy_budgets.entry(task.task_id).or_insert_with(|| {
                        PolicyBudgetTracker {
                            task_id: task.task_id,
                            ..Default::default()
                        }
                    });
                    self.policy_rate_limits.entry(task.task_id).or_insert_with(|| {
                        PolicyRateLimitTracker {
                            task_id: task.task_id,
                            ..Default::default()
                        }
                    });
                }
                TaskAction::Unclaim => {
                    if let Some(entry) = self.tasks.get_mut(&task.task_id) {
                        entry.status = TaskStatus::Open;
                        entry.assignee = None;
                        entry.claimed_at = None;
                    }
                }
                TaskAction::Complete => {
                    if let Some(entry) = self.tasks.get_mut(&task.task_id) {
                        entry.status = TaskStatus::Completed;
                        entry.completed_at = Some(event.timestamp.clone());
                        entry.result = task.result.clone();
                    }
                }
                TaskAction::Fail => {
                    if let Some(entry) = self.tasks.get_mut(&task.task_id) {
                        entry.status = TaskStatus::Failed;
                        entry.completed_at = Some(event.timestamp.clone());
                        entry.result = task.result.clone();
                    }
                }
                TaskAction::Link => {
                    if let Some(entry) = self.tasks.get_mut(&task.task_id) {
                        for linked in &task.linked_events {
                            if !entry.linked_events.contains(linked) {
                                entry.linked_events.push(*linked);
                            }
                        }
                    }
                }
            },
            EventKind::Init(_) => {
                // Init events are informational; no replay state changes needed.
            },
        }

        if !self.applied_event_ids.contains(&event.id) {
            self.applied_event_ids.push(event.id);
        }

        Ok(())
    }
}

pub fn replay_state(events: &[Event]) -> Result<ReplayedState> {
    let checkpoints: BTreeMap<Uuid, Uuid> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Checkpoint(checkpoint) => Some((event.id, checkpoint.snapshot_id)),
            _ => None,
        })
        .collect();

    let mut state = ReplayAccumulator::default();
    let mut state_before_event = BTreeMap::<Uuid, ReplayAccumulator>::new();

    for event in events {
        if state_before_event.contains_key(&event.id) {
            bail!("duplicate event id {} encountered during replay", event.id);
        }
        state_before_event.insert(event.id, state.clone());

        state.apply_event(event, &checkpoints, &state_before_event)?;
    }

    Ok(state.into_state())
}

/// Replay state incrementally starting from a materialized state.
/// Only processes events after `start_index`.
///
/// If any new undo event targets an event before the materialization point,
/// falls back to full replay since the pre-materialization state snapshots
/// are not available.
pub fn replay_state_incremental(
    events: &[Event],
    start_index: usize,
    base_state: ReplayedState,
) -> Result<ReplayedState> {
    if start_index >= events.len() {
        return Ok(base_state);
    }

    // Check whether any new undo event targets an event before the
    // materialization point.  If so, we need the full state_before_event
    // history which incremental replay cannot provide — fall back.
    let pre_event_ids: std::collections::HashSet<Uuid> = events[..start_index]
        .iter()
        .map(|e| e.id)
        .collect();
    for event in &events[start_index..] {
        if let EventKind::Undo(ref undo) = event.kind {
            if undo.file_scope.is_none() && pre_event_ids.contains(&undo.target_event_id) {
                return replay_state(events);
            }
        }
    }

    let checkpoints: BTreeMap<Uuid, Uuid> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Checkpoint(checkpoint) => Some((event.id, checkpoint.snapshot_id)),
            _ => None,
        })
        .collect();

    let mut state = ReplayAccumulator::from(base_state);
    let mut state_before_event = BTreeMap::<Uuid, ReplayAccumulator>::new();

    for event in &events[start_index..] {
        if state_before_event.contains_key(&event.id) {
            bail!("duplicate event id {} encountered during replay", event.id);
        }
        state_before_event.insert(event.id, state.clone());

        state.apply_event(event, &checkpoints, &state_before_event)?;
    }

    Ok(state.into_state())
}

pub fn replay_explorations(events: &[Event]) -> Result<BTreeMap<Uuid, ExplorationSummary>> {
    Ok(replay_state(events)?.explorations)
}

pub fn replay_sessions(events: &[Event]) -> Result<BTreeMap<Uuid, SessionSummary>> {
    Ok(replay_state(events)?.sessions)
}

pub fn replay_tasks(events: &[Event]) -> Result<BTreeMap<Uuid, TaskSummary>> {
    Ok(replay_state(events)?.tasks)
}

pub fn replay_presence(events: &[Event]) -> Result<BTreeMap<String, PresenceSummary>> {
    Ok(replay_state(events)?.presence)
}

pub fn replay_locks(events: &[Event]) -> Result<BTreeMap<Uuid, LockSummary>> {
    Ok(replay_state(events)?.locks)
}

pub fn replay_subscriptions(events: &[Event]) -> Result<BTreeMap<Uuid, SubscriptionSummary>> {
    Ok(replay_state(events)?.subscriptions)
}

pub fn replay_gates(events: &[Event]) -> Result<BTreeMap<Uuid, GateSummary>> {
    Ok(replay_state(events)?.gates)
}

pub fn replay_rebases(events: &[Event]) -> Result<Vec<RebaseSummary>> {
    Ok(replay_state(events)?.rebases)
}

pub fn replay_conflicts(events: &[Event]) -> Result<BTreeMap<Uuid, ConflictSummary>> {
    Ok(replay_state(events)?.conflicts)
}

pub fn build_task_graph(tasks: &BTreeMap<Uuid, TaskSummary>) -> TaskGraph {
    let task_list: Vec<TaskSummary> = tasks.values().cloned().collect();
    let mut edges = Vec::new();

    for task in tasks.values() {
        for dep_id in &task.dependencies {
            edges.push(TaskEdge {
                from_task: task.id,
                to_task: *dep_id,
                relation: TaskRelation::DependsOn,
            });
        }
        if let Some(discovered_from) = task.discovered_from {
            edges.push(TaskEdge {
                from_task: task.id,
                to_task: discovered_from,
                relation: TaskRelation::DiscoveredFrom,
            });
        }
    }

    TaskGraph {
        tasks: task_list,
        edges,
    }
}

pub fn resolve_target_event<'a>(events: &'a [Event], request: &UndoRequest) -> Result<&'a Event> {
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
                    event
                        .timestamp
                        .parse::<u128>()
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

pub fn to_undo_mode(request: &UndoRequest, resolved_target_id: Uuid) -> UndoMode {
    match request {
        UndoRequest::Last => UndoMode::Last,
        UndoRequest::N(n) => UndoMode::N(*n),
        UndoRequest::To(_) => UndoMode::To(resolved_target_id),
        UndoRequest::Since(duration) => UndoMode::SinceNanos(duration.as_nanos()),
    }
}

pub fn previous_checkpoint_before(events: &[Event], target_event_id: Uuid) -> Option<Event> {
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

/// Walk the `parent_checkpoint_event` chain starting from `start_id`,
/// following `steps` parent links. Returns the ancestor checkpoint event.
pub fn walk_checkpoint_ancestor(events: &[Event], start_id: Uuid, steps: usize) -> Result<Event> {
    if steps == 0 {
        bail!("steps must be >= 1");
    }

    let event_map: HashMap<Uuid, &Event> = events.iter().map(|e| (e.id, e)).collect();

    let start = event_map
        .get(&start_id)
        .ok_or_else(|| anyhow!("start event {} not found", start_id))?;

    let mut current = *start;
    for step in 0..steps {
        let EventKind::Checkpoint(ref cp) = current.kind else {
            bail!(
                "event {} is not a checkpoint; cannot follow parent chain",
                current.id
            );
        };

        let parent_id = cp.parent_checkpoint_event.ok_or_else(|| {
            anyhow!(
                "only {} checkpoint(s) exist before HEAD",
                step
            )
        })?;

        current = event_map
            .get(&parent_id)
            .ok_or_else(|| {
                anyhow!(
                    "parent checkpoint event {} not found in event log",
                    parent_id
                )
            })?;
    }

    Ok(current.clone())
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
        "w" => Duration::from_secs(amount.saturating_mul(60 * 60 * 24 * 7)),
        _ => bail!("unsupported duration unit `{}` (use s, m, h, d, w)", unit),
    };

    Ok(duration)
}

#[cfg(test)]
mod tests {
    use fl_storage::{
        ApiCallRecord, CheckpointEvent, DecisionAction, DecisionEvent, EventKind,
        ExplorationEvent, ResourceUsageEvent, SessionAction, SessionEvent, TaskAction, TaskEvent,
        UndoEvent,
    };

    use super::*;

    #[test]
    fn parse_duration_specs() {
        assert_eq!(parse_duration_spec("5m").expect("duration").as_secs(), 300);
        assert_eq!(parse_duration_spec("30").expect("duration").as_secs(), 30);
        assert_eq!(parse_duration_spec("1w").expect("duration").as_secs(), 604800);
    }

    #[test]
    fn replay_state_undo_removes_targeted_exploration_changes() {
        let checkpoint_event = make_event(
            1,
            None,
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp1".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(10),
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
        );
        let exploration_start = make_event(
            2,
            Some(checkpoint_event.id),
            EventKind::Exploration(ExplorationEvent {
                exploration_id: Uuid::from_u128(20),
                title: "exp".to_string(),
                base_checkpoint_event: Some(checkpoint_event.id),
                action: ExplorationAction::Start,
            }),
        );
        let undo = make_event(
            3,
            Some(exploration_start.id),
            EventKind::Undo(UndoEvent {
                target_event_id: exploration_start.id,
                mode: UndoMode::Last,
                restored_checkpoint_event: None,
                file_scope: None,
            }),
        );

        let state = replay_state(&[checkpoint_event, exploration_start, undo]).expect("replay");
        assert_eq!(state.latest_checkpoint_event_id, Some(Uuid::from_u128(1)));
        assert_eq!(
            state.latest_checkpoint_snapshot_id,
            Some(Uuid::from_u128(10))
        );
        assert!(state.explorations.is_empty());
        assert_eq!(
            state.applied_event_ids,
            vec![Uuid::from_u128(1), Uuid::from_u128(3)]
        );
    }

    #[test]
    fn replay_state_uses_restored_checkpoint_from_undo_payload() {
        let cp1 = make_event(
            1,
            None,
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp1".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(11),
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
        );
        let cp2 = make_event(
            2,
            Some(cp1.id),
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp2".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(12),
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
        );
        let restored = make_event(
            3,
            Some(cp2.id),
            EventKind::Checkpoint(CheckpointEvent {
                label: "undo-cp2".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(13),
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
        );
        let undo = make_event(
            4,
            Some(restored.id),
            EventKind::Undo(UndoEvent {
                target_event_id: cp2.id,
                mode: UndoMode::Last,
                restored_checkpoint_event: Some(restored.id),
                file_scope: None,
            }),
        );

        let state = replay_state(&[cp1, cp2, restored.clone(), undo]).expect("replay");
        assert_eq!(state.latest_checkpoint_event_id, Some(restored.id));
        assert_eq!(
            state.latest_checkpoint_snapshot_id,
            Some(Uuid::from_u128(13))
        );
    }

    #[test]
    fn replay_state_is_deterministic() {
        let cp1 = make_event(
            1,
            None,
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp1".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(11),
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
        );
        let cp2 = make_event(
            2,
            Some(cp1.id),
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp2".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(12),
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
        );

        let events = vec![cp1, cp2];
        let first = replay_state(&events).expect("first replay");
        let second = replay_state(&events).expect("second replay");
        assert_eq!(first, second);
    }

    #[test]
    fn replay_state_file_scoped_undo_does_not_rewind_explorations() {
        let cp1 = make_event(
            1,
            None,
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp1".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(11),
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
        );
        let cp2 = make_event(
            2,
            Some(cp1.id),
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp2".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(12),
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
        );
        let exploration_start = make_event(
            3,
            Some(cp2.id),
            EventKind::Exploration(ExplorationEvent {
                exploration_id: Uuid::from_u128(21),
                title: "exp".to_string(),
                base_checkpoint_event: Some(cp2.id),
                action: ExplorationAction::Start,
            }),
        );
        let restored = make_event(
            4,
            Some(exploration_start.id),
            EventKind::Checkpoint(CheckpointEvent {
                label: "undo-file".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(13),
                parent_checkpoint_event: Some(cp2.id),
                snapshot_merkle_root: None,
                ai_intent: None,
                intent_confidence: None,
                files_changed: None,
                category: None,
                scope_label: None,
                structured_description: None,
                git_commit_sha: None,
            }),
        );
        let undo = make_event(
            5,
            Some(restored.id),
            EventKind::Undo(UndoEvent {
                target_event_id: cp2.id,
                mode: UndoMode::To(cp2.id),
                restored_checkpoint_event: Some(restored.id),
                file_scope: Some("src/file.ts".to_string()),
            }),
        );

        let state =
            replay_state(&[cp1, cp2, exploration_start, restored.clone(), undo]).expect("replay");
        assert_eq!(state.latest_checkpoint_event_id, Some(restored.id));
        assert!(state.explorations.contains_key(&Uuid::from_u128(21)));
    }

    #[test]
    fn replay_session_lifecycle() {
        let start = make_event(
            1,
            None,
            EventKind::Session(SessionEvent {
                session_id: Uuid::from_u128(100),
                action: SessionAction::Start,
                agent: "claude".to_string(),
                initiator: Some("user".to_string()),
                task_description: Some("implement feature X".to_string()),
                exploration_id: None,
                result: None,
            }),
        );
        let link = make_event(
            2,
            Some(start.id),
            EventKind::Session(SessionEvent {
                session_id: Uuid::from_u128(100),
                action: SessionAction::Link,
                agent: "claude".to_string(),
                initiator: None,
                task_description: None,
                exploration_id: Some(Uuid::from_u128(200)),
                result: None,
            }),
        );
        let decision = make_event(
            3,
            Some(link.id),
            EventKind::Decision(DecisionEvent {
                session_id: Uuid::from_u128(100),
                exploration_id: Uuid::from_u128(200),
                action: DecisionAction::Kept,
                reason: "tests pass".to_string(),
                confidence: 0.95,
            }),
        );
        let usage = make_event(
            4,
            Some(decision.id),
            EventKind::ResourceUsage(ResourceUsageEvent {
                session_id: Uuid::from_u128(100),
                tokens_consumed: Some(5000),
                runtime_ms: Some(12000),
                api_calls: Some(vec![ApiCallRecord {
                    service: "claude".to_string(),
                    endpoint: "messages".to_string(),
                    count: 3,
                }]),
            }),
        );
        let complete = make_event(
            5,
            Some(usage.id),
            EventKind::Session(SessionEvent {
                session_id: Uuid::from_u128(100),
                action: SessionAction::Complete,
                agent: "claude".to_string(),
                initiator: None,
                task_description: None,
                exploration_id: None,
                result: Some("feature X implemented".to_string()),
            }),
        );

        let state = replay_state(&[start, link, decision, usage, complete]).expect("replay");
        let session = state.sessions.get(&Uuid::from_u128(100)).expect("session");

        assert_eq!(session.agent, "claude");
        assert_eq!(session.initiator.as_deref(), Some("user"));
        assert_eq!(session.status, SessionStatus::Completed);
        assert_eq!(session.explorations, vec![Uuid::from_u128(200)]);
        assert_eq!(session.decisions.len(), 1);
        assert_eq!(session.decisions[0].action, DecisionAction::Kept);
        assert_eq!(session.resource_usage.total_tokens, 5000);
        assert_eq!(session.resource_usage.total_runtime_ms, 12000);
        assert_eq!(session.resource_usage.api_calls.len(), 1);
        assert_eq!(session.result.as_deref(), Some("feature X implemented"));
        assert!(session.completed_at.is_some());
    }

    #[test]
    fn replay_session_fail() {
        let start = make_event(
            1,
            None,
            EventKind::Session(SessionEvent {
                session_id: Uuid::from_u128(101),
                action: SessionAction::Start,
                agent: "agent-1".to_string(),
                initiator: None,
                task_description: Some("risky task".to_string()),
                exploration_id: None,
                result: None,
            }),
        );
        let fail = make_event(
            2,
            Some(start.id),
            EventKind::Session(SessionEvent {
                session_id: Uuid::from_u128(101),
                action: SessionAction::Fail,
                agent: "agent-1".to_string(),
                initiator: None,
                task_description: None,
                exploration_id: None,
                result: Some("compilation errors".to_string()),
            }),
        );

        let state = replay_state(&[start, fail]).expect("replay");
        let session = state.sessions.get(&Uuid::from_u128(101)).expect("session");
        assert_eq!(session.status, SessionStatus::Failed);
        assert_eq!(session.result.as_deref(), Some("compilation errors"));
    }

    #[test]
    fn replay_session_unlink() {
        let start = make_event(
            1,
            None,
            EventKind::Session(SessionEvent {
                session_id: Uuid::from_u128(102),
                action: SessionAction::Start,
                agent: "bot".to_string(),
                initiator: None,
                task_description: None,
                exploration_id: None,
                result: None,
            }),
        );
        let link = make_event(
            2,
            Some(start.id),
            EventKind::Session(SessionEvent {
                session_id: Uuid::from_u128(102),
                action: SessionAction::Link,
                agent: "bot".to_string(),
                initiator: None,
                task_description: None,
                exploration_id: Some(Uuid::from_u128(300)),
                result: None,
            }),
        );
        let unlink = make_event(
            3,
            Some(link.id),
            EventKind::Session(SessionEvent {
                session_id: Uuid::from_u128(102),
                action: SessionAction::Unlink,
                agent: "bot".to_string(),
                initiator: None,
                task_description: None,
                exploration_id: Some(Uuid::from_u128(300)),
                result: None,
            }),
        );

        let state = replay_state(&[start, link, unlink]).expect("replay");
        let session = state.sessions.get(&Uuid::from_u128(102)).expect("session");
        assert!(session.explorations.is_empty());
    }

    #[test]
    fn replay_sessions_accumulates_resource_usage() {
        let start = make_event(
            1,
            None,
            EventKind::Session(SessionEvent {
                session_id: Uuid::from_u128(103),
                action: SessionAction::Start,
                agent: "agent".to_string(),
                initiator: None,
                task_description: None,
                exploration_id: None,
                result: None,
            }),
        );
        let usage1 = make_event(
            2,
            Some(start.id),
            EventKind::ResourceUsage(ResourceUsageEvent {
                session_id: Uuid::from_u128(103),
                tokens_consumed: Some(1000),
                runtime_ms: Some(500),
                api_calls: None,
            }),
        );
        let usage2 = make_event(
            3,
            Some(usage1.id),
            EventKind::ResourceUsage(ResourceUsageEvent {
                session_id: Uuid::from_u128(103),
                tokens_consumed: Some(2000),
                runtime_ms: Some(700),
                api_calls: None,
            }),
        );

        let sessions = replay_sessions(&[start, usage1, usage2]).expect("replay");
        let session = sessions.get(&Uuid::from_u128(103)).expect("session");
        assert_eq!(session.resource_usage.total_tokens, 3000);
        assert_eq!(session.resource_usage.total_runtime_ms, 1200);
    }

    fn make_task_event(task_id: u128, action: TaskAction) -> TaskEvent {
        TaskEvent {
            task_id: Uuid::from_u128(task_id),
            action,
            title: format!("task-{}", task_id),
            description: None,
            dependencies: Vec::new(),
            assignee: None,
            result: None,
            linked_events: Vec::new(),
            discovered_from: None,
            allowed_paths: Vec::new(),
        }
    }

    #[test]
    fn replay_task_lifecycle() {
        let create = make_event(
            1,
            None,
            EventKind::Task(make_task_event(500, TaskAction::Create)),
        );
        let mut claim_payload = make_task_event(500, TaskAction::Claim);
        claim_payload.assignee = Some("agent-1".to_string());
        let claim = make_event(2, Some(create.id), EventKind::Task(claim_payload));
        let mut complete_payload = make_task_event(500, TaskAction::Complete);
        complete_payload.result = Some("done".to_string());
        let complete = make_event(3, Some(claim.id), EventKind::Task(complete_payload));

        let state = replay_state(&[create, claim, complete]).expect("replay");
        let task = state.tasks.get(&Uuid::from_u128(500)).expect("task");

        assert_eq!(task.title, "task-500");
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.assignee.as_deref(), Some("agent-1"));
        assert!(task.completed_at.is_some());
        assert_eq!(task.result.as_deref(), Some("done"));
    }

    #[test]
    fn replay_task_dependencies_compute_dependents() {
        let mut create1 = make_task_event(600, TaskAction::Create);
        create1.title = "parent task".to_string();
        let ev1 = make_event(1, None, EventKind::Task(create1));

        let mut create2 = make_task_event(601, TaskAction::Create);
        create2.title = "child task".to_string();
        create2.dependencies = vec![Uuid::from_u128(600)];
        let ev2 = make_event(2, Some(ev1.id), EventKind::Task(create2));

        let state = replay_state(&[ev1, ev2]).expect("replay");
        let parent = state.tasks.get(&Uuid::from_u128(600)).expect("parent");
        let child = state.tasks.get(&Uuid::from_u128(601)).expect("child");

        assert_eq!(parent.dependents, vec![Uuid::from_u128(601)]);
        assert_eq!(child.dependencies, vec![Uuid::from_u128(600)]);
    }

    #[test]
    fn replay_task_is_ready() {
        let mut create1 = make_task_event(700, TaskAction::Create);
        create1.title = "dep".to_string();
        let ev1 = make_event(1, None, EventKind::Task(create1));

        let mut create2 = make_task_event(701, TaskAction::Create);
        create2.title = "blocked".to_string();
        create2.dependencies = vec![Uuid::from_u128(700)];
        let ev2 = make_event(2, Some(ev1.id), EventKind::Task(create2));

        let ev2_id = ev2.id;
        let state = replay_state(&[ev1, ev2]).expect("replay");
        let blocked = state.tasks.get(&Uuid::from_u128(701)).expect("blocked");
        assert!(!blocked.is_ready(&state.tasks));

        // Now complete the dependency — rebuild events since originals were moved
        let mut complete = make_task_event(700, TaskAction::Complete);
        complete.result = Some("done".to_string());
        let ev3 = make_event(3, Some(ev2_id), EventKind::Task(complete));

        let ev1_clone = make_event(
            1,
            None,
            EventKind::Task({
                let mut t = make_task_event(700, TaskAction::Create);
                t.title = "dep".to_string();
                t
            }),
        );
        let ev2_clone = make_event(
            2,
            Some(ev1_clone.id),
            EventKind::Task({
                let mut t = make_task_event(701, TaskAction::Create);
                t.title = "blocked".to_string();
                t.dependencies = vec![Uuid::from_u128(700)];
                t
            }),
        );
        let state2 = replay_state(&[ev1_clone, ev2_clone, ev3]).expect("replay");
        let unblocked = state2.tasks.get(&Uuid::from_u128(701)).expect("unblocked");
        assert!(unblocked.is_ready(&state2.tasks));
    }

    #[test]
    fn replay_task_fail() {
        let create = make_event(
            1,
            None,
            EventKind::Task(make_task_event(800, TaskAction::Create)),
        );
        let mut fail_payload = make_task_event(800, TaskAction::Fail);
        fail_payload.result = Some("compilation error".to_string());
        let fail = make_event(2, Some(create.id), EventKind::Task(fail_payload));

        let state = replay_state(&[create, fail]).expect("replay");
        let task = state.tasks.get(&Uuid::from_u128(800)).expect("task");
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.result.as_deref(), Some("compilation error"));
    }

    #[test]
    fn replay_task_unclaim() {
        let create = make_event(
            1,
            None,
            EventKind::Task(make_task_event(900, TaskAction::Create)),
        );
        let mut claim_payload = make_task_event(900, TaskAction::Claim);
        claim_payload.assignee = Some("agent-1".to_string());
        let claim = make_event(2, Some(create.id), EventKind::Task(claim_payload));
        let unclaim = make_event(
            3,
            Some(claim.id),
            EventKind::Task(make_task_event(900, TaskAction::Unclaim)),
        );

        let state = replay_state(&[create, claim, unclaim]).expect("replay");
        let task = state.tasks.get(&Uuid::from_u128(900)).expect("task");
        assert_eq!(task.status, TaskStatus::Open);
        assert!(task.assignee.is_none());
    }

    #[test]
    fn build_task_graph_produces_edges() {
        let mut create1 = make_task_event(1000, TaskAction::Create);
        create1.title = "first".to_string();
        let ev1 = make_event(1, None, EventKind::Task(create1));

        let mut create2 = make_task_event(1001, TaskAction::Create);
        create2.title = "second".to_string();
        create2.dependencies = vec![Uuid::from_u128(1000)];
        create2.discovered_from = Some(Uuid::from_u128(1000));
        let ev2 = make_event(2, Some(ev1.id), EventKind::Task(create2));

        let state = replay_state(&[ev1, ev2]).expect("replay");
        let graph = build_task_graph(&state.tasks);

        assert_eq!(graph.tasks.len(), 2);
        assert_eq!(graph.edges.len(), 2); // 1 DependsOn + 1 DiscoveredFrom
    }

    #[test]
    fn replay_presence_heartbeat_and_depart() {
        let heartbeat = make_event(
            1,
            None,
            EventKind::Presence(fl_storage::PresenceEvent {
                actor: "agent-1".to_string(),
                workspace: "ws1".to_string(),
                action: fl_storage::PresenceAction::Heartbeat,
                active_files: vec!["src/main.rs".to_string()],
                active_symbols: vec![],
                intent: Some("refactoring".to_string()),
                ttl_secs: 300,
            }),
        );
        let state = replay_state(&[heartbeat.clone()]).expect("replay");
        assert_eq!(state.presence.len(), 1);
        let p = state.presence.get("agent-1@ws1").expect("presence");
        assert_eq!(p.actor, "agent-1");
        assert_eq!(p.workspace, "ws1");
        assert_eq!(p.active_files, vec!["src/main.rs"]);
        assert_eq!(p.intent.as_deref(), Some("refactoring"));
        assert_eq!(p.ttl.as_secs(), 300);

        let depart = make_event(
            2,
            Some(heartbeat.id),
            EventKind::Presence(fl_storage::PresenceEvent {
                actor: "agent-1".to_string(),
                workspace: "ws1".to_string(),
                action: fl_storage::PresenceAction::Depart,
                active_files: Vec::new(),
                active_symbols: Vec::new(),
                intent: None,
                ttl_secs: 0,
            }),
        );
        let state2 = replay_state(&[heartbeat, depart]).expect("replay");
        assert!(state2.presence.is_empty());
    }

    #[test]
    fn replay_lock_acquire_and_release() {
        let acquire = make_event(
            1,
            None,
            EventKind::Lock(fl_storage::LockEvent {
                lock_id: Uuid::from_u128(50),
                resource: "src/api.ts".to_string(),
                holder: "agent-1".to_string(),
                action: fl_storage::LockAction::Acquire,
                ttl_secs: 600,
            }),
        );
        let state = replay_state(&[acquire.clone()]).expect("replay");
        assert_eq!(state.locks.len(), 1);
        let lock = state.locks.get(&Uuid::from_u128(50)).expect("lock");
        assert_eq!(lock.resource, "src/api.ts");
        assert_eq!(lock.holder, "agent-1");
        assert_eq!(lock.status, fl_collab::LockStatus::Held);
        assert_eq!(lock.ttl.as_secs(), 600);

        let release = make_event(
            2,
            Some(acquire.id),
            EventKind::Lock(fl_storage::LockEvent {
                lock_id: Uuid::from_u128(50),
                resource: "src/api.ts".to_string(),
                holder: "agent-1".to_string(),
                action: fl_storage::LockAction::Release,
                ttl_secs: 0,
            }),
        );
        let state2 = replay_state(&[acquire, release]).expect("replay");
        let lock2 = state2.locks.get(&Uuid::from_u128(50)).expect("lock");
        assert_eq!(lock2.status, fl_collab::LockStatus::Released);
        assert!(lock2.released_at.is_some());
    }

    #[test]
    fn replay_subscription_lifecycle() {
        let subscribe = make_event(
            1,
            None,
            EventKind::Subscription(fl_storage::SubscriptionEvent {
                subscription_id: Uuid::from_u128(60),
                actor: "agent-1".to_string(),
                action: fl_storage::SubscriptionAction::Subscribe,
                filter: Some(fl_storage::SubscriptionFilter {
                    paths: vec!["src/api/*".to_string()],
                    symbols: vec!["processPayment".to_string()],
                    modules: Vec::new(),
                }),
                notify: Some(fl_storage::NotifyConfig::Batched),
            }),
        );
        let state = replay_state(&[subscribe.clone()]).expect("replay");
        assert_eq!(state.subscriptions.len(), 1);
        let sub = state.subscriptions.get(&Uuid::from_u128(60)).expect("sub");
        assert_eq!(sub.actor, "agent-1");
        assert_eq!(sub.status, fl_collab::SubscriptionStatus::Active);
        assert_eq!(sub.paths, vec!["src/api/*"]);
        assert_eq!(sub.symbols, vec!["processPayment"]);
        assert_eq!(sub.notify, fl_collab::SubscriptionNotify::Batched);

        let unsubscribe = make_event(
            2,
            Some(subscribe.id),
            EventKind::Subscription(fl_storage::SubscriptionEvent {
                subscription_id: Uuid::from_u128(60),
                actor: "agent-1".to_string(),
                action: fl_storage::SubscriptionAction::Unsubscribe,
                filter: None,
                notify: None,
            }),
        );
        let state2 = replay_state(&[subscribe, unsubscribe]).expect("replay");
        let sub2 = state2.subscriptions.get(&Uuid::from_u128(60)).expect("sub");
        assert_eq!(sub2.status, fl_collab::SubscriptionStatus::Cancelled);
        assert!(sub2.cancelled_at.is_some());
    }

    #[test]
    fn replay_gate_create_approve_reject() {
        let create = make_event(
            1,
            None,
            EventKind::Gate(fl_storage::GateEvent {
                gate_id: Uuid::from_u128(70),
                action: fl_storage::GateAction::Create,
                condition: Some(fl_storage::GateCondition::FileTouched(
                    "src/payments/*".to_string(),
                )),
                policy: Some(fl_storage::GatePolicy::Block),
                approved_by: None,
                reason: None,
            }),
        );
        let state = replay_state(&[create.clone()]).expect("replay");
        assert_eq!(state.gates.len(), 1);
        let gate = state.gates.get(&Uuid::from_u128(70)).expect("gate");
        assert_eq!(gate.status, fl_collab::GateStatus::Active);
        assert_eq!(
            gate.condition,
            fl_collab::GateConditionKind::FileTouched("src/payments/*".to_string())
        );
        assert_eq!(gate.policy, fl_collab::GatePolicyKind::Block);

        let approve = make_event(
            2,
            Some(create.id),
            EventKind::Gate(fl_storage::GateEvent {
                gate_id: Uuid::from_u128(70),
                action: fl_storage::GateAction::Approve,
                condition: None,
                policy: None,
                approved_by: Some("reviewer".to_string()),
                reason: Some("looks good".to_string()),
            }),
        );
        let state2 = replay_state(&[create.clone(), approve]).expect("replay");
        let gate2 = state2.gates.get(&Uuid::from_u128(70)).expect("gate");
        assert_eq!(gate2.status, fl_collab::GateStatus::Approved);
        assert_eq!(gate2.approved_by.as_deref(), Some("reviewer"));
        assert_eq!(gate2.reason.as_deref(), Some("looks good"));

        // Test reject path
        let reject = make_event(
            3,
            Some(create.id),
            EventKind::Gate(fl_storage::GateEvent {
                gate_id: Uuid::from_u128(71),
                action: fl_storage::GateAction::Create,
                condition: Some(fl_storage::GateCondition::SecuritySensitive),
                policy: Some(fl_storage::GatePolicy::QueueAndContinue),
                approved_by: None,
                reason: None,
            }),
        );
        let reject_ev = make_event(
            4,
            Some(reject.id),
            EventKind::Gate(fl_storage::GateEvent {
                gate_id: Uuid::from_u128(71),
                action: fl_storage::GateAction::Reject,
                condition: None,
                policy: None,
                approved_by: Some("admin".to_string()),
                reason: Some("too risky".to_string()),
            }),
        );
        let state3 = replay_state(&[reject, reject_ev]).expect("replay");
        let gate3 = state3.gates.get(&Uuid::from_u128(71)).expect("gate");
        assert_eq!(gate3.status, fl_collab::GateStatus::Rejected);
    }

    #[test]
    fn lock_conflict_detection() {
        let mut locks = std::collections::BTreeMap::new();
        locks.insert(
            Uuid::from_u128(1),
            fl_collab::LockSummary {
                id: Uuid::from_u128(1),
                resource: "src/file.ts".to_string(),
                holder: "agent-1".to_string(),
                status: fl_collab::LockStatus::Held,
                ttl: std::time::Duration::from_secs(600),
                acquired_at: "1000000000000000000".to_string(),
                released_at: None,
            },
        );

        // Lock is held and not expired -> conflict
        let now = 1000000000100000000u128; // 0.1s later
        assert!(fl_collab::can_acquire_lock("src/file.ts", &locks, now).is_err());

        // Different resource -> ok
        assert!(fl_collab::can_acquire_lock("src/other.ts", &locks, now).is_ok());

        // Lock expired -> ok
        let far_future = 1000000000000000000u128 + 601_000_000_000; // 601s later (> 600s TTL)
        assert!(fl_collab::can_acquire_lock("src/file.ts", &locks, far_future).is_ok());
    }

    #[test]
    fn subscription_path_matching() {
        let sub = fl_collab::SubscriptionSummary {
            id: Uuid::from_u128(1),
            actor: "agent".to_string(),
            status: fl_collab::SubscriptionStatus::Active,
            paths: vec!["src/api/*".to_string(), "README.md".to_string()],
            symbols: Vec::new(),
            modules: Vec::new(),
            notify: fl_collab::SubscriptionNotify::Immediate,
            created_at: "0".to_string(),
            cancelled_at: None,
        };

        assert!(fl_collab::subscription_matches_path(&sub, "src/api/handler.ts"));
        assert!(fl_collab::subscription_matches_path(&sub, "src/api/"));
        assert!(fl_collab::subscription_matches_path(&sub, "README.md"));
        assert!(!fl_collab::subscription_matches_path(&sub, "src/lib.rs"));
    }

    #[test]
    fn gate_path_checking() {
        let mut gates = std::collections::BTreeMap::new();
        gates.insert(
            Uuid::from_u128(1),
            fl_collab::GateSummary {
                id: Uuid::from_u128(1),
                status: fl_collab::GateStatus::Active,
                condition: fl_collab::GateConditionKind::FileTouched("src/payments/*".to_string()),
                policy: fl_collab::GatePolicyKind::Block,
                approved_by: None,
                reason: None,
                created_at: "0".to_string(),
                resolved_at: None,
            },
        );

        let blocking = fl_collab::check_gates_for_path("src/payments/stripe.ts", &gates);
        assert_eq!(blocking.len(), 1);

        let not_blocking = fl_collab::check_gates_for_path("src/utils.ts", &gates);
        assert!(not_blocking.is_empty());
    }

    #[test]
    fn replay_rebase_event() {
        let rebase = make_event(
            1,
            None,
            EventKind::Rebase(fl_storage::RebaseEvent {
                workspace: "dev".to_string(),
                old_base_event: Uuid::from_u128(100),
                new_base_event: Uuid::from_u128(200),
                files_merged: vec!["src/main.ts".to_string()],
                conflicts_found: 0,
                auto: true,
            }),
        );

        let state = replay_state(&[rebase]).unwrap();
        assert_eq!(state.rebases.len(), 1);
        assert_eq!(state.rebases[0].workspace, "dev");
        assert_eq!(state.rebases[0].files_merged.len(), 1);
        assert!(state.rebases[0].auto);
        assert_eq!(state.rebases[0].conflicts_found, 0);
    }

    #[test]
    fn replay_conflict_resolution_workflow() {
        let conflict_id = Uuid::from_u128(42);

        let detect = make_event(
            1,
            None,
            EventKind::ConflictResolution(fl_storage::ConflictResolutionEvent {
                conflict_id,
                action: fl_storage::ConflictAction::Detect,
                workspace: Some("dev".to_string()),
                path: Some("src/main.ts".to_string()),
                symbol: Some("handleClick".to_string()),
                classification: None,
                suggestion: None,
                resolution: None,
                resolved_by: None,
                verified: None,
                reason: None,
            }),
        );

        let classify = make_event(
            2,
            Some(Uuid::from_u128(1)),
            EventKind::ConflictResolution(fl_storage::ConflictResolutionEvent {
                conflict_id,
                action: fl_storage::ConflictAction::Classify,
                workspace: Some("dev".to_string()),
                path: Some("src/main.ts".to_string()),
                symbol: Some("handleClick".to_string()),
                classification: Some("DivergentEdit".to_string()),
                suggestion: None,
                resolution: None,
                resolved_by: None,
                verified: None,
                reason: None,
            }),
        );

        let suggest = make_event(
            3,
            Some(Uuid::from_u128(2)),
            EventKind::ConflictResolution(fl_storage::ConflictResolutionEvent {
                conflict_id,
                action: fl_storage::ConflictAction::Suggest,
                workspace: Some("dev".to_string()),
                path: Some("src/main.ts".to_string()),
                symbol: Some("handleClick".to_string()),
                classification: Some("DivergentEdit".to_string()),
                suggestion: Some("Take left side".to_string()),
                resolution: None,
                resolved_by: None,
                verified: None,
                reason: None,
            }),
        );

        let resolve = make_event(
            4,
            Some(Uuid::from_u128(3)),
            EventKind::ConflictResolution(fl_storage::ConflictResolutionEvent {
                conflict_id,
                action: fl_storage::ConflictAction::Resolve,
                workspace: Some("dev".to_string()),
                path: Some("src/main.ts".to_string()),
                symbol: Some("handleClick".to_string()),
                classification: Some("DivergentEdit".to_string()),
                suggestion: Some("Take left side".to_string()),
                resolution: Some("Used left version".to_string()),
                resolved_by: Some("alice".to_string()),
                verified: None,
                reason: None,
            }),
        );

        let verify = make_event(
            5,
            Some(Uuid::from_u128(4)),
            EventKind::ConflictResolution(fl_storage::ConflictResolutionEvent {
                conflict_id,
                action: fl_storage::ConflictAction::Verify,
                workspace: Some("dev".to_string()),
                path: Some("src/main.ts".to_string()),
                symbol: Some("handleClick".to_string()),
                classification: Some("DivergentEdit".to_string()),
                suggestion: None,
                resolution: Some("Used left version".to_string()),
                resolved_by: Some("alice".to_string()),
                verified: Some(true),
                reason: None,
            }),
        );

        let record = make_event(
            6,
            Some(Uuid::from_u128(5)),
            EventKind::ConflictResolution(fl_storage::ConflictResolutionEvent {
                conflict_id,
                action: fl_storage::ConflictAction::Record,
                workspace: Some("dev".to_string()),
                path: Some("src/main.ts".to_string()),
                symbol: Some("handleClick".to_string()),
                classification: Some("DivergentEdit".to_string()),
                suggestion: None,
                resolution: Some("Used left version".to_string()),
                resolved_by: Some("alice".to_string()),
                verified: Some(true),
                reason: Some("Confirmed fix".to_string()),
            }),
        );

        // After detect
        let state = replay_state(&[detect.clone()]).unwrap();
        assert_eq!(state.conflicts.len(), 1);
        let c = state.conflicts.get(&conflict_id).unwrap();
        assert_eq!(c.status, ConflictStatus::Detected);
        assert_eq!(c.path, "src/main.ts");

        // After classify
        let state = replay_state(&[detect.clone(), classify.clone()]).unwrap();
        let c = state.conflicts.get(&conflict_id).unwrap();
        assert_eq!(c.status, ConflictStatus::Classified);
        assert_eq!(c.classification.as_deref(), Some("DivergentEdit"));

        // After suggest
        let state = replay_state(&[detect.clone(), classify.clone(), suggest.clone()]).unwrap();
        let c = state.conflicts.get(&conflict_id).unwrap();
        assert_eq!(c.status, ConflictStatus::Suggested);
        assert_eq!(c.suggestion.as_deref(), Some("Take left side"));

        // Full workflow
        let state = replay_state(&[detect, classify, suggest, resolve, verify, record]).unwrap();
        let c = state.conflicts.get(&conflict_id).unwrap();
        assert_eq!(c.status, ConflictStatus::Recorded);
        assert!(c.verified);
        assert_eq!(c.resolved_by.as_deref(), Some("alice"));
    }

    #[test]
    fn replay_state_incremental_matches_full_replay() {
        let checkpoint_event = make_event(
            1,
            None,
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp1".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(10),
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
        );
        let exploration_start = make_event(
            2,
            Some(checkpoint_event.id),
            EventKind::Exploration(ExplorationEvent {
                exploration_id: Uuid::from_u128(20),
                title: "exp".to_string(),
                base_checkpoint_event: Some(checkpoint_event.id),
                action: ExplorationAction::Start,
            }),
        );
        let exploration_promote = make_event(
            3,
            Some(exploration_start.id),
            EventKind::Exploration(ExplorationEvent {
                exploration_id: Uuid::from_u128(20),
                title: "".to_string(),
                base_checkpoint_event: None,
                action: ExplorationAction::Promote,
            }),
        );

        let events = vec![checkpoint_event, exploration_start, exploration_promote];

        // Full replay
        let full_state = replay_state(&events).unwrap();

        // Incremental replay from index 2 (only the promotion event)
        let base_state = replay_state(&events[..2]).unwrap();
        let incremental_state = replay_state_incremental(&events, 2, base_state).unwrap();

        // They should match
        assert_eq!(full_state.explorations, incremental_state.explorations);
        assert_eq!(full_state.applied_event_ids, incremental_state.applied_event_ids);

        // Verify the exploration was promoted
        let exploration = incremental_state.explorations.get(&Uuid::from_u128(20)).unwrap();
        assert_eq!(exploration.status, ExplorationStatus::Promoted);
    }

    #[test]
    fn walk_checkpoint_ancestor_chains_correctly() {
        let cp1 = make_event(
            1,
            None,
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp1".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(101),
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
        );
        let cp2 = make_event(
            2,
            Some(cp1.id),
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp2".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(102),
                parent_checkpoint_event: Some(cp1.id),
                snapshot_merkle_root: None,
                ai_intent: None,
                intent_confidence: None,
                files_changed: None,
                category: None,
                scope_label: None,
                structured_description: None,
                git_commit_sha: None,
            }),
        );
        let cp3 = make_event(
            3,
            Some(cp2.id),
            EventKind::Checkpoint(CheckpointEvent {
                label: "cp3".to_string(),
                message: None,
                snapshot_id: Uuid::from_u128(103),
                parent_checkpoint_event: Some(cp2.id),
                snapshot_merkle_root: None,
                ai_intent: None,
                intent_confidence: None,
                files_changed: None,
                category: None,
                scope_label: None,
                structured_description: None,
                git_commit_sha: None,
            }),
        );

        let events = vec![cp1.clone(), cp2.clone(), cp3.clone()];

        // 1 step from cp3 -> cp2
        let result = walk_checkpoint_ancestor(&events, cp3.id, 1).unwrap();
        assert_eq!(result.id, cp2.id);

        // 2 steps from cp3 -> cp1
        let result = walk_checkpoint_ancestor(&events, cp3.id, 2).unwrap();
        assert_eq!(result.id, cp1.id);

        // 3 steps from cp3 -> error (cp1 has no parent)
        let err = walk_checkpoint_ancestor(&events, cp3.id, 3).unwrap_err();
        assert!(
            err.to_string().contains("checkpoint(s) exist before HEAD"),
            "unexpected error: {}",
            err
        );
    }

    fn make_event(id: u128, parent_id: Option<Uuid>, kind: EventKind) -> Event {
        Event {
            id: Uuid::from_u128(id),
            timestamp: format!("1739571600000000{}", id),
            actor: "tester".to_string(),
            parent_id,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            kind,
        }
    }
}
