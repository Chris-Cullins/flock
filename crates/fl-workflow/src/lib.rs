use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use fl_storage::{Event, EventKind, ExplorationAction, UndoMode};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedState {
    pub latest_checkpoint_event_id: Option<Uuid>,
    pub latest_checkpoint_snapshot_id: Option<Uuid>,
    pub explorations: BTreeMap<Uuid, ExplorationSummary>,
    pub applied_event_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Default)]
struct ReplayAccumulator {
    latest_checkpoint_event_id: Option<Uuid>,
    latest_checkpoint_snapshot_id: Option<Uuid>,
    explorations: BTreeMap<Uuid, ExplorationSummary>,
    applied_event_ids: Vec<Uuid>,
}

impl ReplayAccumulator {
    fn into_state(self) -> ReplayedState {
        ReplayedState {
            latest_checkpoint_event_id: self.latest_checkpoint_event_id,
            latest_checkpoint_snapshot_id: self.latest_checkpoint_snapshot_id,
            explorations: self.explorations,
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
    use fl_storage::{CheckpointEvent, EventKind, ExplorationEvent, UndoEvent};

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
            }),
        );
        let undo = make_event(
            4,
            Some(restored.id),
            EventKind::Undo(UndoEvent {
                target_event_id: cp2.id,
                mode: UndoMode::Last,
                restored_checkpoint_event: Some(restored.id),
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
            }),
        );

        let events = vec![cp1, cp2];
        let first = replay_state(&events).expect("first replay");
        let second = replay_state(&events).expect("second replay");
        assert_eq!(first, second);
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
