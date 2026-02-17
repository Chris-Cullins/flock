use std::collections::BTreeSet;

use serde::Serialize;

use crate::event::{Event, EventKind};

/// An anomaly detected in the audit trail.
#[derive(Debug, Clone, Serialize)]
pub struct AuditAnomaly {
    pub event_id: String,
    pub kind: String,
    pub detail: String,
}

/// Summary of a signing identity seen in the event log.
#[derive(Debug, Clone, Serialize)]
pub struct SigningIdentity {
    pub public_key: String,
    pub event_count: usize,
    pub first_seen: String,
    pub last_seen: String,
}

/// Gap in event timestamps exceeding a threshold.
#[derive(Debug, Clone, Serialize)]
pub struct TimelineGap {
    pub from_event_id: String,
    pub to_event_id: String,
    pub from_timestamp: String,
    pub to_timestamp: String,
    pub gap_hours: f64,
}

/// Full audit report for a repository.
#[derive(Debug, Clone, Serialize)]
pub struct AuditReport {
    pub total_events: usize,
    pub signing_identities: Vec<SigningIdentity>,
    pub timeline_gaps: Vec<TimelineGap>,
    pub anomalies: Vec<AuditAnomaly>,
}

const GAP_THRESHOLD_NANOS: u128 = 24 * 60 * 60 * 1_000_000_000; // 24 hours

/// Analyze an event log for audit purposes.
pub fn analyze_audit_trail(events: &[Event]) -> AuditReport {
    let mut identities: std::collections::BTreeMap<String, (usize, String, String)> =
        std::collections::BTreeMap::new();
    let mut anomalies = Vec::new();
    let mut timeline_gaps = Vec::new();
    let mut seen_keys: BTreeSet<String> = BTreeSet::new();

    let mut prev_event: Option<&Event> = None;

    for event in events {
        // Track signing identities
        if let Some(pk) = &event.signer_public_key {
            let entry = identities.entry(pk.clone()).or_insert_with(|| {
                (0, event.timestamp.clone(), event.timestamp.clone())
            });
            entry.0 += 1;
            entry.2 = event.timestamp.clone();

            // Detect key changes
            if !seen_keys.contains(pk) && !seen_keys.is_empty() {
                anomalies.push(AuditAnomaly {
                    event_id: event.id.to_string(),
                    kind: "key_change".to_string(),
                    detail: format!("new signing key appeared: {}...{}", &pk[..8.min(pk.len())], &pk[pk.len().saturating_sub(8)..]),
                });
            }
            seen_keys.insert(pk.clone());
        } else {
            // Unsigned event
            anomalies.push(AuditAnomaly {
                event_id: event.id.to_string(),
                kind: "unsigned_event".to_string(),
                detail: format!("event has no signature (kind: {})", event_kind_label(&event.kind)),
            });
        }

        // Detect hash chain breaks
        if let Some(prev) = prev_event {
            let expected_hash = fl_storage::compute_event_hash(prev);
            match &event.prev_event_hash {
                Some(h) if h != &expected_hash => {
                    anomalies.push(AuditAnomaly {
                        event_id: event.id.to_string(),
                        kind: "hash_chain_break".to_string(),
                        detail: "prev_event_hash does not match computed hash of predecessor".to_string(),
                    });
                }
                None => {
                    // May be a pre-v13 event, only flag if it looks suspicious
                }
                _ => {}
            }

            // Detect timeline gaps > 24h
            if let (Ok(prev_ts), Ok(cur_ts)) = (
                prev.timestamp.parse::<u128>(),
                event.timestamp.parse::<u128>(),
            ) {
                if cur_ts > prev_ts {
                    let gap = cur_ts - prev_ts;
                    if gap > GAP_THRESHOLD_NANOS {
                        let gap_hours = gap as f64 / (3_600_000_000_000.0);
                        timeline_gaps.push(TimelineGap {
                            from_event_id: prev.id.to_string(),
                            to_event_id: event.id.to_string(),
                            from_timestamp: prev.timestamp.clone(),
                            to_timestamp: event.timestamp.clone(),
                            gap_hours,
                        });
                    }
                }
            }
        }

        prev_event = Some(event);
    }

    let signing_identities: Vec<SigningIdentity> = identities
        .into_iter()
        .map(|(pk, (count, first, last))| SigningIdentity {
            public_key: pk,
            event_count: count,
            first_seen: first,
            last_seen: last,
        })
        .collect();

    AuditReport {
        total_events: events.len(),
        signing_identities,
        timeline_gaps,
        anomalies,
    }
}

fn event_kind_label(kind: &EventKind) -> &'static str {
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
    use crate::event::{CheckpointEvent, EventKind};
    use uuid::Uuid;

    fn make_event(idx: u64, parent_id: Option<Uuid>, timestamp: &str, signed: bool) -> Event {
        let mut event = Event {
            id: Uuid::new_v4(),
            timestamp: timestamp.to_string(),
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
                git_commit_sha: None,
            }),
        };
        if signed {
            event.signer_public_key = Some("a".repeat(64));
            event.signature = Some("b".repeat(128));
        }
        event
    }

    #[test]
    fn detects_unsigned_events() {
        let e1 = make_event(1, None, "1000000000000000000", false);
        let report = analyze_audit_trail(&[e1]);
        assert_eq!(report.anomalies.len(), 1);
        assert_eq!(report.anomalies[0].kind, "unsigned_event");
    }

    #[test]
    fn tracks_signing_identities() {
        let e1 = make_event(1, None, "1000000000000000000", true);
        let e2 = make_event(2, Some(e1.id), "1000000001000000000", true);
        let report = analyze_audit_trail(&[e1, e2]);
        assert_eq!(report.signing_identities.len(), 1);
        assert_eq!(report.signing_identities[0].event_count, 2);
    }

    #[test]
    fn detects_timeline_gaps() {
        let e1 = make_event(1, None, "1000000000000000000", true);
        // 48 hours later
        let ts2 = 1000000000000000000u128 + 48 * 3600 * 1_000_000_000;
        let e2 = make_event(2, Some(e1.id), &ts2.to_string(), true);
        let report = analyze_audit_trail(&[e1, e2]);
        assert_eq!(report.timeline_gaps.len(), 1);
        assert!(report.timeline_gaps[0].gap_hours > 47.0);
    }

    #[test]
    fn detects_key_change() {
        let mut e1 = make_event(1, None, "1000000000000000000", true);
        e1.signer_public_key = Some("a".repeat(64));
        let mut e2 = make_event(2, Some(e1.id), "1000000001000000000", true);
        e2.signer_public_key = Some("c".repeat(64));
        let report = analyze_audit_trail(&[e1, e2]);
        assert!(report.anomalies.iter().any(|a| a.kind == "key_change"));
    }
}
