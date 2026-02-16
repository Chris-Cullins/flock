use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use ed25519_dalek::{Signer, SigningKey};
use fl_storage::event::{event_signing_payload, CheckpointEvent, Event, EventKind};
use fl_storage::{EventLog, MaterializedStateStore};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::tempdir;
use uuid::Uuid;

fn sign_event(event: &mut Event) {
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let payload = event_signing_payload(event).expect("serialize signing payload");
    let signature = key.sign(&payload);
    event.signer_public_key = Some(hex::encode(key.verifying_key().to_bytes()));
    event.signature = Some(hex::encode(signature.to_bytes()));
}

fn make_checkpoint_event(seq: u128, parent: Option<Uuid>) -> Event {
    let mut event = Event {
        id: Uuid::from_u128(seq),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string(),
        actor: "bench".to_string(),
        parent_id: parent,
        signer_public_key: None,
        signature: None,
        kind: EventKind::Checkpoint(CheckpointEvent {
            label: format!("cp-{}", seq),
            message: None,
            snapshot_id: Uuid::from_u128(seq + 1_000_000),
            parent_checkpoint_event: None,
            snapshot_merkle_root: Some("0".repeat(64)),
        }),
    };
    sign_event(&mut event);
    event
}

fn bench_event_log_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_log_append");

    for count in [10, 50, 100] {
        group.bench_with_input(
            BenchmarkId::new("append", count),
            &count,
            |b, &count| {
                b.iter(|| {
                    let dir = tempdir().unwrap();
                    let log = EventLog::for_root(dir.path());
                    log.ensure_exists().unwrap();
                    let mut prev_id = None;
                    for i in 0..count {
                        let event = make_checkpoint_event(i as u128, prev_id);
                        prev_id = Some(event.id);
                        log.append(&event).unwrap();
                    }
                });
            },
        );
    }
    group.finish();
}

fn bench_event_log_read_all(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_log_read_all");

    for count in [100, 500, 1000] {
        // Pre-populate the log
        let dir = tempdir().unwrap();
        let log = EventLog::for_root(dir.path());
        log.ensure_exists().unwrap();
        let mut prev_id = None;
        for i in 0..count {
            let event = make_checkpoint_event(i as u128, prev_id);
            prev_id = Some(event.id);
            log.append(&event).unwrap();
        }

        group.bench_with_input(
            BenchmarkId::new("read_all", count),
            &count,
            |b, _| {
                b.iter(|| {
                    log.read_all().unwrap();
                });
            },
        );
    }
    group.finish();
}

fn bench_materialized_state_save_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("materialized_state");

    // Create sample state JSON of various sizes
    for size_kb in [1, 10, 100] {
        let json = format!(r#"{{"data": "{}"}}"#, "x".repeat(size_kb * 1024));

        group.bench_with_input(
            BenchmarkId::new("save", format!("{}kb", size_kb)),
            &json,
            |b, json| {
                let dir = tempdir().unwrap();
                let store = MaterializedStateStore::for_root(dir.path());
                b.iter(|| {
                    store.save(100, json).unwrap();
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("load", format!("{}kb", size_kb)),
            &json,
            |b, json| {
                let dir = tempdir().unwrap();
                let store = MaterializedStateStore::for_root(dir.path());
                store.save(100, json).unwrap();
                b.iter(|| {
                    store.load(100).unwrap();
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_event_log_append,
    bench_event_log_read_all,
    bench_materialized_state_save_load,
);
criterion_main!(benches);
