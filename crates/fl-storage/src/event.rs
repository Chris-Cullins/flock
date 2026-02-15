use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub timestamp: String,
    pub actor: String,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum EventKind {
    Checkpoint(CheckpointEvent),
    Exploration(ExplorationEvent),
    Undo(UndoEvent),
    GitBridge(GitBridgeEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEvent {
    pub label: String,
    pub message: Option<String>,
    pub snapshot_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEvent {
    pub target_event_id: Uuid,
    pub mode: UndoMode,
    pub restored_checkpoint_event: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UndoMode {
    Last,
    N(usize),
    To(Uuid),
    SinceNanos(u128),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitBridgeEvent {
    pub action: GitBridgeAction,
    pub success: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GitBridgeAction {
    Commit,
    Push,
    Pull,
}
