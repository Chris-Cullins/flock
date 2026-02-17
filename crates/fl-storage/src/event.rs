use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CURRENT_EVENT_SCHEMA_VERSION: u32 = 15;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub id: Uuid,
    pub timestamp: String,
    pub actor: String,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    #[serde(default)]
    pub signer_public_key: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
    /// BLAKE3 hex hash of the previous event's serialized JSON (hash chain).
    #[serde(default)]
    pub prev_event_hash: Option<String>,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "payload")]
pub enum EventKind {
    Init(InitEvent),
    Checkpoint(CheckpointEvent),
    Exploration(ExplorationEvent),
    Undo(UndoEvent),
    GitBridge(GitBridgeEvent),
    Session(SessionEvent),
    Decision(DecisionEvent),
    ResourceUsage(ResourceUsageEvent),
    Task(TaskEvent),
    Presence(PresenceEvent),
    Lock(LockEvent),
    Subscription(SubscriptionEvent),
    Gate(GateEvent),
    Rebase(RebaseEvent),
    ConflictResolution(ConflictResolutionEvent),
    Hook(HookEvent),
    RemoteSync(RemoteSyncEvent),
    Intelligence(IntelligenceEvent),
    Policy(PolicyEvent),
    Directive(DirectiveEvent),
}

impl Event {
    /// Human-readable name of this event's kind.
    pub fn kind_name(&self) -> &'static str {
        crate::ws_protocol::event_kind_name(&self.kind)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileChangeSummary {
    pub path: String,
    pub change_kind: FileChangeKind,
    #[serde(default)]
    pub lines_added: u32,
    #[serde(default)]
    pub lines_removed: u32,
    /// Number of semantic changes (symbol-level adds/removes/modifications) in this file.
    #[serde(default)]
    pub semantic_changes_count: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InitEvent {
    pub mode: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CheckpointCategory {
    Bugfix,
    Feature,
    Refactor,
    Test,
    Docs,
    Style,
    Chore,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointEvent {
    pub label: String,
    pub message: Option<String>,
    pub snapshot_id: Uuid,
    #[serde(default)]
    pub parent_checkpoint_event: Option<Uuid>,
    #[serde(default)]
    pub snapshot_merkle_root: Option<String>,
    #[serde(default)]
    pub ai_intent: Option<String>,
    #[serde(default)]
    pub intent_confidence: Option<u32>,
    #[serde(default)]
    pub files_changed: Option<Vec<FileChangeSummary>>,
    #[serde(default)]
    pub category: Option<CheckpointCategory>,
    #[serde(default)]
    pub scope_label: Option<String>,
    #[serde(default)]
    pub structured_description: Option<String>,
    /// Git commit SHA for this checkpoint (colocated mode).
    /// When present, the snapshot can be lazily extracted from git
    /// instead of requiring a physical `.flock/snapshots/<uuid>/` directory.
    #[serde(default)]
    pub git_commit_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplorationEvent {
    pub exploration_id: Uuid,
    pub title: String,
    pub base_checkpoint_event: Option<Uuid>,
    pub action: ExplorationAction,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExplorationAction {
    Start,
    Promote,
    Abandon,
    Prune,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UndoEvent {
    pub target_event_id: Uuid,
    pub mode: UndoMode,
    pub restored_checkpoint_event: Option<Uuid>,
    #[serde(default)]
    pub file_scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum UndoMode {
    Last,
    N(usize),
    To(Uuid),
    SinceNanos(u128),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitBridgeEvent {
    pub action: GitBridgeAction,
    pub success: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GitBridgeAction {
    Commit,
    Push,
    Pull,
    Import,
    Export,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionEvent {
    pub session_id: Uuid,
    pub action: SessionAction,
    pub agent: String,
    pub initiator: Option<String>,
    pub task_description: Option<String>,
    pub exploration_id: Option<Uuid>,
    pub result: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionAction {
    Start,
    Link,
    Unlink,
    Complete,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionEvent {
    pub session_id: Uuid,
    pub exploration_id: Uuid,
    pub action: DecisionAction,
    pub reason: String,
    pub confidence: f64,
}

impl Eq for DecisionEvent {}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DecisionAction {
    Kept,
    Discarded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceUsageEvent {
    pub session_id: Uuid,
    pub tokens_consumed: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub api_calls: Option<Vec<ApiCallRecord>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiCallRecord {
    pub service: String,
    pub endpoint: String,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskEvent {
    pub task_id: Uuid,
    pub action: TaskAction,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<Uuid>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub linked_events: Vec<Uuid>,
    #[serde(default)]
    pub discovered_from: Option<Uuid>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskAction {
    Create,
    Claim,
    Unclaim,
    Complete,
    Fail,
    Link,
}

// --- Collaboration events ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresenceEvent {
    pub actor: String,
    pub workspace: String,
    pub action: PresenceAction,
    #[serde(default)]
    pub active_files: Vec<String>,
    #[serde(default)]
    pub active_symbols: Vec<String>,
    #[serde(default)]
    pub intent: Option<String>,
    /// TTL in seconds
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PresenceAction {
    Heartbeat,
    Depart,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockEvent {
    pub lock_id: Uuid,
    pub resource: String,
    pub holder: String,
    pub action: LockAction,
    /// TTL in seconds
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LockAction {
    Acquire,
    Release,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubscriptionEvent {
    pub subscription_id: Uuid,
    pub actor: String,
    pub action: SubscriptionAction,
    #[serde(default)]
    pub filter: Option<SubscriptionFilter>,
    #[serde(default)]
    pub notify: Option<NotifyConfig>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SubscriptionAction {
    Subscribe,
    Unsubscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubscriptionFilter {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub modules: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum NotifyConfig {
    Immediate,
    Batched,
    Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateEvent {
    pub gate_id: Uuid,
    pub action: GateAction,
    #[serde(default)]
    pub condition: Option<GateCondition>,
    #[serde(default)]
    pub policy: Option<GatePolicy>,
    #[serde(default)]
    pub approved_by: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GateAction {
    Create,
    Approve,
    Reject,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GateCondition {
    FileTouched(String),
    SymbolModified(String),
    ImpactExceeds(u32),
    SecuritySensitive,
    AgentConfidenceLow(u32),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GatePolicy {
    Block,
    QueueAndContinue,
}

// --- Rebase and conflict resolution events ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebaseEvent {
    pub workspace: String,
    pub old_base_event: Uuid,
    pub new_base_event: Uuid,
    pub files_merged: Vec<String>,
    pub conflicts_found: usize,
    pub auto: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictResolutionEvent {
    pub conflict_id: Uuid,
    pub action: ConflictAction,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub classification: Option<String>,
    #[serde(default)]
    pub suggestion: Option<String>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub resolved_by: Option<String>,
    #[serde(default)]
    pub verified: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConflictAction {
    Detect,
    Classify,
    Suggest,
    Resolve,
    Verify,
    Record,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookEvent {
    pub hook_point: String,
    pub hook_name: String,
    pub command: String,
    pub success: bool,
    pub duration_ms: u64,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub bypassed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteSyncEvent {
    pub action: RemoteSyncAction,
    pub roost_name: String,
    pub roost_url: String,
    pub success: bool,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub event_count: usize,
    #[serde(default)]
    pub block_count: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoteSyncAction {
    Push,
    Pull,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntelligenceEvent {
    pub action: IntelligenceAction,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
}

impl Eq for IntelligenceEvent {}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IntelligenceAction {
    Query,
    IntentExtraction,
    ConflictSuggestion,
    ConfidenceCalculation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyEvent {
    pub policy_name: String,
    pub policy_category: String,
    pub verdict: PolicyVerdictKind,
    pub operation: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub task_id: Option<Uuid>,
    #[serde(default)]
    pub exploration_id: Option<Uuid>,
    #[serde(default)]
    pub affected_files: Vec<String>,
    /// Structured escalation context when a rate limit triggers PauseAndEscalate.
    #[serde(default)]
    pub escalation_context: Option<EscalationContextEvent>,
}

/// Persisted escalation context for rate limit escalations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EscalationContextEvent {
    pub agent_action: String,
    pub limit_name: String,
    pub current_value: u32,
    pub limit_value: u32,
    #[serde(default)]
    pub exploration_id: Option<Uuid>,
    #[serde(default)]
    pub task_id: Option<Uuid>,
    #[serde(default)]
    pub exploration_history: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PolicyVerdictKind {
    Allow,
    Gate,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectiveEvent {
    pub target_actor: String,
    pub directive: DirectiveKind,
    #[serde(default)]
    pub reason: Option<String>,
    pub issued_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DirectiveKind {
    Pause,
    Resume,
    Redirect { new_task: String },
    Abort { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventRecord {
    pub schema_version: u32,
    pub event: Event,
}

impl EventRecord {
    pub fn from_event(event: &Event) -> Self {
        Self {
            schema_version: CURRENT_EVENT_SCHEMA_VERSION,
            event: event.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct EventSigningPayload<'a> {
    id: Uuid,
    timestamp: &'a str,
    actor: &'a str,
    parent_id: Option<Uuid>,
    kind: &'a EventKind,
}

/// Compute a BLAKE3 hash of the event's serialized JSON (for hash chain).
pub fn compute_event_hash(event: &Event) -> String {
    let record = EventRecord::from_event(event);
    let json = serde_json::to_string(&record).expect("event serialization should not fail");
    blake3::hash(json.as_bytes()).to_hex().to_string()
}

pub fn event_signing_payload(event: &Event) -> Result<Vec<u8>> {
    let payload = EventSigningPayload {
        id: event.id,
        timestamp: &event.timestamp,
        actor: &event.actor,
        parent_id: event.parent_id,
        kind: &event.kind,
    };
    serde_json::to_vec(&payload).context("failed to serialize event signing payload")
}
