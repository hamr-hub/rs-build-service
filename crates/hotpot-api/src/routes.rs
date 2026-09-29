//! HTTP 路由与处理器。

use std::convert::Infallible;
use std::path::Path;
use std::time::Duration;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hotpot_core::model::{BuildProfile, BuildRecord, BuildStatus, SourceSpec};
use hotpot_core::{ContentDigest, EventKind};
use hotpot_store::BlobStore;
use serde::Deserialize;
use tracing::warn;

use crate::error::{ApiError, parse_build_id};
use crate::state::AppState;

const EVENT_BATCH: u32 = 512;
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// 构建应用路由。
pub fn router(state: AppState) -> Router {
    let metrics = crate::metrics::MetricsSource {
        scheduler: state.scheduler.clone(),
        store: state.store.clone(),
        cache: state.cache.clone(),
        workers: state.workers,
        executor: format!("{:?}", state.executor),
        toolchain: state.default_toolchain.clone(),
        version: env!("CARGO_PKG_VERSION"),
    };
    let toolchains = crate::toolchains::ToolchainState {
        docker_host: match &*state.executor {
            hotpot_worker::ExecutorKind::Docker { docker_host, .. } => docker_host.clone(),
            hotpot_worker::ExecutorKind::Local => None,
        },
    };
    // axum 的 `Router<S>` 每个 S 只能 `with_state` 一次，因此按状态分组
    // 构造子路由再 merge——比把所有状态揉进 AppState 更清晰。
    let builds = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/builds", post(create_build).get(list_builds))
        .route("/v1/builds/{id}", get(get_build))
        .route("/v1/builds/{id}/logs/stream", get(logs_stream))
        .route("/v1/builds/{id}/cancel", post(cancel_build))
        .route("/v1/builds/{id}/artifacts", get(list_artifacts))
        .route("/v1/artifacts/{digest}", get(download_artifact))
        .with_state(state);
    let discovery = Router::new()
        .route("/v1/toolchains", get(crate::toolchains::inventory))
        .with_state(toolchains);
    let observability = Router::new()
        .route("/metrics", get(crate::metrics::handler))
        .with_state(metrics);

    builds.merge(discovery).merge(observability)
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
pub struct CreateBuildRequest {
    pub source: SourceSpec,
    #[serde(default)]
    pub profile: Option<BuildProfile>,
}

async fn create_build(
    State(state): State<AppState>,
    Json(req): Json<CreateBuildRequest>,
) -> Result<(StatusCode, Json<BuildRecord>), ApiError> {
    validate_source(&req.source, state.allow_git_source)?;
    let profile = req.profile.unwrap_or_default();
    // 工具链写法、target 三元组形状、列表规模都在系统边界 fail fast：
    // 坏请求不该排到队里再失败（那会浪费一个 worker 槽位并留下误导性记录）。
    hotpot_core::toolchain::validate_profile(&profile).map_err(ApiError::from)?;
    let record = BuildRecord::queued(req.source, profile);
    let record = state.scheduler.enqueue(record).await?;
    Ok((StatusCode::ACCEPTED, Json(record)))
}

#[derive(Deserialize)]
pub struct ListBuildsQuery {
    /// 按状态过滤（省略则返回全部状态）。
    #[serde(default)]
    status: Option<BuildStatus>,
    /// 返回条数（1..=200，默认 50）。
    #[serde(default)]
    limit: Option<u32>,
    /// 偏移（默认 0）。列表按创建时间倒序，分页用 offset 足够。
    #[serde(default)]
    offset: Option<u32>,
}

/// 列出构建（按创建时间倒序）。
async fn list_builds(
    State(state): State<AppState>,
    Query(query): Query<ListBuildsQuery>,
) -> Result<Json<Vec<BuildRecord>>, ApiError> {
    let builds = state
        .scheduler
        .list_builds(
            query.status,
            query.limit.unwrap_or(50),
            query.offset.unwrap_or(0),
        )
        .await?;
    Ok(Json(builds))
}

async fn get_build(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<BuildRecord>, ApiError> {
    let build_id = parse_build_id(&id)?;
    match state.scheduler.get_build(build_id).await? {
        Some(rec) => Ok(Json(rec)),
        None => Err(ApiError::not_found(format!("build {id} not found"))),
    }
}

#[derive(Deserialize)]
struct StreamQuery {
    /// 只返回 seq > since 的事件。
    #[serde(default)]
    since: Option<u64>,
}

async fn logs_stream(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<StreamQuery>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let build_id = parse_build_id(&id)?;
    if state.scheduler.get_build(build_id).await?.is_none() {
        return Err(ApiError::not_found(format!("build {id} not found")));
    }

    let state = state.clone();
    let stream = async_stream::stream! {
        let mut cursor = query.since.unwrap_or(0);
        loop {
            let events = state
                .scheduler
                .list_events(build_id, cursor, EVENT_BATCH)
                .await;
            match events {
                Ok(events) => {
                    for event in events {
                        cursor = event.seq + 1;
                        let data = serde_json::to_string(&event).unwrap_or_default();
                        yield Ok(Event::default().event(event_name(event.kind)).data(data));
                    }
                }
                Err(e) => {
                    warn!("sse list_events failed: {e}");
                    yield Ok(Event::default().event("error").data(e.to_string()));
                    break;
                }
            }

            let terminal = state
                .scheduler
                .get_build(build_id)
                .await
                .ok()
                .flatten()
                .map(|r| r.status.is_terminal())
                .unwrap_or(true);
            if terminal {
                yield Ok(Event::default().event("end").data("{}"));
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn cancel_build(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<BuildRecord>, ApiError> {
    let build_id = parse_build_id(&id)?;
    let record = state
        .scheduler
        .get_build(build_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("build {id} not found")))?;

    if record.status.is_terminal() {
        return Ok(Json(record));
    }
    // 排队任务直接置 canceled；运行中任务通过 watch 通知执行器杀进程。
    if !state.scheduler.cancel_queued(build_id).await? {
        state.signal_cancel(build_id).await;
    }
    let updated = state.scheduler.get_build(build_id).await?.unwrap_or(record);
    Ok(Json(updated))
}

async fn list_artifacts(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<hotpot_core::ArtifactMeta>>, ApiError> {
    let build_id = parse_build_id(&id)?;
    if state.scheduler.get_build(build_id).await?.is_none() {
        return Err(ApiError::not_found(format!("build {id} not found")));
    }
    Ok(Json(state.scheduler.list_artifacts(build_id).await?))
}

#[derive(Deserialize)]
struct DownloadQuery {
    #[serde(default)]
    filename: Option<String>,
}

async fn download_artifact(
    State(state): State<AppState>,
    AxumPath(digest_hex): AxumPath<String>,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    let digest = ContentDigest::from_hex(&digest_hex)
        .map_err(|e| ApiError::bad_request(format!("invalid digest: {e}")))?;
    let data = state
        .store
        .get(&digest)?
        .ok_or_else(|| ApiError::not_found("artifact not found"))?;
    // 触发下载而非内联展示。
    let disposition = match query.filename {
        Some(name) => format!("attachment; filename=\"{name}\""),
        None => "attachment".to_string(),
    };
    let response = (
        StatusCode::OK,
        [(
            header::CONTENT_DISPOSITION,
            header::HeaderValue::from_str(&disposition)
                .map_err(|_| ApiError::internal("bad disposition header"))?,
        )],
        data,
    )
        .into_response();
    Ok(response)
}

/// 源码字段长度上限（防御超长输入）。
const MAX_SOURCE_FIELD: usize = 1024;

/// 在系统边界校验源码。
///
/// - `local`：路径必须存在且含 `Cargo.toml`（fail fast，不让坏请求占队列）；
/// - `git`：仅在服务端显式允许时接受，并校验 url/ref 非空与长度；
///   **未开启时直接 400**，因为这类构建会执行不可信仓库中的代码；
/// - `upload`：尚未实现，明确拒绝。
fn validate_source(source: &SourceSpec, allow_git_source: bool) -> Result<(), ApiError> {
    match source {
        SourceSpec::Local { path } => {
            let p = Path::new(path);
            if !p.is_dir() {
                return Err(ApiError::bad_request(format!(
                    "project path does not exist: {path}"
                )));
            }
            if !p.join("Cargo.toml").is_file() {
                return Err(ApiError::bad_request(format!(
                    "no Cargo.toml under: {path}"
                )));
            }
            Ok(())
        }
        SourceSpec::Git { url, ref_name, sha } => {
            if !allow_git_source {
                return Err(ApiError::bad_request(
                    "git source builds are disabled on this server; \
                     start it with --allow-git-source to enable"
                        .to_string(),
                ));
            }
            for (name, value) in [("url", url), ("ref_name", ref_name)] {
                if value.trim().is_empty() {
                    return Err(ApiError::bad_request(format!(
                        "git {name} must not be empty"
                    )));
                }
                if value.len() > MAX_SOURCE_FIELD {
                    return Err(ApiError::bad_request(format!(
                        "git {name} must be at most {MAX_SOURCE_FIELD} chars"
                    )));
                }
            }
            if let Some(sha) = sha {
                if sha.trim().is_empty() || sha.len() > 64 {
                    return Err(ApiError::bad_request(
                        "git sha must be 1-64 hex chars".to_string(),
                    ));
                }
                if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(ApiError::bad_request(
                        "git sha must be hexadecimal".to_string(),
                    ));
                }
            }
            Ok(())
        }
        SourceSpec::Upload { upload_id, .. } => Err(ApiError::bad_request(format!(
            "upload source is not supported yet (upload_id={upload_id})"
        ))),
    }
}

fn event_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Stdout => "stdout",
        EventKind::Stderr => "stderr",
        EventKind::Phase => "phase",
        EventKind::Status => "status",
    }
}
