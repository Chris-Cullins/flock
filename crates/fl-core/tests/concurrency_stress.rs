//! Concurrent agent stress tests for Flock's multi-agent coordination.
//!
//! Validates that filesystem-level `flock(2)` locking correctly serializes
//! concurrent access to the event log, refs, and repo operations.

use std::collections::HashSet;
use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use fl_core::Repo;

/// Create a fresh initialized Flock repo in a tempdir with GitCompatible mode.
/// Returns the tempdir (so it isn't dropped) and the repo root path.
fn setup_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let root = dir.path().to_path_buf();
    let repo = Repo::at(&root);
    repo.init().expect("failed to init repo");

    // Create a seed file so checkpoints have something to snapshot.
    fs::write(root.join("seed.txt"), "initial content\n").expect("failed to write seed file");
    repo.create_checkpoint(Some("seed".into()))
        .expect("seed checkpoint");
    (dir, root)
}

/// Helper: create a unique file for a given thread index and write content.
fn create_thread_file(root: &std::path::Path, thread_idx: usize, iteration: usize) {
    let filename = format!("agent-{}-iter-{}.txt", thread_idx, iteration);
    let content = format!(
        "thread={} iteration={} timestamp={:?}\n",
        thread_idx,
        iteration,
        std::time::SystemTime::now()
    );
    fs::write(root.join(&filename), content).expect("failed to write thread file");
}

// ── Test 1: 10 concurrent writers ──────────────────────────────────────────

#[test]
fn ten_concurrent_checkpoint_writers() {
    let (dir, root) = setup_repo();
    let n_writers = 10;
    let barrier = Arc::new(Barrier::new(n_writers));
    let mut handles = Vec::new();

    for i in 0..n_writers {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            // Each writer creates a unique file so there is something to checkpoint.
            create_thread_file(&root, i, 0);

            // All threads wait here, then race to checkpoint simultaneously.
            barrier.wait();

            let repo = Repo::at(&root);
            repo.quick_save(Some(format!("writer-{}", i)))
                .expect("quick_save should succeed under contention");
        }));
    }

    for h in handles {
        h.join().expect("writer thread panicked");
    }

    // Verify: all 10 quick-saves + seed checkpoint + init = 12 events.
    let repo = Repo::at(&root);
    let events = repo.list_events().expect("list_events");

    // Count checkpoint events only (excluding init).
    let checkpoint_count = events
        .iter()
        .filter(|e| matches!(e.kind, fl_storage::event::EventKind::Checkpoint(_)))
        .count();
    assert_eq!(
        checkpoint_count,
        n_writers + 1,
        "expected {} checkpoints (seed + {} writers), got {}",
        n_writers + 1,
        n_writers,
        checkpoint_count
    );

    // Verify unique event IDs (no duplicates).
    let ids: HashSet<_> = events.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), events.len(), "duplicate event IDs detected");

    drop(dir);
}

// ── Test 2: Mixed read/write operations ────────────────────────────────────

#[test]
fn mixed_read_write_concurrency() {
    let (dir, root) = setup_repo();
    let n_writers = 5;
    let n_readers = 5;
    let total = n_writers + n_readers;
    let barrier = Arc::new(Barrier::new(total));
    let mut handles = Vec::new();

    // Spawn writers.
    for i in 0..n_writers {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            create_thread_file(&root, i, 0);
            barrier.wait();
            let repo = Repo::at(&root);
            repo.quick_save(Some(format!("rw-writer-{}", i)))
                .expect("quick_save failed");
        }));
    }

    // Spawn readers that call status and list_events concurrently.
    for i in 0..n_readers {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let repo = Repo::at(&root);

            // Multiple read operations — should never panic or corrupt.
            let _ = repo.list_events().expect("list_events failed during concurrent write");
            let _ = repo.replay_state().expect("replay_state failed during concurrent write");

            // Status reads working dir + snapshots, should be safe concurrent with writes.
            let _ = repo.status().expect("status failed during concurrent write");

            // Verify we get sane event count (at least init + seed).
            let events = repo.list_events().expect("second list_events");
            assert!(
                events.len() >= 2,
                "reader {} saw fewer than 2 events: {}",
                i,
                events.len()
            );
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    drop(dir);
}

// ── Test 3: Lock contention measurement ────────────────────────────────────

#[test]
fn lock_contention_under_load() {
    let (_dir, root) = setup_repo();
    let n_writers = 10;
    let barrier = Arc::new(Barrier::new(n_writers));
    let mut handles = Vec::new();

    for i in 0..n_writers {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || -> Duration {
            create_thread_file(&root, i, 0);
            barrier.wait();

            let start = Instant::now();
            let repo = Repo::at(&root);
            repo.quick_save(Some(format!("contention-{}", i)))
                .expect("quick_save failed");
            start.elapsed()
        }));
    }

    let mut durations: Vec<Duration> = Vec::new();
    for h in handles {
        durations.push(h.join().expect("thread panicked"));
    }

    durations.sort();
    let min = durations.first().unwrap();
    let max = durations.last().unwrap();
    let median = &durations[durations.len() / 2];
    let total: Duration = durations.iter().sum();
    let avg = total / durations.len() as u32;

    eprintln!("── Lock contention results ({} writers) ──", n_writers);
    eprintln!("  Min:    {:?}", min);
    eprintln!("  Median: {:?}", median);
    eprintln!("  Avg:    {:?}", avg);
    eprintln!("  Max:    {:?}", max);

    // Sanity check: even under contention, no single writer should take > 30s.
    assert!(
        *max < Duration::from_secs(30),
        "max lock wait {:?} exceeded 30s — possible deadlock",
        max
    );
}

// ── Test 4: Event log integrity after concurrent writes ────────────────────

#[test]
fn event_log_integrity_after_concurrent_writes() {
    let (dir, root) = setup_repo();
    let n_writers = 10;
    let barrier = Arc::new(Barrier::new(n_writers));
    let mut handles = Vec::new();

    for i in 0..n_writers {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            create_thread_file(&root, i, 0);
            barrier.wait();
            let repo = Repo::at(&root);
            repo.quick_save(Some(format!("integrity-{}", i)))
                .expect("quick_save failed");
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    let repo = Repo::at(&root);
    let events = repo.list_events().expect("list_events");

    // Verify causal chain: each event's parent_id should point to the previous event.
    for i in 1..events.len() {
        let event = &events[i];
        let expected_parent = events[i - 1].id;
        assert_eq!(
            event.parent_id,
            Some(expected_parent),
            "event {} (id={}) has parent_id={:?} but expected {:?}",
            i,
            event.id,
            event.parent_id,
            Some(expected_parent)
        );
    }

    // First event should have no parent.
    assert_eq!(
        events[0].parent_id, None,
        "first event should have no parent"
    );

    // Unique IDs.
    let ids: HashSet<_> = events.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), events.len(), "duplicate event IDs found");

    // Unique timestamps (nanosecond precision + lock serialization should make these unique).
    let timestamps: HashSet<_> = events.iter().map(|e| &e.timestamp).collect();
    // Timestamps may collide in rare cases, so just check no gross issues.
    assert!(
        timestamps.len() >= events.len() / 2,
        "too many timestamp collisions: {} unique out of {}",
        timestamps.len(),
        events.len()
    );

    // Replay should succeed without errors (event log is consistent).
    let state = repo
        .replay_state()
        .expect("replay_state should succeed on concurrent event log");
    assert!(
        state.latest_checkpoint_event_id.is_some(),
        "replayed state should have a latest checkpoint"
    );

    drop(dir);
}

// ── Test 5: Quick-save storm ───────────────────────────────────────────────

#[test]
fn quick_save_storm() {
    let (dir, root) = setup_repo();
    let n_agents = 5;
    let saves_per_agent = 5;
    let total_threads = n_agents;
    let barrier = Arc::new(Barrier::new(total_threads));
    let mut handles = Vec::new();

    for agent in 0..n_agents {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for iteration in 0..saves_per_agent {
                create_thread_file(&root, agent, iteration);
                let repo = Repo::at(&root);
                repo.quick_save(Some(format!("storm-{}-{}", agent, iteration)))
                    .expect("quick_save in storm failed");
            }
        }));
    }

    for h in handles {
        h.join().expect("storm thread panicked");
    }

    let repo = Repo::at(&root);
    let events = repo.list_events().expect("list_events");

    // Total checkpoints: seed + (agents * saves_per_agent)
    let checkpoint_count = events
        .iter()
        .filter(|e| matches!(e.kind, fl_storage::event::EventKind::Checkpoint(_)))
        .count();
    let expected = 1 + n_agents * saves_per_agent;
    assert_eq!(
        checkpoint_count, expected,
        "expected {} checkpoints in storm, got {}",
        expected, checkpoint_count
    );

    // Causal chain must be intact.
    for i in 1..events.len() {
        assert_eq!(
            events[i].parent_id,
            Some(events[i - 1].id),
            "broken causal chain at event {}",
            i
        );
    }

    drop(dir);
}

// ── Test 6: Concurrent exploration lifecycle ───────────────────────────────

#[test]
fn concurrent_exploration_start() {
    let (dir, root) = setup_repo();
    let n_explorations = 5;
    let barrier = Arc::new(Barrier::new(n_explorations));
    let mut handles = Vec::new();

    for i in 0..n_explorations {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let repo = Repo::at(&root);
            // Each exploration has a unique title so no duplicate-name rejection.
            repo.start_exploration(format!("explore-{}", i))
                .expect("start_exploration failed");
        }));
    }

    for h in handles {
        h.join().expect("exploration thread panicked");
    }

    let repo = Repo::at(&root);
    let explorations = repo.list_explorations().expect("list_explorations");

    assert_eq!(
        explorations.len(),
        n_explorations,
        "expected {} explorations, got {}",
        n_explorations,
        explorations.len()
    );

    // All should be active.
    for e in &explorations {
        assert_eq!(
            e.status,
            fl_workflow::ExplorationStatus::Active,
            "exploration {} should be active",
            e.title
        );
    }

    // Unique exploration IDs.
    let ids: HashSet<_> = explorations.iter().map(|e| e.id).collect();
    assert_eq!(
        ids.len(),
        explorations.len(),
        "duplicate exploration IDs"
    );

    drop(dir);
}

#[test]
fn concurrent_exploration_abandon() {
    let (dir, root) = setup_repo();

    // Start explorations sequentially first (they need unique names checked serially).
    let repo = Repo::at(&root);
    let mut exploration_ids = Vec::new();
    for i in 0..5 {
        let summary = repo
            .start_exploration(format!("abandon-{}", i))
            .expect("start_exploration");
        exploration_ids.push(summary.id);
    }

    // Now abandon them all concurrently.
    let barrier = Arc::new(Barrier::new(exploration_ids.len()));
    let mut handles = Vec::new();

    for id in exploration_ids.clone() {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let repo = Repo::at(&root);
            repo.abandon_exploration(id)
                .expect("abandon_exploration failed");
        }));
    }

    for h in handles {
        h.join().expect("abandon thread panicked");
    }

    let explorations = repo.list_explorations().expect("list_explorations");
    for e in &explorations {
        assert_eq!(
            e.status,
            fl_workflow::ExplorationStatus::Abandoned,
            "exploration {} should be abandoned",
            e.title
        );
    }

    drop(dir);
}

// ── Test 7: Mixed checkpoint + exploration concurrency ─────────────────────

#[test]
fn mixed_checkpoint_and_exploration_concurrency() {
    let (dir, root) = setup_repo();
    let n_ops = 10;
    let barrier = Arc::new(Barrier::new(n_ops));
    let mut handles = Vec::new();

    for i in 0..n_ops {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let repo = Repo::at(&root);
            if i % 2 == 0 {
                // Even: checkpoint
                create_thread_file(&root, i, 0);
                repo.quick_save(Some(format!("mixed-cp-{}", i)))
                    .expect("quick_save in mixed test failed");
            } else {
                // Odd: start exploration
                repo.start_exploration(format!("mixed-explore-{}", i))
                    .expect("start_exploration in mixed test failed");
            }
        }));
    }

    for h in handles {
        h.join().expect("mixed ops thread panicked");
    }

    let repo = Repo::at(&root);
    let events = repo.list_events().expect("list_events");

    // Causal chain must remain intact across mixed operations.
    for i in 1..events.len() {
        assert_eq!(
            events[i].parent_id,
            Some(events[i - 1].id),
            "broken causal chain at event {} in mixed test",
            i
        );
    }

    // Replay should succeed.
    let state = repo
        .replay_state()
        .expect("replay_state failed after mixed ops");

    // We should have 5 explorations and 6 checkpoints (5 mixed + 1 seed).
    let explore_count = state.explorations.len();
    assert_eq!(
        explore_count, 5,
        "expected 5 explorations, got {}",
        explore_count
    );

    drop(dir);
}
