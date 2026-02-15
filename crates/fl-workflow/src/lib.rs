use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use fl_storage::{
    ApiCallRecord, DecisionAction, Event, EventKind, ExplorationAction, SessionAction, TaskAction,
    UndoMode,
};
use uuid::Uuid;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorationSummary {
    pub id: Uuid,
    pub title: String,
    pub status: ExplorationStatus,
    pub base_checkpoint_event: Option<Uuid>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionSummary {
    pub exploration_id: Uuid,
    pub action: DecisionAction,
    pub reason: String,
    pub confidence: f64,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskGraph {
    pub tasks: Vec<TaskSummary>,
    pub edges: Vec<TaskEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEdge {
    pub from_task: Uuid,
    pub to_task: Uuid,
    pub relation: TaskRelation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRelation {
    DependsOn,
    DiscoveredFrom,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayedState {
    pub latest_checkpoint_event_id: Option<Uuid>,
    pub latest_checkpoint_snapshot_id: Option<Uuid>,
    pub explorations: BTreeMap<Uuid, ExplorationSummary>,
    pub sessions: BTreeMap<Uuid, SessionSummary>,
    pub tasks: BTreeMap<Uuid, TaskSummary>,
    pub applied_event_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Default)]
struct ReplayAccumulator {
    latest_checkpoint_event_id: Option<Uuid>,
    latest_checkpoint_snapshot_id: Option<Uuid>,
    explorations: BTreeMap<Uuid, ExplorationSummary>,
    sessions: BTreeMap<Uuid, SessionSummary>,
    tasks: BTreeMap<Uuid, TaskSummary>,
    applied_event_ids: Vec<Uuid>,
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
            applied_event_ids: self.applied_event_ids,
        }
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

        match &event.kind {
            EventKind::Checkpoint(checkpoint) => {
                state.latest_checkpoint_event_id = Some(event.id);
                state.latest_checkpoint_snapshot_id = Some(checkpoint.snapshot_id);
            }
            EventKind::Exploration(exploration) => match exploration.action {
                ExplorationAction::Start => {
                    state.explorations.insert(
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
                    if let Some(entry) = state.explorations.get_mut(&exploration.exploration_id) {
                        entry.status = ExplorationStatus::Promoted;
                        entry.updated_at = event.timestamp.clone();
                    }
                }
                ExplorationAction::Abandon => {
                    if let Some(entry) = state.explorations.get_mut(&exploration.exploration_id) {
                        entry.status = ExplorationStatus::Abandoned;
                        entry.updated_at = event.timestamp.clone();
                    }
                }
            },
            EventKind::Undo(undo) => {
                if undo.target_event_id == event.id {
                    bail!("undo event {} cannot target itself", event.id);
                }

                if undo.file_scope.is_none() {
                    let rewound = state_before_event
                        .get(&undo.target_event_id)
                        .cloned()
                        .ok_or_else(|| {
                            anyhow!(
                                "undo event {} targets unknown event {}",
                                event.id,
                                undo.target_event_id
                            )
                        })?;
                    state = rewound;
                }

                if let Some(restored_checkpoint_event) = undo.restored_checkpoint_event {
                    let snapshot_id = checkpoints
                        .get(&restored_checkpoint_event)
                        .copied()
                        .ok_or_else(|| {
                            anyhow!(
                                "undo event {} references unknown restored checkpoint {}",
                                event.id,
                                restored_checkpoint_event
                            )
                        })?;
                    state.latest_checkpoint_event_id = Some(restored_checkpoint_event);
                    state.latest_checkpoint_snapshot_id = Some(snapshot_id);
                    if !state.applied_event_ids.contains(&restored_checkpoint_event) {
                        state.applied_event_ids.push(restored_checkpoint_event);
                    }
                }
            }
            EventKind::GitBridge(_) => {}
            EventKind::Session(session) => match session.action {
                SessionAction::Start => {
                    state.sessions.insert(
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
                    if let Some(entry) = state.sessions.get_mut(&session.session_id) {
                        if let Some(exploration_id) = session.exploration_id {
                            if !entry.explorations.contains(&exploration_id) {
                                entry.explorations.push(exploration_id);
                            }
                        }
                    }
                }
                SessionAction::Unlink => {
                    if let Some(entry) = state.sessions.get_mut(&session.session_id) {
                        if let Some(exploration_id) = session.exploration_id {
                            entry.explorations.retain(|id| *id != exploration_id);
                        }
                    }
                }
                SessionAction::Complete => {
                    if let Some(entry) = state.sessions.get_mut(&session.session_id) {
                        entry.status = SessionStatus::Completed;
                        entry.completed_at = Some(event.timestamp.clone());
                        entry.result = session.result.clone();
                    }
                }
                SessionAction::Fail => {
                    if let Some(entry) = state.sessions.get_mut(&session.session_id) {
                        entry.status = SessionStatus::Failed;
                        entry.completed_at = Some(event.timestamp.clone());
                        entry.result = session.result.clone();
                    }
                }
            },
            EventKind::Decision(decision) => {
                if let Some(entry) = state.sessions.get_mut(&decision.session_id) {
                    entry.decisions.push(DecisionSummary {
                        exploration_id: decision.exploration_id,
                        action: decision.action,
                        reason: decision.reason.clone(),
                        confidence: decision.confidence,
                        timestamp: event.timestamp.clone(),
                    });
                }
            }
            EventKind::ResourceUsage(usage) => {
                if let Some(entry) = state.sessions.get_mut(&usage.session_id) {
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
            EventKind::Task(task) => match task.action {
                TaskAction::Create => {
                    state.tasks.insert(
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
                        },
                    );
                }
                TaskAction::Claim => {
                    if let Some(entry) = state.tasks.get_mut(&task.task_id) {
                        entry.status = TaskStatus::Claimed;
                        entry.assignee = task
                            .assignee
                            .clone()
                            .or_else(|| Some(event.actor.clone()));
                        entry.claimed_at = Some(event.timestamp.clone());
                    }
                }
                TaskAction::Unclaim => {
                    if let Some(entry) = state.tasks.get_mut(&task.task_id) {
                        entry.status = TaskStatus::Open;
                        entry.assignee = None;
                        entry.claimed_at = None;
                    }
                }
                TaskAction::Complete => {
                    if let Some(entry) = state.tasks.get_mut(&task.task_id) {
                        entry.status = TaskStatus::Completed;
                        entry.completed_at = Some(event.timestamp.clone());
                        entry.result = task.result.clone();
                    }
                }
                TaskAction::Fail => {
                    if let Some(entry) = state.tasks.get_mut(&task.task_id) {
                        entry.status = TaskStatus::Failed;
                        entry.completed_at = Some(event.timestamp.clone());
                        entry.result = task.result.clone();
                    }
                }
                TaskAction::Link => {
                    if let Some(entry) = state.tasks.get_mut(&task.task_id) {
                        for linked in &task.linked_events {
                            if !entry.linked_events.contains(linked) {
                                entry.linked_events.push(*linked);
                            }
                        }
                    }
                }
            },
        }

        if !state.applied_event_ids.contains(&event.id) {
            state.applied_event_ids.push(event.id);
        }
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
        assert!(parse_duration_spec("1w").is_err());
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

    fn make_event(id: u128, parent_id: Option<Uuid>, kind: EventKind) -> Event {
        Event {
            id: Uuid::from_u128(id),
            timestamp: format!("1739571600000000{}", id),
            actor: "tester".to_string(),
            parent_id,
            signer_public_key: None,
            signature: None,
            kind,
        }
    }
}
