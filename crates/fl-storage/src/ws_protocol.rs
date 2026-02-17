use crate::event::{DirectiveKind, Event, EventKind, SubscriptionFilter};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Client → Server messages
// ---------------------------------------------------------------------------

/// Client-to-server WebSocket messages.
///
/// Wire format: `{"type": "VariantName", "data": { ... }}` (adjacently tagged).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum WsClientMessage {
    Auth {
        token: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Ping {
        seq: u64,
    },
    Pong {
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
    /// Request catch-up events since a known event ID.
    SyncRequest {
        last_event_id: String,
    },
    /// Claim a task for the current agent.
    TaskClaim {
        task_id: String,
    },
    /// Release a previously claimed task.
    TaskRelease {
        task_id: String,
    },
    /// Renew the TTL on a claimed task.
    TaskRenew {
        task_id: String,
    },
    /// Acquire a region lock.
    LockAcquire {
        repo_id: String,
        patterns: Vec<String>,
        /// TTL in seconds.
        ttl: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Release a region lock.
    LockRelease {
        repo_id: String,
        lock_id: String,
    },
    /// Query locks for a repo.
    LockQuery {
        repo_id: String,
    },
    /// Respond to a proactive task assignment.
    TaskAssignmentResponse {
        task_id: String,
        accepted: bool,
    },
}

// ---------------------------------------------------------------------------
// Server → Client messages
// ---------------------------------------------------------------------------

/// Server-to-client WebSocket messages.
///
/// Wire format: `{"type": "VariantName", "data": { ... }}` (adjacently tagged).
/// This matches the envelope produced by both the server's typed enum serialization
/// and its `send_compat()` helper.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum WsServerMessage {
    // --- Handshake / control ---
    AuthResult {
        success: bool,
        identity: Option<String>,
        error: Option<String>,
    },
    /// Server-initiated heartbeat ping.
    Ping {
        seq: u64,
    },
    Pong {
        seq: u64,
    },
    Subscribed {
        subscription_id: String,
        filter: serde_json::Value,
    },
    Error {
        code: String,
        message: String,
    },

    // --- Event streaming ---
    /// Single-event notification with subscription routing (planned).
    EventNotification {
        subscription_id: String,
        event: Event,
    },
    /// Batch event broadcast from the server.
    EventBroadcast {
        events: Vec<serde_json::Value>,
    },
    /// Notification that new events were appended (sent on `fl push`).
    EventsAppended {
        repo_id: String,
        count: usize,
    },
    /// Catch-up events in response to a SyncRequest.
    SyncResponse {
        events: Vec<serde_json::Value>,
    },

    // --- Presence ---
    /// Single-actor presence update (planned single-actor delivery).
    PresenceUpdate {
        actor: String,
        workspace: String,
        files: Vec<String>,
        symbols: Vec<String>,
        intent: Option<String>,
        departed: bool,
    },
    /// Bulk presence broadcast for a repo.
    PresenceBroadcast {
        repo_id: String,
        presences: Vec<PresenceRecord>,
    },

    // --- Agent / semantic feeds ---
    AgentUpdate {
        agent_id: String,
        repo_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_task_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_exploration_id: Option<String>,
        timestamp: String,
    },
    SemanticFeed {
        repo_id: String,
        change_id: String,
        kind: String,
        symbol_name: String,
        file_path: String,
        timestamp: String,
    },

    // --- Conflict / warnings ---
    ConflictForecast {
        repo_id: String,
        exploration_ids: Vec<String>,
        conflict_count: usize,
        risk_level: String,
        timestamp: String,
    },
    ConflictAlert {
        alert: ConflictAlertInfo,
    },
    HeadsUpWarning {
        actor: String,
        symbol: String,
        path: String,
        action: String,
    },

    // --- Tasks ---
    TaskSync {
        task_id: String,
        repo_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        assigned_agent_id: Option<String>,
        timestamp: String,
    },
    TaskClaimResult {
        task_id: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        claimed_by: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Proactive task assignment from the server.
    TaskAssignment {
        task_id: String,
        repo_id: String,
        title: String,
        description: String,
        priority: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        affected_files: Option<Vec<String>>,
    },

    // --- Locks ---
    LockResult {
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        lock_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    LockUpdate {
        repo_id: String,
        action: String,
        lock: RegionLockInfo,
    },
    LockList {
        repo_id: String,
        locks: Vec<RegionLockInfo>,
    },

    // --- Policy ---
    PolicyVerdict {
        decisions: Vec<serde_json::Value>,
    },

    // --- Directives (planned) ---
    Directive {
        from_actor: String,
        directive: DirectiveKind,
        reason: Option<String>,
    },

    // --- Preview (planned) ---
    WorkspacePreview {
        actor: String,
        workspace: String,
        diffs: Vec<PreviewDiff>,
        timestamp: String,
    },
}

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresenceRecord {
    pub identity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub workspace: String,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegionLockInfo {
    pub id: String,
    pub repo_id: String,
    pub owner: String,
    pub patterns: Vec<String>,
    pub acquired_at: String,
    pub ttl_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConflictAlertInfo {
    pub id: String,
    pub repo_id: String,
    pub severity: String,
    pub affected_files: Vec<String>,
    pub actors: Vec<String>,
    pub description: String,
    pub related_lock_ids: Vec<String>,
    pub timestamp: String,
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
        EventKind::Init(_) => "Init",
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
        CheckpointEvent, ExplorationAction, ExplorationEvent, TaskAction, TaskEvent, UndoEvent,
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
            session_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_client_message_serde_auth_with_session() {
        let msg = WsClientMessage::Auth {
            token: "test-token".to_string(),
            session_id: Some("sess-123".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("sess-123"));
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
    fn test_ws_client_message_serde_task_claim() {
        let msg = WsClientMessage::TaskClaim {
            task_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_client_message_serde_lock_acquire() {
        let msg = WsClientMessage::LockAcquire {
            repo_id: "repo-1".to_string(),
            patterns: vec!["src/**/*.rs".to_string()],
            ttl: 300,
            reason: Some("editing".to_string()),
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
            filter: serde_json::json!({
                "filter": { "paths": [], "symbols": [], "modules": [] },
                "agents": [],
                "event_kinds": ["Checkpoint"],
            }),
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
    fn test_ws_server_message_serde_event_broadcast() {
        let msg = WsServerMessage::EventBroadcast {
            events: vec![serde_json::json!({"id": "test", "kind": "checkpoint"})],
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
    fn test_ws_server_message_serde_presence_broadcast() {
        let msg = WsServerMessage::PresenceBroadcast {
            repo_id: "repo-1".to_string(),
            presences: vec![PresenceRecord {
                identity: "alice".to_string(),
                agent_id: None,
                workspace: "main".to_string(),
                files: vec!["src/lib.rs".to_string()],
                symbols: vec![],
                intent: None,
                last_seen: "2026-02-17T12:00:00Z".to_string(),
            }],
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
            repo_id: "repo-1".to_string(),
            exploration_ids: vec!["exp-1".to_string()],
            conflict_count: 3,
            risk_level: "high".to_string(),
            timestamp: "2026-02-17T12:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_task_sync() {
        let msg = WsServerMessage::TaskSync {
            task_id: "task-456".to_string(),
            repo_id: "repo-1".to_string(),
            status: "in_progress".to_string(),
            assigned_agent_id: Some("agent-1".to_string()),
            timestamp: "2026-02-17T12:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_task_claim_result() {
        let msg = WsServerMessage::TaskClaimResult {
            task_id: "task-456".to_string(),
            success: true,
            claimed_by: Some("alice".to_string()),
            reason: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_lock_result() {
        let msg = WsServerMessage::LockResult {
            success: true,
            lock_id: Some("lock-1".to_string()),
            reason: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_lock_update() {
        let msg = WsServerMessage::LockUpdate {
            repo_id: "repo-1".to_string(),
            action: "acquired".to_string(),
            lock: RegionLockInfo {
                id: "lock-1".to_string(),
                repo_id: "repo-1".to_string(),
                owner: "alice".to_string(),
                patterns: vec!["src/**".to_string()],
                acquired_at: "2026-02-17T12:00:00Z".to_string(),
                ttl_secs: 300,
                reason: None,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: WsServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_ws_server_message_serde_events_appended() {
        let msg = WsServerMessage::EventsAppended {
            repo_id: "repo-1".to_string(),
            count: 5,
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
    fn test_wire_format_envelope() {
        // Verify the adjacently-tagged format produces the expected envelope
        let msg = WsClientMessage::Ping { seq: 1 };
        let json = serde_json::to_string(&msg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "Ping");
        assert_eq!(v["data"]["seq"], 1);
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
