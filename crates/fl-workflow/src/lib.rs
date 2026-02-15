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

pub fn replay_explorations(events: &[Event]) -> BTreeMap<Uuid, ExplorationSummary> {
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
    use super::*;

    #[test]
    fn parse_duration_specs() {
        assert_eq!(parse_duration_spec("5m").expect("duration").as_secs(), 300);
        assert_eq!(parse_duration_spec("30").expect("duration").as_secs(), 30);
        assert!(parse_duration_spec("1w").is_err());
    }
}
