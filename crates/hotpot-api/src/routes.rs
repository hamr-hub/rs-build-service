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
use hotpot_core::model::{BuildProfile, BuildRecord, SourceSpec};
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
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/v1/builds",
            post(create_build).get(list_builds_placeholder),
        )
        .route("/v1/builds/{id}", get(get_build))
        .route("/v1/builds/{id}/logs/stream", get(logs_stream))
        .route("/v1/builds/{id}/cancel", post(cancel_build))
        .route("/v1/builds/{id}/artifacts", get(list_artifacts))
        .route("/v1/artifacts/{digest}", get(download_artifact))
        .with_state(state)
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
    validate_source(&req.source)?;
    let record = BuildRecord::queued(req.source, req.profile.unwrap_or_default());
    let record = state.scheduler.enqueue(record).await?;
    Ok((StatusCode::ACCEPTED, Json(record)))
}

/// M2 未实现构建列表；显式返回 405 而非静默 404。
async fn list_builds_placeholder() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
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

/// 在系统边界校验源码：M2 仅支持本机 Local 路径且必须是 Cargo 项目。
fn validate_source(source: &SourceSpec) -> Result<(), ApiError> {
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
        other => Err(ApiError::bad_request(format!(
            "source kind not supported yet: {}",
            serde_json::to_string(other).unwrap_or_default()
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
