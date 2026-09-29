//! sccache WebDAV 兼容端点与 Turborepo v8 兼容端点。

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use crate::remote::{ArtifactMeta, Namespace};
use crate::state::CacheState;

/// 单个 artifact 的体积上限（字节）。turbo 的 tar 产物可以到 GB 级，
/// 但必须设上限：否则一个 PUT 就能把服务内存吃光（`to_bytes(usize::MAX)` 是无界的）。
/// 超限返回 413，客户端会当作上传失败并降级告警，不影响构建正确性。
pub const DEFAULT_MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024 * 1024;

/// 缓存协议路由（状态类型与 hotpot-api 一致）。
///
/// `token` 为 Some 时对全部缓存端点启用 Bearer 鉴权。
pub fn router(token: Option<std::sync::Arc<str>>) -> Router<CacheState> {
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
            get(get_turbo)
                .head(head_turbo)
                .put(put_turbo)
                .options(turbo_preflight),
        )
        .layer(axum::extract::DefaultBodyLimit::max(
            DEFAULT_MAX_ARTIFACT_BYTES,
        ))
        .layer(axum::middleware::from_fn_with_state(
            crate::state::AuthState(token),
            crate::state::require_token,
        ))
}

/// turbo 在 `--preflight` / `TURBO_PREFLIGHT=true` 时会先发 OPTIONS，
/// 并根据 `Access-Control-Allow-Headers` 决定是否携带 Authorization。
/// 回 405 会让预检失败，故显式应答。
async fn turbo_preflight() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, HEAD, PUT, OPTIONS",
        )
        .header(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "Authorization, Content-Type, User-Agent, Content-Length, \
             x-artifact-duration, x-artifact-tag, x-artifact-sha, x-artifact-dirty-hash",
        )
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------- sccache ----------

/// sccache 启动能力探测用的哨兵 key。
///
/// sccache 启动时对每个 remote storage 做一次探测：`GET .sccache_check`
/// （404 被容忍）后 `PUT .sccache_check`。**若该 PUT 不是 2xx，sccache 会静默
/// 降级为只读**，远端写入全部丢弃且不报错——这是最难排查的失败模式。
/// 因此这里显式短路：不落 CAS、不入索引，但保证 404/204 语义稳定，
/// 避免将来给 key 加格式校验时把它打破。
const SCCACHE_CHECK_KEY: &str = ".sccache_check";

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

    // 能力探测哨兵：稳定应答且不落盘。
    if key == SCCACHE_CHECK_KEY {
        return match parts.method {
            axum::http::Method::GET | axum::http::Method::HEAD => {
                StatusCode::NOT_FOUND.into_response()
            }
            axum::http::Method::PUT => StatusCode::NO_CONTENT.into_response(),
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        };
    }

    match parts.method {
        axum::http::Method::GET => match state.cache.get(Namespace::Sccache, &key, "").await {
            Ok(Some(entry)) => octet_stream(entry.bytes, entry.tag, None),
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
            let bytes = match read_body_limited(body).await {
                Ok(bytes) => bytes,
                Err(status) => return status.into_response(),
            };
            match state
                .cache
                .put(
                    Namespace::Sccache,
                    &key,
                    "",
                    &bytes,
                    None,
                    Default::default(),
                )
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
        Ok(Some(entry)) => octet_stream(entry.bytes, entry.tag, Some(entry.meta)),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal(e),
    }
}

/// HEAD：与 GET 构造相同的响应头但不带 body。
/// spec 为 HEAD 200 声明了 `Content-Length` / `x-artifact-*`，返回裸状态码会让
/// turbo 拿不到 size 与 time-saved。
async fn head_turbo(
    State(state): State<CacheState>,
    AxumPath(hash): AxumPath<String>,
    Query(query): Query<TurboQuery>,
) -> Response {
    match state
        .cache
        .get(Namespace::Turbo, &hash, &query.tenant())
        .await
    {
        Ok(Some(entry)) => {
            let mut response = octet_stream(Vec::new(), entry.tag, Some(entry.meta));
            // HEAD 不带 body，但 Content-Length 必须是 artifact 的真实体积。
            if let Ok(value) = header::HeaderValue::from_str(&entry.size.to_string()) {
                response.headers_mut().insert(header::CONTENT_LENGTH, value);
            }
            response
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal(e),
    }
}

async fn put_turbo(
    State(state): State<CacheState>,
    AxumPath(hash): AxumPath<String>,
    Query(query): Query<TurboQuery>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Response {
    // spec 把 Content-Length 标为 required；与实际长度不符会让 turbo 侧的
    // 签名校验（body_len 是 HMAC 消息字段）失败，故显式拒绝。
    if let Some(declared) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if declared > DEFAULT_MAX_ARTIFACT_BYTES as u64 {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
    }

    let tag = headers
        .get("x-artifact-tag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let meta = ArtifactMeta {
        duration_ms: headers
            .get("x-artifact-duration")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok()),
        sha: headers
            .get("x-artifact-sha")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        dirty_hash: headers
            .get("x-artifact-dirty-hash")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    };

    let bytes = match read_body_limited(body).await {
        Ok(bytes) => bytes,
        Err(status) => return status.into_response(),
    };
    match state
        .cache
        .put(Namespace::Turbo, &hash, &query.tenant(), &bytes, tag, meta)
        .await
    {
        // 201 与官方 mock server 一致；turbo 用 error_for_status()，任意 2xx 均通过。
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// 读 body 并强制体积上限：超限 413，其它读失败 400。
async fn read_body_limited(body: axum::body::Body) -> Result<Bytes, StatusCode> {
    axum::body::to_bytes(body, DEFAULT_MAX_ARTIFACT_BYTES)
        .await
        .map_err(|e| {
            if e.to_string().contains("length limit") {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            }
        })
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
    // href 回显请求路径，必须做 XML 转义，否则含 & 或 < 的路径会产出非法 XML，
    // opendal 反序列化失败 → create_dir/write 失败 → sccache 静默降级只读。
    let href = xml_escape(path);
    // opendal 用 getlastmodified 的**存在性**判断 multistatus 解析成功与否，
    // 不校验其值；集合是虚拟的，没有真实 mtime，故用固定合法 RFC 1123 值。
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<multistatus xmlns=\"DAV:\">\
<response><href>{href}</href>\
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

/// XML 文本/属性值转义（RFC 3986 & RFC 4918 要求 href 至少转义这五个字符）。
fn xml_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn octet_stream(bytes: Vec<u8>, tag: Option<String>, meta: Option<ArtifactMeta>) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    if let Some(tag) = tag {
        // 头非法时必须可见：签名客户端缺 tag 是硬错误，静默丢弃极难排查。
        match header::HeaderValue::from_str(&tag) {
            Ok(value) => {
                response.headers_mut().insert("x-artifact-tag", value);
            }
            Err(e) => {
                tracing::warn!(
                    "dropping invalid x-artifact-tag ({e}); signature clients will fail"
                );
            }
        }
    }
    if let Some(meta) = meta {
        if let Some(ms) = meta.duration_ms {
            if let Ok(value) = header::HeaderValue::from_str(&ms.to_string()) {
                response.headers_mut().insert("x-artifact-duration", value);
            }
        }
        for (name, value) in [
            ("x-artifact-sha", meta.sha),
            ("x-artifact-dirty-hash", meta.dirty_hash),
        ] {
            if let Some(value) = value {
                if let Ok(v) = header::HeaderValue::from_str(&value) {
                    response.headers_mut().insert(name, v);
                }
            }
        }
    }
    response
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::warn!("cacheproto error: {e}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[cfg(test)]
mod tests {
    use super::xml_escape;

    #[test]
    fn escapes_xml_metacharacters() {
        assert_eq!(xml_escape("/sccache/a&b/"), "/sccache/a&amp;b/");
        assert_eq!(xml_escape("<script>"), "&lt;script&gt;");
        assert_eq!(xml_escape("a\"b"), "a&quot;b");
        assert_eq!(xml_escape("it's"), "it&apos;s");
        // 普通路径（含百分号编码）必须原样保留。
        assert_eq!(xml_escape("/sccache/a%26b/"), "/sccache/a%26b/");
    }
}
