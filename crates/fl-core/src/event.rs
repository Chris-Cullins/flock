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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEvent {
    pub label: String,
    pub message: Option<String>,
    pub snapshot_id: Uuid,
}
