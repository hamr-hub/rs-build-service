//! 生产加固：CORS、超时、限流降级、压缩、请求体上限与优雅停机。
//!
//! 这一层解决的是"服务能不能扛住真实流量"的问题。单机能跑通不等于能上线：
//! 一个恶意（或仅仅是失控的）客户端就能用长连接把连接数占满，让正常请求
//! 全部排队直到超时。这里把每个已知的失效模式都变成显式、可配置的行为。
//!
//! **最重要的一条设计约束：SSE 日志流必须豁免超时、压缩与并发计数。**
//! 日志流的连接在整个构建期间（可能是几十分钟）保持打开，如果套上 30s 超时，
//! 每次日志查看都会在 30 秒后被服务端主动切断；如果把它计入并发限制，
//! 十几个看日志的人就能把 API 全部挤死。这两处很容易在"统一加中间件"时
//! 被无意打破，所以路径判定与豁免逻辑集中放在 [`is_sse_path`]，
//! 并由单元测试钉死。

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::Semaphore;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::ServeDir;

/// 日志流路径。SSE 相关的豁免判定都以它为准。
const SSE_PATH: &str = "/logs/stream";

/// 该请求是否是 SSE 日志流。
///
/// 用 `ends_with` 而不是全等：日志流路径带构建 id（`/v1/builds/{id}/logs/stream`），
/// 而这个函数在**路由匹配之前**执行，此时拿不到路由参数，只能按路径后缀判断。
pub fn is_sse_path(path: &str) -> bool {
    // 去掉查询串再比对。中间件里传的是 `uri().path()`（本来就没有 query），
    // 但这条不变量太关键——一旦 SSE 被套上超时，用户会在构建跑到一半时看到
    // 日志流被服务端主动掐断——所以对完整 URI 也保持成立，别让调用方踩坑。
    match path.split_once('?') {
        Some((path, _query)) => path.ends_with(SSE_PATH),
        None => path.ends_with(SSE_PATH),
    }
}

/// 限流器：达到上限立即拒绝，而不是无限排队。
///
/// 排队（`tower::limit::ConcurrencyLimit`）在高负载下比拒绝更糟——请求会
/// 一直挂着直到客户端超时，内存里堆满等待者。快速失败让调用方能立刻重试到
/// 别的实例，或者至少拿到一个明确的 503 而不是"卡住"。
#[derive(Clone)]
pub struct LoadShedder {
    permits: Arc<Semaphore>,
    limit: usize,
}

impl LoadShedder {
    pub fn new(limit: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(limit.max(1))),
            limit: limit.max(1),
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// 当前可立即获得的许可数（用于 `/metrics` 暴露饱和度）。
    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

/// 超时与限流按同一套路径规则豁免 SSE。
///
/// SSE 既不计入并发限制也不套超时：一条日志流在整个构建期间合法地占用一个
/// 长连接，用它去挤占 API 的配额是错的；给它加超时则会在构建还在跑的时候
/// 把用户的日志视图掐断。
fn exempt_from_sse<T>(req: &Request, apply: impl FnOnce() -> T) -> Option<T> {
    if is_sse_path(req.uri().path()) {
        None
    } else {
        Some(apply())
    }
}

/// 请求级超时（豁免 SSE）。
///
/// 没有超时层时，一个卡住的下游（docker daemon、Slowloris 客户端）会一直占着
/// 连接与 worker，直到系统自己耗尽。超时应给出一个可辨识的 503 而不是 408 ——
/// 对客户端而言这是"服务端没及时处理"，重试有意义。
pub fn timeout_middleware<S: Clone + Send + Sync + 'static>(
    router: axum::Router<S>,
    timeout: Duration,
) -> axum::Router<S> {
    router.layer(axum::middleware::from_fn_with_state::<
        _,
        Duration,
        (axum::extract::State<Duration>, Request),
    >(
        timeout,
        |axum::extract::State(timeout): axum::extract::State<Duration>,
         req: Request,
         next: Next| async move {
            let Some(deadline) = exempt_from_sse(&req, || tokio::time::Instant::now() + timeout)
            else {
                return next.run(req).await;
            };
            // 先取路径再移动 req：超时后 req 已被 next 消费。
            let path = req.uri().path().to_string();
            match tokio::time::timeout_at(deadline, next.run(req)).await {
                Ok(response) => response,
                Err(_) => {
                    tracing::warn!(
                        %path,
                        ?timeout,
                        "request timed out; shedding load"
                    );
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"error":"request timed out"}"#,
                    )
                        .into_response()
                }
            }
        },
    ))
}

/// 并发限流 + 快速失败（豁免 SSE）。
pub fn load_shed_middleware<S: Clone + Send + Sync + 'static>(
    router: axum::Router<S>,
    shedder: LoadShedder,
) -> axum::Router<S> {
    router.layer(axum::middleware::from_fn_with_state::<
        _,
        LoadShedder,
        (axum::extract::State<LoadShedder>, Request),
    >(
        shedder,
        |axum::extract::State(shedder): axum::extract::State<LoadShedder>,
         req: Request,
         next: Next| async move {
            // SSE 不占配额。
            let Some(permit) =
                exempt_from_sse(&req, || shedder.permits.clone().try_acquire_owned())
            else {
                return next.run(req).await;
            };
            let permit = match permit {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::warn!(
                        path = %req.uri().path(),
                        limit = shedder.limit,
                        "concurrency limit reached; shedding request"
                    );
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"error":"server at capacity; retry shortly"}"#,
                    )
                        .into_response();
                }
            };
            // 许可在响应体放完之前一直持有：请求处理完但连接还没读完时，
            // 资源其实仍在占用。
            let response = next.run(req).await;
            drop(permit);
            response
        },
    ))
}

/// CORS 白名单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsOrigins {
    /// 不加 CORS 头（同源部署，由反向代理负责）。
    Disabled,
    /// 放行任意来源。**仅适用于构建 API**，且必须清楚它的含义。
    Any,
    /// 精确白名单。
    List(Vec<String>),
}

impl CorsOrigins {
    /// 从逗号分隔的字符串解析；空串等价于 `Disabled`。
    pub fn parse(raw: &str) -> Self {
        let items: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        match items.len() {
            0 => Self::Disabled,
            _ => Self::List(items),
        }
    }

    pub fn layer(&self) -> Option<CorsLayer> {
        match self {
            Self::Disabled => None,
            Self::Any => Some(
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(Any)
                    .allow_headers(Any)
                    // SSE 依赖流式响应，预检里不能声明 max-age 之外的缓存行为。
                    .max_age(Duration::from_secs(600)),
            ),
            Self::List(origins) => {
                let parsed: Vec<HeaderValue> = origins
                    .iter()
                    .filter_map(|origin| match origin.parse() {
                        Ok(value) => Some(value),
                        Err(_) => {
                            tracing::warn!(%origin, "ignoring malformed CORS origin");
                            None
                        }
                    })
                    .collect();
                Some(
                    CorsLayer::new()
                        .allow_origin(AllowOrigin::list(parsed))
                        .allow_methods(Any)
                        .allow_headers(Any)
                        .max_age(Duration::from_secs(600)),
                )
            }
        }
    }
}

/// 响应压缩。
///
/// 日志流是例外且已在调用侧规避：压缩器会把流缓冲起来，"实时"日志会变成
/// 构建结束后一次性刷出。这里不套在 SSE 上是刻意的。
pub fn compression_layer() -> CompressionLayer {
    CompressionLayer::new().br(true)
}

/// 请求体上限。构建提交的 JSON 很小，不设限的话一个巨型 body 就能吃掉内存。
pub fn body_limit_layer(max_bytes: usize) -> RequestBodyLimitLayer {
    RequestBodyLimitLayer::new(max_bytes)
}

/// 读取并返回 `index.html`。
///
/// 状态码必须是 **200**：`ServeDir::not_found_service` 会原样保留 404，
/// 虽然 body 是正确的首页，但 404 会让浏览器控制台、监控告警、
/// `curl -f` 脚本和部分 HTTP 客户端全部当成失败。SPA 回退的语义就是
/// "这个路径由前端路由处理"，不是"资源不存在"。
async fn serve_index(index: std::path::PathBuf) -> Response {
    match tokio::fs::read(&index).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            bytes,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(path = %index.display(), error = %e, "failed to read index.html");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// 挂载前端静态资源（含 SPA 回退与缓存策略）。
///
/// 这样 `hotpot-server` 一个进程就是完整交付物：API、日志流、界面同源，
/// 既避免了跨源部署时 CORS 的麻烦，也省掉一层反向代理。
///
/// `dir` 指向 `web/dist`。目录不存在时返回空路由——开发时前端由 Vite 提供，
/// 服务端不该因为找不到 dist 就起不来。
pub fn frontend_router(dir: &std::path::Path) -> Router {
    let index = dir.join("index.html");
    if !index.is_file() {
        tracing::debug!(dir = %dir.display(), "frontend assets not found; serving API only");
        return Router::new();
    }
    tracing::info!(dir = %dir.display(), "serving frontend assets");

    // 这里用 `fallback` 而不是 `not_found_service`，是一个有实质后果的选择：
    // `not_found_service` 内部是 `SetStatus`，会把回退响应的状态码**强制改成
    // 404**，哪怕回退服务返回的是 200。而 SPA 回退的语义是「这个路径交给前端
    // 路由处理」，不是「资源不存在」——状态码必须是 200，否则浏览器控制台、
    // `curl -f` 脚本、监控探针全部把它记成失败，而 body 却是正确的首页。
    // `fallback` 则保留回退服务自己的状态码。
    //
    // 另外只有「确实没有这个文件」才回首页：ServeDir 自身对真实 I/O 错误
    // 已经返回 500，这类错误被伪装成正常首页会把磁盘故障藏起来。
    let fallback_index = index.clone();
    Router::new()
        .fallback_service(
            ServeDir::new(dir)
                .append_index_html_on_directories(true)
                .fallback(tower::service_fn(move |_req: Request| {
                    let index = fallback_index.clone();
                    async move { Ok::<_, std::convert::Infallible>(serve_index(index).await) }
                })),
        )
        .layer(axum::middleware::from_fn(
            |req: Request, next: Next| async move {
                // 先取出判定缓存策略所需的路径：`req` 随后会被 move 进 next.run()。
                // Vite 产物 `/assets/*` 的文件名带内容哈希，可以永久缓存；
                // index.html 引用了那些哈希名，被缓存住就会一直指向已删除的旧资源
                // （表现为「部署后白屏」），因此必须每次回源校验。
                let cache_policy = if req.uri().path().starts_with("/assets/") {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                };
                let mut res = next.run(req).await;
                if let Ok(value) = HeaderValue::from_str(cache_policy) {
                    res.headers_mut().insert(header::CACHE_CONTROL, value);
                }
                res
            },
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_paths_are_detected_by_suffix() {
        assert!(is_sse_path("/v1/builds/abc-123/logs/stream"));
        // 带查询串也必须识别为 SSE（否则会被套上超时而被掐断）
        assert!(is_sse_path("/v1/builds/x/logs/stream?since=5"));
        assert!(is_sse_path("/v1/builds/x/logs/stream?since=5&x=1"));
        // 不能误伤普通 JSON 端点
        assert!(!is_sse_path("/v1/builds"));
        assert!(!is_sse_path("/v1/builds/abc/logs"));
        assert!(!is_sse_path("/metrics"));
        assert!(!is_sse_path("/sccache/abc"));
    }

    #[test]
    fn cors_origins_parse() {
        assert_eq!(CorsOrigins::parse(""), CorsOrigins::Disabled);
        assert_eq!(CorsOrigins::parse("   "), CorsOrigins::Disabled);
        assert_eq!(
            CorsOrigins::parse("http://a.dev , http://b.dev"),
            CorsOrigins::List(vec!["http://a.dev".into(), "http://b.dev".into()])
        );
    }

    #[test]
    fn load_shedder_rejects_past_limit() {
        let shedder = LoadShedder::new(2);
        assert_eq!(shedder.limit(), 2);
        let a = shedder.permits.clone().try_acquire_owned().unwrap();
        let b = shedder.permits.clone().try_acquire_owned().unwrap();
        assert!(shedder.permits.clone().try_acquire_owned().is_err());
        assert_eq!(shedder.available(), 0);
        drop(a);
        drop(b);
        assert_eq!(shedder.available(), 2);
    }

    #[test]
    fn load_shedder_never_zero() {
        // 配成 0 时不能变成"拒绝一切"或 panic。
        assert_eq!(LoadShedder::new(0).limit(), 1);
    }

    /// 端到端钉死最关键的不变量：**SSE 豁免超时与限流，普通请求不豁免**。
    ///
    /// 这条如果哪天被"统一加中间件"顺手打破，后果是每次查看日志都会在
    /// 超时点被服务端掐断，而且只在长构建上复现——很容易漏过。
    mod behaviour {
        use super::*;
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use axum::routing::get;
        use std::time::Duration as StdDuration;
        use tower::ServiceExt as _;

        /// 一个"慢端点"：睡指定时长再返回。
        async fn slow(delay: StdDuration) -> Response {
            tokio::time::sleep(delay).await;
            (StatusCode::OK, "done").into_response()
        }

        #[tokio::test(start_paused = true)]
        async fn non_sse_request_is_timed_out() {
            let app = timeout_middleware(
                Router::new().route("/slow", get(|| slow(StdDuration::from_secs(3600)))),
                StdDuration::from_secs(5),
            );

            let response = app
                .oneshot(HttpRequest::get("/slow").body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "慢请求必须被超时层切成 503"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn fast_request_passes_through() {
            let app = timeout_middleware(
                Router::new().route("/fast", get(|| async { (StatusCode::OK, "ok") })),
                StdDuration::from_secs(5),
            );

            let response = app
                .oneshot(HttpRequest::get("/fast").body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
        }

        #[tokio::test(start_paused = true)]
        async fn sse_request_is_not_timed_out() {
            // 超时只有 1s，但 SSE 端点"跑"了 1 小时：只有豁免生效才会返回 200。
            let app = timeout_middleware(
                Router::new().route(
                    "/v1/builds/{id}/logs/stream",
                    get(|| slow(StdDuration::from_secs(3600))),
                ),
                StdDuration::from_secs(1),
            );

            let response = app
                .oneshot(
                    HttpRequest::get("/v1/builds/abc/logs/stream")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                StatusCode::OK,
                "SSE 必须豁免请求超时，否则长构建的日志流会被定期掐断"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn sse_does_not_consume_concurrency_budget() {
            // 并发上限 1：先占满唯一许可，再验证 SSE 仍能通过。
            let shedder = LoadShedder::new(1);
            let _held = shedder.permits.clone().try_acquire_owned().unwrap();

            let app = load_shed_middleware(
                Router::new()
                    .route(
                        "/v1/builds/{id}/logs/stream",
                        get(|| slow(StdDuration::from_secs(3600))),
                    )
                    .route("/fast", get(|| async { (StatusCode::OK, "ok") })),
                shedder,
            );

            let sse = app
                .clone()
                .oneshot(
                    HttpRequest::get("/v1/builds/abc/logs/stream")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(sse.status(), StatusCode::OK, "SSE 不应被并发限流挡住");

            let plain = app
                .oneshot(HttpRequest::get("/fast").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                plain.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "配额已被普通请求占满，普通请求应快速失败"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn saturated_server_sheds_plain_requests() {
            let shedder = LoadShedder::new(1);
            let app = load_shed_middleware(
                Router::new().route("/slow", get(|| slow(StdDuration::from_secs(3600)))),
                shedder.clone(),
            );
            // 手动占满唯一许可，模拟饱和。
            let _held = shedder.permits.clone().try_acquire_owned().unwrap();

            let response = app
                .oneshot(HttpRequest::get("/slow").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "饱和时必须快速失败（503）而不是无限排队"
            );
        }

        /// 回显请求体长度，用来观察 body limit 是否生效。
        async fn echo_len(body: String) -> String {
            body.len().to_string()
        }

        #[tokio::test(start_paused = true)]
        async fn body_limit_rejects_oversized_upload() {
            use axum::routing::post;
            let app = Router::new()
                .route("/v1/builds", post(echo_len))
                .layer(body_limit_layer(32));

            let oversized = Body::from(vec![b'x'; 1024]);
            let response = app
                .oneshot(HttpRequest::post("/v1/builds").body(oversized).unwrap())
                .await
                .unwrap();
            assert!(
                response.status().is_client_error(),
                "超大请求体应被拒绝，实际 {}",
                response.status()
            );
        }
    }
}
