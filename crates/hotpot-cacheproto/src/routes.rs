//! sccache WebDAV 兼容端点与 Turborepo v8 兼容端点。

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use serde::Deserialize;

use crate::remote::Namespace;
use crate::state::CacheState;

/// 缓存协议路由（状态类型与 hotpot-api 一致）。
pub fn router() -> Router<CacheState> {
    Router::new()
        // sccache/opendal WebDAV：完整方法分发。opendal 默认在写前发
        // PROPFIND/MKCOL，因此除 GET/HEAD/PUT 外也对这两个方法返回成功，
        // 不要求客户端设置 disable_create_dir。
        .route("/sccache/{*key}", axum::routing::any(any_sccache))
        // 集合根：通配路由不匹配空段，opendal 恰好会探测这里。
        .route("/sccache", axum::routing::any(any_sccache))
        .route("/sccache/", axum::routing::any(any_sccache))
        // Turborepo v8。
        .route("/v8/artifacts/status", get(turbo_status))
        .route(
            "/v8/artifacts/{hash}",
            get(get_turbo).head(head_turbo),
        )
        .route("/v8/artifacts/{hash}", put(put_turbo))
}

// ---------- sccache ----------

/// opendal WebDAV 单入口：GET/HEAD/PUT 走 CAS 索引；MKCOL/PROPFIND
/// 一律成功（集合是虚拟的，对象即键路径），避免写前探测失败。
async fn any_sccache(State(state): State<CacheState>, req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();
    let key = parts
        .uri
        .path()
        .strip_prefix("/sccache/")
        .unwrap_or("")
        .to_string();

    match parts.method {
        axum::http::Method::GET => match state.cache.get(Namespace::Sccache, &key, "").await {
            Ok(Some(entry)) => octet_stream(entry.bytes, entry.tag),
            Ok(None) => StatusCode::NOT_FOUND.into_response(),
            Err(e) => internal(e),
        },
        axum::http::Method::HEAD => {
            match state.cache.contains(Namespace::Sccache, &key, "").await {
                Ok(true) => StatusCode::OK.into_response(),
                Ok(false) => StatusCode::NOT_FOUND.into_response(),
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        axum::http::Method::PUT => {
            let bytes = match axum::body::to_bytes(body, usize::MAX).await {
                Ok(bytes) => bytes,
                Err(_) => return StatusCode::BAD_REQUEST.into_response(),
            };
            match state
                .cache
                .put(Namespace::Sccache, &key, "", &bytes, None)
                .await
            {
                Ok(_) => StatusCode::NO_CONTENT.into_response(),
                Err(e) => internal(e),
            }
        }
        // 集合探测：目录不落盘，直接告诉客户端存在/已创建。
        m if m == "MKCOL" => StatusCode::CREATED.into_response(),
        m if m == "PROPFIND" => propfind(parts.uri.path()),
        m if m == "OPTIONS" => StatusCode::OK.into_response(),
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

// ---------- Turborepo v8 ----------

async fn turbo_status() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "enabled" }))
}

#[derive(Deserialize)]
struct TurboQuery {
    #[serde(default, rename = "teamId")]
    team_id: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

impl TurboQuery {
    fn tenant(&self) -> String {
        self.team_id
            .clone()
            .or_else(|| self.slug.clone())
            .unwrap_or_default()
    }
}

async fn get_turbo(
    State(state): State<CacheState>,
    AxumPath(hash): AxumPath<String>,
    Query(query): Query<TurboQuery>,
) -> Response {
    match state
        .cache
        .get(Namespace::Turbo, &hash, &query.tenant())
        .await
    {
        Ok(Some(entry)) => octet_stream(entry.bytes, entry.tag),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal(e),
    }
}

async fn head_turbo(
    State(state): State<CacheState>,
    AxumPath(hash): AxumPath<String>,
    Query(query): Query<TurboQuery>,
) -> StatusCode {
    match state
        .cache
        .contains(Namespace::Turbo, &hash, &query.tenant())
        .await
    {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[axum::debug_handler]
async fn put_turbo(
    State(state): State<CacheState>,
    AxumPath(hash): AxumPath<String>,
    Query(query): Query<TurboQuery>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> StatusCode {
    let tag = headers
        .get("x-artifact-tag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match state
        .cache
        .put(Namespace::Turbo, &hash, &query.tenant(), &body, tag)
        .await
    {
        Ok(_) => StatusCode::CREATED,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// ---------- helpers ----------

/// 最小合法的 WebDAV PROPFIND 207 响应。opendal 用它判断父集合是否存在，
/// 空体会触发 XML 反序列化错误并把存储降级为只读。
fn propfind(path: &str) -> Response {
    let is_collection = path.ends_with('/');
    let resource_type = if is_collection {
        "<resourcetype><collection/></resourcetype>"
    } else {
        "<resourcetype/>"
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<multistatus xmlns=\"DAV:\">\
<response><href>{path}</href>\
<propstat><prop>{resource_type}\
<getlastmodified>Sat, 01 Jan 2000 00:00:00 GMT</getlastmodified>\
<getcontentlength>0</getcontentlength></prop>\
<status>HTTP/1.1 200 OK</status></propstat>\
</response></multistatus>"
    );
    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .header(header::CONTENT_LENGTH, xml.len())
        .body(axum::body::Body::from(xml))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn octet_stream(bytes: Vec<u8>, tag: Option<String>) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    if let Some(tag) = tag {
        if let Ok(value) = header::HeaderValue::from_str(&tag) {
            response.headers_mut().insert("x-artifact-tag", value);
        }
    }
    response
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::warn!("cacheproto error: {e}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
