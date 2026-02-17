use std::collections::HashSet;

use uuid::Uuid;

use crate::event::{Event, EventKind};

/// Describes the divergence between local and remote event logs.
#[derive(Debug, Clone)]
pub enum Divergence {
    /// No divergence, logs are identical.
    None,
    /// Remote has events that local doesn't; simple append.
    FastForward {
        remote_only: Vec<Event>,
    },
    /// Both sides have unique events from a common ancestor.
    CanAutoMerge {
        common_ancestor: Uuid,
        local_only: Vec<Event>,
        remote_only: Vec<Event>,
    },
    /// Cannot auto-merge, manual resolution needed.
    NeedsManual {
        reason: String,
    },
}

/// Detect divergence between local and remote event lists.
pub fn detect_event_divergence(local: &[Event], remote: &[Event]) -> Divergence {
    if local.is_empty() && remote.is_empty() {
        return Divergence::None;
    }

    // Build set of local event IDs for fast lookup
    let remote_ids: HashSet<Uuid> = remote.iter().map(|e| e.id).collect();

    // Find the last common event
    let mut common_ancestor: Option<Uuid> = None;
    let mut common_idx_local = 0;
    for (i, event) in local.iter().enumerate() {
        if remote_ids.contains(&event.id) {
            common_ancestor = Some(event.id);
            common_idx_local = i + 1;
        }
    }

    // Find common index in remote
    let common_idx_remote = if let Some(ancestor) = common_ancestor {
        remote.iter().position(|e| e.id == ancestor).map(|p| p + 1).unwrap_or(0)
    } else {
        0
    };

    let local_only: Vec<Event> = local[common_idx_local..].to_vec();
    let remote_only: Vec<Event> = remote[common_idx_remote..].to_vec();

    if local_only.is_empty() && remote_only.is_empty() {
        return Divergence::None;
    }

    if local_only.is_empty() {
        return Divergence::FastForward { remote_only };
    }

    // Check if both sides have conflicting checkpoints
    let local_has_checkpoints = local_only.iter().any(|e| matches!(&e.kind, EventKind::Checkpoint(_)));
    let remote_has_checkpoints = remote_only.iter().any(|e| matches!(&e.kind, EventKind::Checkpoint(_)));

    if local_has_checkpoints && remote_has_checkpoints {
        return Divergence::NeedsManual {
            reason: "both local and remote have new checkpoints since divergence".to_string(),
        };
    }

    if common_ancestor.is_none() && !local.is_empty() && !remote.is_empty() {
        return Divergence::NeedsManual {
            reason: "no common ancestor found between local and remote logs".to_string(),
        };
    }

    match common_ancestor {
        Some(ancestor) => Divergence::CanAutoMerge {
            common_ancestor: ancestor,
            local_only,
            remote_only,
        },
        None => Divergence::NeedsManual {
            reason: "no common ancestor found".to_string(),
        },
    }
}

/// Auto-merge remote-only events after local-only events by rewriting
/// the parent_id chain of the remote events.
pub fn auto_merge_events(
    local_tip: &Event,
    remote_only: &[Event],
) -> Vec<Event> {
    let mut merged = Vec::with_capacity(remote_only.len());
    let mut prev_id = local_tip.id;

    for event in remote_only {
        let mut rewritten = event.clone();
        rewritten.parent_id = Some(prev_id);
        // Clear hash chain since we're rewriting the chain
        rewritten.prev_event_hash = None;
        prev_id = rewritten.id;
        merged.push(rewritten);
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CheckpointEvent, EventKind, SessionAction, SessionEvent};

    fn make_checkpoint(idx: u64, parent_id: Option<Uuid>) -> Event {
        Event {
            id: Uuid::new_v4(),
            timestamp: format!("10000000000000000{}", idx),
            actor: "tester".to_string(),
            parent_id,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            kind: EventKind::Checkpoint(CheckpointEvent {
                label: format!("cp-{}", idx),
                message: None,
                snapshot_id: Uuid::new_v4(),
                parent_checkpoint_event: None,
                snapshot_merkle_root: Some("0".repeat(64)),
                ai_intent: None,
                intent_confidence: None,
                files_changed: None,
                category: None,
                scope_label: None,
                structured_description: None,
            }),
        }
    }

    fn make_session(idx: u64, parent_id: Option<Uuid>) -> Event {
        Event {
            id: Uuid::new_v4(),
            timestamp: format!("10000000000000000{}", idx),
            actor: "tester".to_string(),
            parent_id,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            kind: EventKind::Session(SessionEvent {
                session_id: Uuid::new_v4(),
                action: SessionAction::Start,
                agent: "test-agent".to_string(),
                initiator: None,
                task_description: None,
                exploration_id: None,
                result: None,
            }),
        }
    }

    #[test]
    fn identical_logs_no_divergence() {
        let e1 = make_checkpoint(1, None);
        let e2 = make_checkpoint(2, Some(e1.id));
        let divergence = detect_event_divergence(&[e1.clone(), e2.clone()], &[e1, e2]);
        assert!(matches!(divergence, Divergence::None));
    }

    #[test]
    fn fast_forward_when_local_is_subset() {
        let e1 = make_checkpoint(1, None);
        let e2 = make_checkpoint(2, Some(e1.id));
        let e3 = make_checkpoint(3, Some(e2.id));
        let divergence = detect_event_divergence(
            &[e1.clone(), e2.clone()],
            &[e1, e2, e3],
        );
        assert!(matches!(divergence, Divergence::FastForward { .. }));
    }

    #[test]
    fn auto_merge_non_checkpoint_divergence() {
        let e1 = make_checkpoint(1, None);
        // Local adds a session
        let e2_local = make_session(2, Some(e1.id));
        // Remote adds a session
        let e2_remote = make_session(3, Some(e1.id));

        let divergence = detect_event_divergence(
            &[e1.clone(), e2_local],
            &[e1, e2_remote],
        );
        assert!(matches!(divergence, Divergence::CanAutoMerge { .. }));
    }

    #[test]
    fn needs_manual_when_both_have_checkpoints() {
        let e1 = make_checkpoint(1, None);
        let e2_local = make_checkpoint(2, Some(e1.id));
        let e2_remote = make_checkpoint(3, Some(e1.id));

        let divergence = detect_event_divergence(
            &[e1.clone(), e2_local],
            &[e1, e2_remote],
        );
        assert!(matches!(divergence, Divergence::NeedsManual { .. }));
    }

    #[test]
    fn auto_merge_rewrites_parent_chain() {
        let e1 = make_checkpoint(1, None);
        let e2_local = make_session(2, Some(e1.id));
        let e3_remote = make_session(3, Some(e1.id));
        let e4_remote = make_session(4, Some(e3_remote.id));

        let merged = auto_merge_events(&e2_local, &[e3_remote.clone(), e4_remote.clone()]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].parent_id, Some(e2_local.id));
        assert_eq!(merged[1].parent_id, Some(merged[0].id));
        assert_eq!(merged[0].id, e3_remote.id);
        assert_eq!(merged[1].id, e4_remote.id);
    }
}
