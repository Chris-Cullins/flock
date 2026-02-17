use crate::event::{DirectiveKind, Event, EventKind, SubscriptionFilter};
use serde::{Deserialize, Serialize};

/// Client-to-server WebSocket messages
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data")]
pub enum WsClientMessage {
    Auth {
        token: String,
    },
    Ping {
        seq: u64,
    },
    Subscribe(WsSubscribeRequest),
    Unsubscribe {
        subscription_id: String,
    },
    PresenceAnnounce {
        workspace: String,
        files: Vec<String>,
        symbols: Vec<String>,
        intent: Option<String>,
    },
    ChangeSetAnnounce {
        paths: Vec<String>,
        symbols: Vec<String>,
        change_kinds: Vec<String>,
    },
    SendDirective {
        target_actor: String,
        directive: DirectiveKind,
        reason: Option<String>,
    },
    StartPreview {
        workspace: String,
        interval_ms: u64,
    },
    StopPreview,
}

/// Server-to-client WebSocket messages
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data")]
pub enum WsServerMessage {
    AuthResult {
        success: bool,
        identity: Option<String>,
        error: Option<String>,
    },
    Pong {
        seq: u64,
    },
    Subscribed {
        subscription_id: String,
        filter: WsSubscribeRequest,
    },
    EventNotification {
        subscription_id: String,
        event: Event,
    },
    PresenceUpdate {
        actor: String,
        workspace: String,
        files: Vec<String>,
        symbols: Vec<String>,
        intent: Option<String>,
        departed: bool,
    },
    HeadsUpWarning {
        actor: String,
        symbol: String,
        path: String,
        action: String,
    },
    ConflictForecast {
        symbol: String,
        path: String,
        local_change: String,
        remote_change: String,
        remote_actor: String,
    },
    TaskUpdate {
        task_id: String,
        title: String,
        status: String,
        assignee: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
    Directive {
        from_actor: String,
        directive: DirectiveKind,
        reason: Option<String>,
    },
    WorkspacePreview {
        actor: String,
        workspace: String,
        diffs: Vec<PreviewDiff>,
        timestamp: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreviewDiff {
    pub path: String,
    pub symbols_changed: Vec<String>,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Subscription request with filtering criteria
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WsSubscribeRequest {
    pub filter: SubscriptionFilter,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub event_kinds: Vec<String>,
}

impl WsSubscribeRequest {
    /// Check if this subscription filter matches the given event
    pub fn matches_event(&self, event: &Event) -> bool {
        // Empty filter matches everything
        let is_empty = self.filter.paths.is_empty()
            && self.filter.symbols.is_empty()
            && self.filter.modules.is_empty()
            && self.agents.is_empty()
            && self.event_kinds.is_empty();

        if is_empty {
            return true;
        }

        // Check event_kinds filter
        if !self.event_kinds.is_empty() {
            let kind_name = event_kind_name(&event.kind);
            if !self.event_kinds.iter().any(|k| k == kind_name) {
                return false;
            }
        }

        // Check agents filter
        if !self.agents.is_empty() {
            if !self.agents.contains(&event.actor) {
                return false;
            }
        }

        // For paths/symbols/modules filtering, we return true if the filter is set
        // since we can't reliably extract paths from all event types.
        // The server will do proper filtering based on event content.
        if !self.filter.paths.is_empty()
            || !self.filter.symbols.is_empty()
            || !self.filter.modules.is_empty()
        {
            return true;
        }

        true
    }
}

/// Helper function to get event kind name as string
pub fn event_kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Checkpoint(_) => "Checkpoint",
        EventKind::Exploration(_) => "Exploration",
        EventKind::Undo(_) => "Undo",
        EventKind::GitBridge(_) => "GitBridge",
        EventKind::Session(_) => "Session",
        EventKind::Decision(_) => "Decision",
        EventKind::ResourceUsage(_) => "ResourceUsage",
        EventKind::Task(_) => "Task",
        EventKind::Presence(_) => "Presence",
        EventKind::Lock(_) => "Lock",
        EventKind::Subscription(_) => "Subscription",
        EventKind::Gate(_) => "Gate",
        EventKind::Rebase(_) => "Rebase",
        EventKind::ConflictResolution(_) => "ConflictResolution",
        EventKind::Hook(_) => "Hook",
        EventKind::RemoteSync(_) => "RemoteSync",
        EventKind::Intelligence(_) => "Intelligence",
        EventKind::Policy(_) => "Policy",
        EventKind::Directive(_) => "Directive",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        CheckpointEvent, ExplorationEvent, ExplorationAction, TaskEvent, TaskAction, UndoEvent,
        UndoMode,
    };
    use uuid::Uuid;

    fn make_checkpoint_event(actor: &str) -> Event {
        Event {
            id: Uuid::new_v4(),
            timestamp: "2026-02-16T12:00:00Z".to_string(),
            actor: actor.to_string(),
            parent_id: None,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            kind: EventKind::Checkpoint(CheckpointEvent {
                label: "test".to_string(),
                message: Some("Test".to_string()),
                snapshot_id: Uuid::new_v4(),
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
        }
    }

    fn make_task_event(actor: &str) -> Event {
        Event {
            id: Uuid::new_v4(),
            timestamp: "2026-02-16T12:00:00Z".to_string(),
            actor: actor.to_string(),
            parent_id: None,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            kind: EventKind::Task(TaskEvent {
                task_id: Uuid::new_v4(),
                action: TaskAction::Create,
                title: "Fix bug".to_string(),
                description: None,
                dependencies: vec![],
                assignee: None,
                result: None,
                linked_events: vec![],
                discovered_from: None,
                allowed_paths: Vec::new(),
            }),
        }
    }

    fn make_undo_event(actor: &str) -> Event {
        Event {
            id: Uuid::new_v4(),
            timestamp: "2026-02-16T12:00:00Z".to_string(),
            actor: actor.to_string(),
            parent_id: None,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            kind: EventKind::Undo(UndoEvent {
                target_event_id: Uuid::new_v4(),
                mode: UndoMode::Last,
                restored_checkpoint_event: None,
                file_scope: None,
            }),
        }
    }

    #[test]
    fn test_ws_client_message_serde_auth() {
        let msg = WsClientMessage::Auth {
            token: "test-token".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_client_message_serde_ping() {
        let msg = WsClientMessage::Ping { seq: 42 };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_client_message_serde_subscribe() {
        let msg = WsClientMessage::Subscribe(WsSubscribeRequest {
            filter: SubscriptionFilter {
                paths: vec!["src/main.rs".to_string()],
                symbols: vec!["main".to_string()],
                modules: vec!["core".to_string()],
            },
            agents: vec!["alice".to_string()],
            event_kinds: vec!["Checkpoint".to_string()],
        });
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_client_message_serde_unsubscribe() {
        let msg = WsClientMessage::Unsubscribe {
            subscription_id: "sub-123".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_client_message_serde_presence_announce() {
        let msg = WsClientMessage::PresenceAnnounce {
            workspace: "main".to_string(),
            files: vec!["src/lib.rs".to_string()],
            symbols: vec!["process".to_string()],
            intent: Some("reviewing".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_client_message_serde_changeset_announce() {
        let msg = WsClientMessage::ChangeSetAnnounce {
            paths: vec!["src/main.rs".to_string()],
            symbols: vec!["main".to_string()],
            change_kinds: vec!["Modified".to_string()],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_auth_result() {
        let msg = WsServerMessage::AuthResult {
            success: true,
            identity: Some("alice".to_string()),
            error: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_pong() {
        let msg = WsServerMessage::Pong { seq: 42 };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_subscribed() {
        let msg = WsServerMessage::Subscribed {
            subscription_id: "sub-123".to_string(),
            filter: WsSubscribeRequest {
                filter: SubscriptionFilter {
                    paths: vec![],
                    symbols: vec![],
                    modules: vec![],
                },
                agents: vec![],
                event_kinds: vec!["Checkpoint".to_string()],
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_event_notification() {
        let event = make_checkpoint_event("alice");
        let msg = WsServerMessage::EventNotification {
            subscription_id: "sub-123".to_string(),
            event: event.clone(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_presence_update() {
        let msg = WsServerMessage::PresenceUpdate {
            actor: "alice".to_string(),
            workspace: "main".to_string(),
            files: vec!["src/lib.rs".to_string()],
            symbols: vec!["process".to_string()],
            intent: Some("editing".to_string()),
            departed: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_heads_up_warning() {
        let msg = WsServerMessage::HeadsUpWarning {
            actor: "bob".to_string(),
            symbol: "calculate".to_string(),
            path: "src/math.rs".to_string(),
            action: "editing".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_conflict_forecast() {
        let msg = WsServerMessage::ConflictForecast {
            symbol: "process".to_string(),
            path: "src/lib.rs".to_string(),
            local_change: "Modified signature".to_string(),
            remote_change: "Modified implementation".to_string(),
            remote_actor: "charlie".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_task_update() {
        let msg = WsServerMessage::TaskUpdate {
            task_id: "task-456".to_string(),
            title: "Fix bug".to_string(),
            status: "in_progress".to_string(),
            assignee: Some("alice".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_error() {
        let msg = WsServerMessage::Error {
            code: "AUTH_FAILED".to_string(),
            message: "Invalid token".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_matches_event_empty_filter_matches_all() {
        let filter = WsSubscribeRequest {
            filter: SubscriptionFilter {
                paths: vec![],
                symbols: vec![],
                modules: vec![],
            },
            agents: vec![],
            event_kinds: vec![],
        };
        assert!(filter.matches_event(&make_checkpoint_event("alice")));
    }

    #[test]
    fn test_matches_event_event_kinds_filter() {
        let filter = WsSubscribeRequest {
            filter: SubscriptionFilter {
                paths: vec![],
                symbols: vec![],
                modules: vec![],
            },
            agents: vec![],
            event_kinds: vec!["Checkpoint".to_string()],
        };
        assert!(filter.matches_event(&make_checkpoint_event("alice")));
        assert!(!filter.matches_event(&make_task_event("alice")));
    }

    #[test]
    fn test_matches_event_agents_filter() {
        let filter = WsSubscribeRequest {
            filter: SubscriptionFilter {
                paths: vec![],
                symbols: vec![],
                modules: vec![],
            },
            agents: vec!["alice".to_string(), "bob".to_string()],
            event_kinds: vec![],
        };
        assert!(filter.matches_event(&make_checkpoint_event("alice")));
        assert!(!filter.matches_event(&make_checkpoint_event("charlie")));
    }

    #[test]
    fn test_matches_event_combined_filters() {
        let filter = WsSubscribeRequest {
            filter: SubscriptionFilter {
                paths: vec![],
                symbols: vec![],
                modules: vec![],
            },
            agents: vec!["alice".to_string()],
            event_kinds: vec!["Checkpoint".to_string(), "Task".to_string()],
        };
        assert!(filter.matches_event(&make_checkpoint_event("alice")));
        assert!(!filter.matches_event(&make_checkpoint_event("bob")));
        assert!(!filter.matches_event(&make_undo_event("alice")));
    }

    #[test]
    fn test_matches_event_paths_filter_returns_true() {
        let filter = WsSubscribeRequest {
            filter: SubscriptionFilter {
                paths: vec!["src/main.rs".to_string()],
                symbols: vec![],
                modules: vec![],
            },
            agents: vec![],
            event_kinds: vec![],
        };
        assert!(filter.matches_event(&make_checkpoint_event("alice")));
    }

    #[test]
    fn test_event_kind_name() {
        let cp = make_checkpoint_event("x");
        assert_eq!(event_kind_name(&cp.kind), "Checkpoint");

        let task = make_task_event("x");
        assert_eq!(event_kind_name(&task.kind), "Task");

        let undo = make_undo_event("x");
        assert_eq!(event_kind_name(&undo.kind), "Undo");

        let exploration_kind = EventKind::Exploration(ExplorationEvent {
            exploration_id: Uuid::new_v4(),
            title: "Test".to_string(),
            base_checkpoint_event: None,
            action: ExplorationAction::Start,
        });
        assert_eq!(event_kind_name(&exploration_kind), "Exploration");
    }
}
