//! Prometheus 文本格式指标端点（`GET /metrics`）。
//!
//! 设计取舍：**不维护进程内累计状态**，构建侧指标一律从 SQLite 现算，
//! 因此进程重启不会让计数回退（Prometheus 的 counter 语义要求单调）。
//! 只有缓存命中/未命中是进程内计数器（`RemoteCache` 内），重启归零，
//! 并以 `hotpot_cache_hits_total` 形式暴露为「自进程启动以来」。

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use hotpot_cacheproto::{Namespace, RemoteCache};
use hotpot_scheduler::Scheduler;
use hotpot_store::LocalStore;

use std::sync::Arc;

/// 指标数据源。
#[derive(Clone)]
pub struct MetricsSource {
    pub scheduler: Scheduler,
    pub store: Arc<LocalStore>,
    /// 未挂载缓存协议时为 None。
    pub cache: Option<RemoteCache>,
    pub workers: usize,
    /// 回收器累计计数。
    pub gc_stats: Arc<crate::gc::GcStats>,
    pub executor: String,
    pub toolchain: String,
    pub version: &'static str,
}

/// Prometheus 文本格式渲染。
pub async fn render(source: &MetricsSource) -> String {
    let mut out = String::with_capacity(2048);

    // --- 构建状态分布 ---
    match source.scheduler.status_counts().await {
        Ok(counts) => {
            metric(
                &mut out,
                "hotpot_builds_by_status",
                "gauge",
                "当前各状态的构建数",
                &[],
                &counts
                    .iter()
                    .map(|(status, n)| {
                        (
                            vec![("status", status_name(*status).to_string())],
                            *n as f64,
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
        Err(e) => out.push_str(&format!("# hotpot_builds_by_status unavailable: {e}\n")),
    }

    // --- 队列 ---
    match source.scheduler.queue_stats().await {
        Ok((depth, oldest_ms)) => {
            metric(
                &mut out,
                "hotpot_queue_depth",
                "gauge",
                "排队中的构建数",
                &[],
                &[(vec![], depth as f64)],
            );
            metric(
                &mut out,
                "hotpot_queue_oldest_wait_ms",
                "gauge",
                "最老排队构建已等待的毫秒数",
                &[],
                &[(vec![], oldest_ms as f64)],
            );
        }
        Err(e) => out.push_str(&format!("# hotpot_queue unavailable: {e}\n")),
    }

    // --- 阶段耗时 ---
    match source.scheduler.phase_duration_totals().await {
        Ok(t) => {
            let phase = |name: &str| vec![("phase", name.to_string())];
            let samples = [
                (phase("queue"), t.queue_sum as f64, t.count as f64),
                (phase("build"), t.build_sum as f64, t.count as f64),
                (phase("total"), t.total_sum as f64, t.count as f64),
            ];
            metric(
                &mut out,
                "hotpot_build_duration_ms_sum",
                "counter",
                "终态构建各阶段耗时累计（按记录现算，重启不回退）",
                &[],
                &samples
                    .iter()
                    .map(|(l, sum, _)| (l.clone(), *sum))
                    .collect::<Vec<_>>(),
            );
            metric(
                &mut out,
                "hotpot_build_duration_ms_count",
                "counter",
                "参与阶段耗时统计的终态构建数",
                &[],
                &[(vec![], t.count as f64)],
            );
            // 均值是运维最常看的数字，直接给出避免额外查询。
            if t.count > 0 {
                let n = t.count as f64;
                metric(
                    &mut out,
                    "hotpot_build_duration_ms_avg",
                    "gauge",
                    "终态构建各阶段平均耗时（由 sum/count 现算）",
                    &[],
                    &samples
                        .iter()
                        .map(|(l, sum, _)| (l.clone(), sum / n))
                        .collect::<Vec<_>>(),
                );
            }
        }
        Err(e) => out.push_str(&format!("# hotpot_build_duration unavailable: {e}\n")),
    }

    // --- 缓存协议层 ---
    if let Some(cache) = &source.cache {
        for (ns, protocol) in [(Namespace::Sccache, "sccache"), (Namespace::Turbo, "turbo")] {
            let stats = cache.stats(ns);
            let labels = vec![("protocol", protocol.to_string())];
            metric(
                &mut out,
                "hotpot_cache_lookups_total",
                "counter",
                "缓存查询次数（按协议分；进程启动以来）",
                &[],
                &[(labels.clone(), (stats.hits + stats.misses) as f64)],
            );
            metric(
                &mut out,
                "hotpot_cache_hits_total",
                "counter",
                "缓存命中次数",
                &[],
                &[(labels.clone(), stats.hits as f64)],
            );
            metric(
                &mut out,
                "hotpot_cache_misses_total",
                "counter",
                "缓存未命中次数",
                &[],
                &[(labels.clone(), stats.misses as f64)],
            );
            metric(
                &mut out,
                "hotpot_cache_puts_total",
                "counter",
                "缓存写入次数",
                &[],
                &[(labels.clone(), stats.puts as f64)],
            );
            metric(
                &mut out,
                "hotpot_cache_hit_ratio",
                "gauge",
                "缓存命中率（0-1；无查询时为 0）",
                &[],
                &[(labels.clone(), stats.hit_ratio())],
            );
            match cache.footprint(ns).await {
                Ok((entries, bytes)) => {
                    metric(
                        &mut out,
                        "hotpot_cache_index_entries",
                        "gauge",
                        "缓存索引中的条目数",
                        &[],
                        &[(labels.clone(), entries as f64)],
                    );
                    metric(
                        &mut out,
                        "hotpot_cache_index_bytes",
                        "gauge",
                        "缓存索引记录的逻辑字节数（去重前）",
                        &[],
                        &[(labels, bytes as f64)],
                    );
                }
                Err(e) => out.push_str(&format!(
                    "# hotpot_cache_index_entries({protocol}) unavailable: {e}\n"
                )),
            }
        }
    }

    // --- 产物 CAS ---
    match source.store.stored_bytes() {
        Ok(bytes) => metric(
            &mut out,
            "hotpot_store_bytes",
            "gauge",
            "本地产物/缓存 CAS 占用的磁盘字节数",
            &[],
            &[(vec![], bytes as f64)],
        ),
        Err(e) => out.push_str(&format!("# hotpot_store_bytes unavailable: {e}\n")),
    }

    // --- 资源回收 ---
    {
        use std::sync::atomic::Ordering::Relaxed;
        let g = &source.gc_stats;
        metric(
            &mut out,
            "hotpot_gc_sweeps_total",
            "counter",
            "资源回收扫描轮次",
            &[],
            &[(vec![], g.sweeps.load(Relaxed) as f64)],
        );
        metric(
            &mut out,
            "hotpot_gc_sessions_present",
            "gauge",
            "最近一轮扫描到的会话目录数",
            &[],
            &[(vec![], g.sessions_present.load(Relaxed) as f64)],
        );
        metric(
            &mut out,
            "hotpot_gc_sessions_removed_total",
            "counter",
            "累计删除的会话目录数",
            &[],
            &[(vec![], g.sessions_removed.load(Relaxed) as f64)],
        );
        metric(
            &mut out,
            "hotpot_gc_sessions_skipped_active_total",
            "counter",
            "因构建非终态而跳过的会话目录数（活跃构建保护生效的证据）",
            &[],
            &[(
                vec![("reason", "build_active".to_string())],
                g.sessions_skipped_active.load(Relaxed) as f64,
            )],
        );
        metric(
            &mut out,
            "hotpot_gc_session_bytes_freed_total",
            "counter",
            "累计由会话回收释放的字节数",
            &[],
            &[(vec![], g.session_bytes_freed.load(Relaxed) as f64)],
        );
        metric(
            &mut out,
            "hotpot_gc_git_workspaces_removed_total",
            "counter",
            "累计删除的 git 共享工作区数",
            &[],
            &[(vec![], g.workspaces_removed.load(Relaxed) as f64)],
        );
        metric(
            &mut out,
            "hotpot_gc_git_workspace_bytes_freed_total",
            "counter",
            "累计由 git 工作区回收释放的字节数",
            &[],
            &[(vec![], g.workspace_bytes_freed.load(Relaxed) as f64)],
        );
        metric(
            &mut out,
            "hotpot_gc_cas_runs_total",
            "counter",
            "CAS 容量回收轮次",
            &[],
            &[(vec![], g.cas_gc_runs.load(Relaxed) as f64)],
        );
        metric(
            &mut out,
            "hotpot_gc_cas_bytes_freed_total",
            "counter",
            "CAS 容量回收累计释放的字节数",
            &[],
            &[(vec![], g.cas_bytes_freed.load(Relaxed) as f64)],
        );
    }

    // --- 运行时信息 ---
    metric(
        &mut out,
        "hotpot_workers",
        "gauge",
        "内嵌 worker 数",
        &[],
        &[(
            vec![("executor", source.executor.clone())],
            source.workers as f64,
        )],
    );
    metric(
        &mut out,
        "hotpot_info",
        "gauge",
        "服务信息（恒为 1）",
        &[
            ("version", source.version.to_string()),
            ("toolchain", source.toolchain.clone()),
        ],
        &[(vec![("executor", source.executor.clone())], 1.0)],
    );

    out
}

/// `GET /metrics` 处理器。
pub async fn handler(State(source): State<MetricsSource>) -> Response {
    let body = render(&source).await;
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// 输出一条指标（含 HELP / TYPE）。
fn metric(
    out: &mut String,
    name: &str,
    kind: &str,
    help: &str,
    const_labels: &[(&str, String)],
    samples: &[(Vec<(&str, String)>, f64)],
) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
    for (labels, value) in samples {
        out.push_str(name);
        out.push('{');
        let mut all: Vec<(&str, String)> = const_labels.to_vec();
        all.extend(labels.iter().cloned());
        let rendered: Vec<String> = all
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
            .collect();
        out.push_str(&rendered.join(","));
        out.push_str(&format!("}} {value}\n"));
    }
}

/// Prometheus label 值转义（`\`、`"`、换行）。
fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn status_name(status: hotpot_core::BuildStatus) -> &'static str {
    use hotpot_core::BuildStatus::*;
    match status {
        Queued => "queued",
        Dispatched => "dispatched",
        Running => "running",
        Succeeded => "succeeded",
        Failed => "failed",
        Canceled => "canceled",
        Timeout => "timeout",
    }
}
