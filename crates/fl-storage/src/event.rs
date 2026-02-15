use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CURRENT_EVENT_SCHEMA_VERSION: u32 = 3;

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
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "payload")]
pub enum EventKind {
    Checkpoint(CheckpointEvent),
    Exploration(ExplorationEvent),
    Undo(UndoEvent),
    GitBridge(GitBridgeEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointEvent {
    pub label: String,
    pub message: Option<String>,
    pub snapshot_id: Uuid,
    #[serde(default)]
    pub parent_checkpoint_event: Option<Uuid>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UndoEvent {
    pub target_event_id: Uuid,
    pub mode: UndoMode,
    pub restored_checkpoint_event: Option<Uuid>,
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
