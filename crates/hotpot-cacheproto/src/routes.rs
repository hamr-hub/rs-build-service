//! sccache WebDAV 兼容端点与 Turborepo v8 兼容端点。

use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use hotpot_core::ContentDigest;

use crate::remote::{ArtifactMeta, Namespace, PutMeta, RemoteCache};
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
        axum::http::Method::GET => {
            // 流式返回：内存占用与对象大小无关，GB 级缓存条目也能服务。
            match stream_entry(
                &state.cache,
                Namespace::Sccache,
                &key,
                "",
                "application/octet-stream",
            )
            .await
            {
                Ok(response) => response,
                Err(status) => status.into_response(),
            }
        }
        axum::http::Method::HEAD => {
            // HEAD 不读内容，只查索引。真实 sccache 读路径不发 HEAD。
            match state.cache.peek(Namespace::Sccache, &key, "").await {
                Ok(Some(size)) => response_with_length(size),
                Ok(None) => StatusCode::NOT_FOUND.into_response(),
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        axum::http::Method::PUT => {
            // 流式落库：请求体不整体进内存。
            match store_body_streaming(
                &state.cache,
                Namespace::Sccache,
                &key,
                "",
                body,
                PutMeta::default(),
            )
            .await
            {
                Ok(_) => StatusCode::NO_CONTENT.into_response(),
                Err(status) => status.into_response(),
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
    // 流式返回：内存占用与对象大小无关，GB 级 artifact 也能服务。
    match stream_entry(
        &state.cache,
        Namespace::Turbo,
        &hash,
        &query.tenant(),
        "application/octet-stream",
    )
    .await
    {
        Ok(response) => response,
        Err(status) => status.into_response(),
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
    // HEAD 只查索引，不读对象内容；但 Content-Length 必须是 artifact 的真实体积。
    match state
        .cache
        .peek(Namespace::Turbo, &hash, &query.tenant())
        .await
    {
        Ok(Some(size)) => response_with_length(size),
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
    let declared = content_length(headers.get(header::CONTENT_LENGTH));
    if declared.is_some_and(|n| n > DEFAULT_MAX_ARTIFACT_BYTES as i64) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
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

    match store_body_streaming(
        &state.cache,
        Namespace::Turbo,
        &hash,
        &query.tenant(),
        body,
        PutMeta {
            size: declared,
            tag,
            meta,
        },
    )
    .await
    {
        // 201 与官方 mock server 一致；turbo 用 error_for_status()，任意 2xx 均通过。
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(status) => status.into_response(),
    }
}

/// 把请求 body 流式落库：内存占用与对象大小无关。
///
/// 做法是把 axum 的 `Body` 包成 `std::io::Read` 适配器，交给 store 的
/// `put_reader`（在 `spawn_blocking` 里做 zstd + 落盘）。这样一次上传
/// 不再同时持有「请求缓冲 + 压缩缓冲」两份完整拷贝。
///
/// 体积上限仍然生效：由 `DefaultBodyLimit` 在 body 层拦截，超限表现为
/// 读取中途出错 → 按 413 处理。
async fn store_body_streaming(
    cache: &RemoteCache,
    ns: Namespace,
    key: &str,
    tenant: &str,
    body: axum::body::Body,
    put: PutMeta,
) -> Result<ContentDigest, StatusCode> {
    let reader = BodyReader::spawn(body);
    cache
        .put_reader(ns, key, tenant, reader, put)
        .await
        .map_err(|e| {
            tracing::warn!("streaming put failed for {key}: {e}");
            // 读取中途失败通常来自 body 层（超限 / 客户端断开）。
            StatusCode::PAYLOAD_TOO_LARGE
        })
}

/// 读取条目并构造流式响应：内存占用与对象大小无关。
///
/// `Content-Length` 直接取索引里的逻辑大小，无需把对象读进内存。
/// 摘要在传输末尾由 `ObjectReader` 校验；若发现损坏，body 会被截断——
/// 两套协议的客户端都能安全处理（sccache 视为 miss，turbo 验签失败），
/// 因此**不会**把与 key 不符的字节当作正常内容交付。
async fn stream_entry(
    cache: &RemoteCache,
    ns: Namespace,
    key: &str,
    tenant: &str,
    content_type: &'static str,
) -> Result<Response, StatusCode> {
    use axum::body::Body;
    use http_body_util::StreamBody;

    match cache.open_entry(ns, key, tenant).await {
        Ok(Some((reader, size, tag, meta))) => {
            let mut response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, size)
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            apply_turbo_headers(&mut response, tag, meta);
            // 已有响应头（含 Content-Length），只把 body 换成流。
            //
            // store 层是同步 `Read`（zstd 解码本身是同步的），用 tokio-util 的
            // `SyncIoBridge` 桥接到 `AsyncRead`：它在专用 blocking 线程上执行读取，
            // 因此不会占用 async worker 线程。读取中途若发现摘要不匹配，
            // 错误会作为流上的一个 `Err` 项传出，hyper 随即中断响应体——
            // 客户端看到的是截断的下载，而不是「看起来正常但内容错误」的响应。
            *response.body_mut() =
                Body::from_stream(StreamBody::new(reader_to_body_stream(reader)));
            Ok(response)
        }
        Ok(None) => Ok(StatusCode::NOT_FOUND.into_response()),
        Err(e) => {
            internal(e);
            Ok(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

/// 把 turbo 的 artifact 头贴到响应上（tag 必回显，签名客户端依赖它）。
fn apply_turbo_headers(response: &mut Response, tag: Option<String>, meta: ArtifactMeta) {
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
    if let Some(ms) = meta.duration_ms
        && let Ok(value) = header::HeaderValue::from_str(&ms.to_string())
    {
        response.headers_mut().insert("x-artifact-duration", value);
    }
    for (name, value) in [
        ("x-artifact-sha", meta.sha),
        ("x-artifact-dirty-hash", meta.dirty_hash),
    ] {
        if let Some(value) = value
            && let Ok(v) = header::HeaderValue::from_str(&value)
        {
            response.headers_mut().insert(name, v);
        }
    }
}

/// 解析 `Content-Length` 头。
fn content_length(value: Option<&header::HeaderValue>) -> Option<i64> {
    value
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
}

/// 只有 `Content-Length` 的响应（HEAD 用：真实体积、无 body）。
fn response_with_length(size: u64) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_LENGTH, size)
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// 把同步 `Read` 转成响应体流：blocking 线程读盘 + 有界通道背压。
///
/// store 层刻意是同步的（zstd 解码是 CPU 密集的同步 API），因此这里用
/// `spawn_blocking` 承担读取，块经容量为 4 的通道送回 async 侧。
/// 通道有界即背压：客户端读得慢，磁盘读取就会自然减速，而不是把
/// 整个对象堆进内存。
///
/// 读取出错（例如 EOF 时摘要不匹配）会作为流上的 `Err` 项传出，hyper
/// 随即中断响应体：客户端看到的是**截断的下载**，而不是一段看起来正常
/// 但内容与 key 不符的数据。两套协议客户端对此都是安全的失败方式
/// （sccache 视为 miss，turbo 签名校验失败）。
fn reader_to_body_stream(
    mut reader: Box<dyn std::io::Read + Send>,
) -> impl futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
    tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; 128 * 1024];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // 客户端已断开时停止读取。
                    if tx
                        .blocking_send(Ok(bytes::Bytes::copy_from_slice(&buf[..n])))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });
    futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

/// 把 axum `Body` 适配成 `std::io::Read`，供同步的 store 层消费。
///
/// 同步 `Read` 里没法 await，因此用通道解耦：异步侧把 body 的数据帧搬进
/// 通道，同步侧（`spawn_blocking` 内的 store 写入）在 `read` 里阻塞收取。
///
/// **为什么必须是 `tokio::sync::mpsc` 而不是 `std::sync::mpsc`**：
/// std 的有界通道 `send` 是阻塞调用，在异步任务里调用它，一旦通道满就会
/// **永久占住一个 tokio worker 线程**。消费端提前放弃时（例如客户端中断上传、
/// 或存储层写失败提前返回），搬运任务就再也没人来收，于是这条线程被永久
/// 泄漏。反复几次之后整个运行时被拖死——`/healthz` 也不再响应。
/// `tokio::sync::mpsc` 的 `send` 是异步的（满了就挂起而不是阻塞线程），
/// 而 `blocking_recv` 只在 `spawn_blocking` 线程里使用，正是它该出现的地方。
///
/// 通道有界（8 帧）：既提供背压，又把内存占用钉在「几个 64 KiB 块」。
struct BodyReader {
    rx: tokio::sync::mpsc::Receiver<io_chunk::Chunk>,
    buf: bytes::Bytes,
    /// 通道关闭且缓冲已空 = 对端结束或出错。
    source_done: bool,
}

/// 跨线程传递的数据块。`Err` 用字符串而非 `io::Error` 以便跨 `Send` 边界。
mod io_chunk {
    /// 通道元素：`None` 表示正常结束，`Some(err)` 表示读 body 失败。
    pub type Chunk = Result<Option<bytes::Bytes>, String>;
}

impl BodyReader {
    /// 启动搬运任务并返回同步读取端。
    fn spawn(body: axum::body::Body) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel::<io_chunk::Chunk>(8);
        tokio::spawn(async move {
            use futures::StreamExt;
            // `into_data_stream` 只产出数据帧，trailer 等自动跳过。
            let mut stream = body.into_data_stream();
            while let Some(item) = stream.next().await {
                match item {
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string())).await;
                        return;
                    }
                    Ok(data) if data.is_empty() => continue,
                    Ok(data) => {
                        // await：通道满时挂起当前任务，而不是阻塞一个 worker 线程。
                        // 发送端被丢弃时说明读取端已放弃，直接结束搬运。
                        if tx.send(Ok(Some(data))).await.is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Ok(None)).await;
        });
        Self {
            rx,
            buf: bytes::Bytes::new(),
            source_done: false,
        }
    }
}

impl std::io::Read for BodyReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.buf.is_empty() {
            if self.source_done {
                return Ok(0);
            }
            // 只在 spawn_blocking 线程里调用，允许阻塞。
            match self.rx.blocking_recv() {
                Some(Ok(None)) => {
                    self.source_done = true;
                    return Ok(0);
                }
                Some(Ok(Some(data))) => self.buf = data,
                Some(Err(e)) => {
                    self.source_done = true;
                    return Err(std::io::Error::other(format!("read request body: {e}")));
                }
                // 搬运任务被取消：视为流结束。
                None => {
                    self.source_done = true;
                    return Ok(0);
                }
            }
        }
        let n = self.buf.len().min(out.len());
        let chunk = self.buf.split_to(n);
        out[..n].copy_from_slice(&chunk);
        Ok(n)
    }
}

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
/// XML 文本/属性值转义（RFC 4918 要求 href 至少转义这五个字符）。
///
/// 不转义的后果：含 `&`/`<` 的路径会产出非法 multistatus，opendal 反序列化
/// 失败 → 写前探测失败 → sccache 静默降级为只读。
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
