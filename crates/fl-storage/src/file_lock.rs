//! Filesystem-level advisory locking using `flock(2)` (via the `fs2` crate).
//!
//! Provides RAII lock guards that hold an exclusive `flock(2)` lock on a
//! `.lock` file for the duration of a critical section. The lock is released
//! automatically when the guard is dropped (file handle closed).

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;

/// RAII guard for an exclusive filesystem lock. The lock is held as long as
/// this value is alive and released when dropped.
pub struct FileLock {
    _file: File,
}

impl FileLock {
    /// Acquire an exclusive lock on the given lockfile path. Creates the
    /// lockfile (and parent directories) if it doesn't exist. Blocks until
    /// the lock is acquired.
    pub fn acquire(lock_path: &Path) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create lock directory {}", parent.display())
            })?;
        }

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(lock_path)
            .with_context(|| format!("failed to open lockfile {}", lock_path.display()))?;

        file.lock_exclusive()
            .with_context(|| format!("failed to acquire lock on {}", lock_path.display()))?;

        Ok(Self { _file: file })
    }
}

// Lock is released when the File is dropped (fd closed), which releases
// the flock(2) advisory lock automatically.

/// Standard lockfile paths relative to the repository root.
pub struct LockPaths;

impl LockPaths {
    /// Lockfile for event log operations (append, batch append, migration).
    pub fn event_log(root: &Path) -> PathBuf {
        root.join(".flock/event-log.lock")
    }

    /// Lockfile for ref store operations (upsert, delete, write_all, migration).
    pub fn refs(root: &Path) -> PathBuf {
        root.join(".flock/refs.lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn lock_acquire_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");

        {
            let _lock = FileLock::acquire(&lock_path).unwrap();
            assert!(lock_path.exists());
        }
        // Lock released after drop — we should be able to re-acquire
        let _lock = FileLock::acquire(&lock_path).unwrap();
    }

    #[test]
    fn lock_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("nested/dir/test.lock");

        let _lock = FileLock::acquire(&lock_path).unwrap();
        assert!(lock_path.exists());
    }

    #[test]
    fn concurrent_lock_serializes_access() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");
        let counter_path = dir.path().join("counter.txt");
        fs::write(&counter_path, "0").unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let lock_path = lock_path.clone();
            let counter_path = counter_path.clone();
            let barrier = Arc::clone(&barrier);

            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let _lock = FileLock::acquire(&lock_path).unwrap();
                // Read-modify-write under lock
                let val: u32 = fs::read_to_string(&counter_path)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                // Small sleep to increase chance of interleaving without lock
                std::thread::sleep(std::time::Duration::from_millis(10));
                fs::write(&counter_path, (val + 1).to_string()).unwrap();
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let final_val: u32 = fs::read_to_string(&counter_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(final_val, 2, "lock should serialize concurrent increments");
    }
}
