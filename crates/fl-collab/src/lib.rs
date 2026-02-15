use std::time::Duration;

use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub actor: String,
    pub workspace: String,
    pub ttl: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoryLock {
    pub resource: String,
    pub holder: String,
    pub ttl: Duration,
}

pub trait PresenceStore {
    fn heartbeat(&self, presence: Presence) -> Result<()>;
    fn list(&self) -> Result<Vec<Presence>>;
}

pub trait LockStore {
    fn acquire(&self, lock: AdvisoryLock) -> Result<()>;
    fn release(&self, resource: &str, holder: &str) -> Result<()>;
    fn list(&self) -> Result<Vec<AdvisoryLock>>;
}
