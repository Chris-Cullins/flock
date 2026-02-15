use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// --- Presence ---

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceSummary {
    pub actor: String,
    pub workspace: String,
    pub active_files: Vec<String>,
    pub intent: Option<String>,
    pub ttl: Duration,
    pub last_heartbeat: String,
}

impl PresenceSummary {
    /// Returns true if this presence has expired relative to the given timestamp (nanos).
    pub fn is_expired_at(&self, now_nanos: u128) -> bool {
        let last = self.last_heartbeat.parse::<u128>().unwrap_or(0);
        let ttl_nanos = self.ttl.as_nanos();
        now_nanos.saturating_sub(last) > ttl_nanos
    }
}

// --- Advisory Locks ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockStatus {
    Held,
    Released,
}

impl fmt::Display for LockStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Held => f.write_str("held"),
            Self::Released => f.write_str("released"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockSummary {
    pub id: Uuid,
    pub resource: String,
    pub holder: String,
    pub status: LockStatus,
    pub ttl: Duration,
    pub acquired_at: String,
    pub released_at: Option<String>,
}

impl LockSummary {
    /// Returns true if this lock has expired relative to the given timestamp (nanos).
    pub fn is_expired_at(&self, now_nanos: u128) -> bool {
        if self.status == LockStatus::Released {
            return true;
        }
        let acquired = self.acquired_at.parse::<u128>().unwrap_or(0);
        let ttl_nanos = self.ttl.as_nanos();
        now_nanos.saturating_sub(acquired) > ttl_nanos
    }
}

/// Check whether a lock can be acquired on a resource, given current held locks.
pub fn can_acquire_lock(
    resource: &str,
    locks: &BTreeMap<Uuid, LockSummary>,
    now_nanos: u128,
) -> Result<()> {
    for lock in locks.values() {
        if lock.resource == resource
            && lock.status == LockStatus::Held
            && !lock.is_expired_at(now_nanos)
        {
            bail!(
                "resource `{}` is already locked by `{}` (lock {})",
                resource,
                lock.holder,
                lock.id
            );
        }
    }
    Ok(())
}

// --- Subscriptions ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubscriptionStatus {
    Active,
    Cancelled,
}

impl fmt::Display for SubscriptionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionSummary {
    pub id: Uuid,
    pub actor: String,
    pub status: SubscriptionStatus,
    pub paths: Vec<String>,
    pub symbols: Vec<String>,
    pub modules: Vec<String>,
    pub notify: SubscriptionNotify,
    pub created_at: String,
    pub cancelled_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubscriptionNotify {
    Immediate,
    Batched,
    Digest,
}

impl fmt::Display for SubscriptionNotify {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Immediate => f.write_str("immediate"),
            Self::Batched => f.write_str("batched"),
            Self::Digest => f.write_str("digest"),
        }
    }
}

/// Check if a file path matches any subscription filters.
pub fn subscription_matches_path(sub: &SubscriptionSummary, path: &str) -> bool {
    if sub.paths.is_empty() {
        return false;
    }
    sub.paths.iter().any(|p| {
        if p.ends_with('*') {
            path.starts_with(&p[..p.len() - 1])
        } else {
            path == p
        }
    })
}

// --- Gates ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateStatus {
    Active,
    Approved,
    Rejected,
    Deleted,
}

impl fmt::Display for GateStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Approved => f.write_str("approved"),
            Self::Rejected => f.write_str("rejected"),
            Self::Deleted => f.write_str("deleted"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateSummary {
    pub id: Uuid,
    pub status: GateStatus,
    pub condition: GateConditionKind,
    pub policy: GatePolicyKind,
    pub approved_by: Option<String>,
    pub reason: Option<String>,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateConditionKind {
    FileTouched(String),
    SymbolModified(String),
    ImpactExceeds(u32),
    SecuritySensitive,
    AgentConfidenceLow(u32),
}

impl fmt::Display for GateConditionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileTouched(p) => write!(f, "file-touched:{}", p),
            Self::SymbolModified(s) => write!(f, "symbol-modified:{}", s),
            Self::ImpactExceeds(n) => write!(f, "impact-exceeds:{}", n),
            Self::SecuritySensitive => f.write_str("security-sensitive"),
            Self::AgentConfidenceLow(n) => write!(f, "confidence-low:{}", n),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatePolicyKind {
    Block,
    QueueAndContinue,
}

impl fmt::Display for GatePolicyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block => f.write_str("block"),
            Self::QueueAndContinue => f.write_str("queue-and-continue"),
        }
    }
}

/// Check if any active gates would block a given file path.
pub fn check_gates_for_path(
    path: &str,
    gates: &BTreeMap<Uuid, GateSummary>,
) -> Vec<GateSummary> {
    gates
        .values()
        .filter(|g| g.status == GateStatus::Active)
        .filter(|g| match &g.condition {
            GateConditionKind::FileTouched(pattern) => {
                if pattern.ends_with('*') {
                    path.starts_with(&pattern[..pattern.len() - 1])
                } else {
                    path == pattern
                }
            }
            _ => false,
        })
        .cloned()
        .collect()
}
