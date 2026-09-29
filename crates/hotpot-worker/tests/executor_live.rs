//! 执行器真实 cargo 构建测试（需要本机 cargo；CI 默认工具链即可）。

use std::fs;
use std::time::Duration;

use hotpot_core::model::EventKind;
use hotpot_worker::{BuildPlan, ExecutorKind, run_build};

fn write_fixture(dir: &std::path::Path, main_rs: &str) {
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.rs"), main_rs).unwrap();
}

#[tokio::test]
async fn builds_a_tiny_project_and_collects_events() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    write_fixture(&project, "fn main() { println!(\"hi\"); }\n");

    let session = root.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.timeout = Duration::from_secs(120);

    let (mut events, handle) = run_build(plan, &ExecutorKind::Local).await;
    let mut kinds = Vec::new();
    let mut max_seq = 0;
    while let Some(event) = events.recv().await {
        // seq 必须在单构建内严格递增且无重复。
        assert!(event.seq >= max_seq);
        max_seq = event.seq;
        kinds.push(event.kind);
    }
    let result = handle.await.unwrap();

    assert!(result.success, "fixture build should succeed");
    assert_eq!(result.exit_code, Some(0));
    assert!(result.timings.build_ms > 0);
    assert!(kinds.contains(&EventKind::Phase));
}

#[tokio::test]
async fn reports_failure_for_broken_source() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("proj");
    fs::create_dir_all(&project).unwrap();
    write_fixture(&project, "fn main() { this is not rust\n");

    let session = root.path().join("session");
    let mut plan = BuildPlan::local(&project, &session);
    plan.timeout = Duration::from_secs(120);

    let (mut events, handle) = run_build(plan, &ExecutorKind::Local).await;
    let mut saw_compiler_error = false;
    while let Some(event) = events.recv().await {
        if event.kind == EventKind::Stderr && event.payload.contains("error") {
            saw_compiler_error = true;
        }
    }
    let result = handle.await.unwrap();

    assert!(!result.success);
    assert_ne!(result.exit_code, Some(0));
    assert!(saw_compiler_error, "cargo diagnostic should be captured");
}
