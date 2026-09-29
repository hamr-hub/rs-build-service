//! 队列租约与持久化集成测试（真实 SQLite 文件）。

use std::time::Duration;

use hotpot_core::EventKind;
use hotpot_core::model::{ArtifactMeta, BuildEvent, BuildProfile, BuildRecord, SourceSpec};
use hotpot_scheduler::Scheduler;

fn local_spec(tag: &str) -> SourceSpec {
    SourceSpec::Local {
        path: format!("/tmp/fake-{tag}"),
    }
}

async fn open() -> (Scheduler, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let sched = Scheduler::open(&dir).await.unwrap();
    (sched, dir)
}

fn queued(tag: &str) -> BuildRecord {
    BuildRecord::queued(local_spec(tag), BuildProfile::default())
}

#[tokio::test]
async fn enqueue_and_get() {
    let (sched, _dir) = open().await;
    let rec = sched.enqueue(queued("a")).await.unwrap();
    let got = sched.get_build(rec.id).await.unwrap().unwrap();
    assert_eq!(got.id, rec.id);
    assert_eq!(got.status, hotpot_core::BuildStatus::Queued);
}

#[tokio::test]
async fn claim_sets_lease_and_blocks_second_claim() {
    let (sched, _dir) = open().await;
    let rec = sched.enqueue(queued("a")).await.unwrap();

    let claimed = sched
        .claim_next("w1", Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, rec.id);
    assert_eq!(claimed.status, hotpot_core::BuildStatus::Dispatched);

    let again = sched
        .claim_next("w2", Duration::from_secs(30))
        .await
        .unwrap();
    assert!(again.is_none(), "valid lease must not be reclaimed");
}

#[tokio::test]
async fn expired_lease_is_taken_over() {
    let (sched, _dir) = open().await;
    let rec = sched.enqueue(queued("a")).await.unwrap();

    let _ = sched
        .claim_next("w1", Duration::from_millis(20))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;

    let taken = sched
        .claim_next("w2", Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.id, rec.id);
}

#[tokio::test]
async fn events_round_trip_in_order() {
    let (sched, _dir) = open().await;
    let rec = sched.enqueue(queued("a")).await.unwrap();
    let events = vec![
        BuildEvent {
            build_id: rec.id,
            seq: 0,
            timestamp_ms: 1,
            kind: EventKind::Phase,
            payload: "start".into(),
        },
        BuildEvent {
            build_id: rec.id,
            seq: 1,
            timestamp_ms: 2,
            kind: EventKind::Stdout,
            payload: "line".into(),
        },
    ];
    sched.append_events(&events).await.unwrap();

    let got = sched.list_events(rec.id, 0, 100).await.unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].seq, 0);
    assert_eq!(got[0].kind, EventKind::Phase);
    assert_eq!(got[1].payload, "line");

    let after = sched.list_events(rec.id, 0, 1).await.unwrap();
    assert_eq!(after.len(), 1);
    let tail = sched.list_events(rec.id, 1, 100).await.unwrap();
    assert_eq!(tail.len(), 1);
}

#[tokio::test]
async fn cancel_queued_prevents_claim() {
    let (sched, _dir) = open().await;
    let rec = sched.enqueue(queued("a")).await.unwrap();

    assert!(sched.cancel_queued(rec.id).await.unwrap());
    let claim = sched
        .claim_next("w1", Duration::from_secs(30))
        .await
        .unwrap();
    assert!(claim.is_none());
    let got = sched.get_build(rec.id).await.unwrap().unwrap();
    assert_eq!(got.status, hotpot_core::BuildStatus::Canceled);
}

#[tokio::test]
async fn artifacts_round_trip() {
    let (sched, _dir) = open().await;
    let rec = sched.enqueue(queued("a")).await.unwrap();
    let artifacts = vec![ArtifactMeta {
        name: "demo-webapp".into(),
        digest: "abc123".into(),
        size: 42,
        attrs: Default::default(),
    }];
    sched.add_artifacts(rec.id, &artifacts).await.unwrap();

    let got = sched.list_artifacts(rec.id).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].name, "demo-webapp");
}

#[tokio::test]
async fn finish_build_writes_terminal_state() {
    let (sched, _dir) = open().await;
    let rec = sched.enqueue(queued("a")).await.unwrap();
    sched
        .finish_build(
            rec.id,
            hotpot_core::BuildStatus::Succeeded,
            Default::default(),
            None,
        )
        .await
        .unwrap();
    let got = sched.get_build(rec.id).await.unwrap().unwrap();
    assert_eq!(got.status, hotpot_core::BuildStatus::Succeeded);
    assert!(got.finished_at_ms.is_some());
}
