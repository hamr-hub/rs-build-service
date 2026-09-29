//! 缓存协议 HTTP 层契约测试。
//!
//! 为什么必须有这层测试：`RemoteCache` 的行为测试无法锁住 HTTP 语义，
//! 而协议兼容的失败模式是**静默**的——sccache 写前探测失败会退化为只读，
//! turbo 收到非 404 的错误会中断构建。状态码、Content-Length、租户隔离、
//! `x-artifact-tag` 回显这些都必须有回归网。

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use hotpot_cacheproto::{CacheState, RemoteCache, router};
use hotpot_store::local::{LocalStore, StoreOptions};
use tower::ServiceExt;

/// 构造被测应用。
///
/// 返回的 `TempDir` **必须**由调用方持有到测试结束：`RemoteCache` 打开的
/// SQLite 文件与 CAS 目录都在其中，提前 drop 会 unlink 文件，产生与协议
/// 无关的假故障（例如读到 500）。
async fn app() -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path().join("store"), StoreOptions::default()).unwrap();
    let cache = RemoteCache::open(dir.path(), store).await.unwrap();
    (router(None).with_state(CacheState::new(cache)), dir)
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    body: &'static [u8],
) -> axum::response::Response {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(body))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

const KEY: &str = "ab/cd/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// WebDAV 方法在 `http` 1.x 里没有常量，只能从字节构造。
fn dav(method: &str) -> Method {
    Method::from_bytes(method.as_bytes()).expect("valid method")
}

/// 真实 sccache 的三层分片 key 布局：`<hex[0..2]>/<hex[2..4]>/<hex[4..]>`。
#[tokio::test]
async fn sccache_put_get_head_roundtrip() {
    let (app, _dir) = app().await;
    let uri = format!("/sccache/{KEY}");

    // 首次 HEAD：未命中
    assert_eq!(
        call(&app, Method::HEAD, &uri, b"").await.status(),
        StatusCode::NOT_FOUND
    );

    // PUT → 204（sccache 唯一必需的写成功码）
    let put = call(&app, Method::PUT, &uri, b"cache-entry-payload").await;
    assert_eq!(put.status(), StatusCode::NO_CONTENT);

    // GET → 200 + octet-stream，且 Content-Length 与 body 严格一致
    // （sccache 用 content_length 参与签名计算，长度不符会验签失败）
    let get = call(&app, Method::GET, &uri, b"").await;
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(
        get.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/octet-stream"
    );
    let declared: usize = get
        .headers()
        .get(header::CONTENT_LENGTH)
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let body = axum::body::to_bytes(get.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(declared, body.len());
    assert_eq!(body.as_ref(), b"cache-entry-payload");

    // HEAD 命中
    assert_eq!(
        call(&app, Method::HEAD, &uri, b"").await.status(),
        StatusCode::OK
    );
}

/// 按 key 幂等：同 key 二次写入不同内容被丢弃（sccache 的 key 即内容哈希，
/// 因此这不是 bug；但把它钉住，避免有人误当 WebDAV 覆盖语义改掉）。
#[tokio::test]
async fn sccache_put_is_idempotent_by_key() {
    let (app, _dir) = app().await;
    let uri = format!("/sccache/{KEY}");
    call(&app, Method::PUT, &uri, b"first").await;
    call(&app, Method::PUT, &uri, b"second").await;
    let get = call(&app, Method::GET, &uri, b"").await;
    let body = axum::body::to_bytes(get.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"first");
}

/// sccache 启动能力探测契约：GET 必须 404（NotFound 被容忍），
/// PUT 必须 2xx（否则 sccache **静默降级为只读**，远端写入全部丢弃）。
/// 这条契约最容易被「顺手加个 key 格式校验」打破，所以显式钉住。
#[tokio::test]
async fn sccache_check_probe_contract() {
    let (app, _dir) = app().await;
    let uri = "/sccache/.sccache_check";
    assert_eq!(
        call(&app, Method::GET, uri, b"").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, Method::PUT, uri, b"Hello, World!")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    // 哨兵不落索引：反复探测不应污染 kv_entries。
    assert_eq!(
        call(&app, Method::GET, uri, b"").await.status(),
        StatusCode::NOT_FOUND
    );
}

/// opendal 写前会逐级 PROPFIND/MKCOL 父集合；任一非 2xx 会让 sccache
/// 退化为只读。响应必须是可反序列化的 207 multistatus，且
/// Content-Length 与实际 XML 字节数一致。
#[tokio::test]
async fn sccache_propfind_and_mkcol() {
    let (app, _dir) = app().await;
    for path in ["/sccache/", "/sccache/ab/", "/sccache/ab/cd/"] {
        let mkcol = call(&app, dav("MKCOL"), path, b"").await;
        assert_eq!(mkcol.status(), StatusCode::CREATED, "MKCOL {path}");

        let propfind = call(&app, dav("PROPFIND"), path, b"").await;
        assert_eq!(
            propfind.status(),
            StatusCode::MULTI_STATUS,
            "PROPFIND {path}"
        );
        let declared: usize = propfind
            .headers()
            .get(header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let body = axum::body::to_bytes(propfind.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(declared, body.len(), "PROPFIND {path} Content-Length");
        let xml = String::from_utf8_lossy(&body);
        assert!(xml.contains("<multistatus"), "{path}: {xml}");
        assert!(xml.contains("<collection/>"), "{path}: {xml}");
        // opendal 用 getlastmodified 的存在性判断解析成功。
        assert!(xml.contains("<getlastmodified>"), "{path}: {xml}");
    }
}

/// href 必须被安全回显。
///
/// 注意：axum 的 `uri.path()` 保留**百分号编码**，所以真实 HTTP 请求里
/// 路径是 `a%26b` 而不是裸 `&`——这条测试守住的是「不二次编码、不引入裸
/// XML 元字符」；`xml_escape` 本身由 routes.rs 内的单元测试直接覆盖。
#[tokio::test]
async fn sccache_propfind_echoes_href_safely() {
    let (app, _dir) = app().await;
    for path in ["/sccache/a%26b/", "/sccache/plain/", "/sccache/a%3Cb/"] {
        let propfind = call(&app, dav("PROPFIND"), path, b"").await;
        assert_eq!(propfind.status(), StatusCode::MULTI_STATUS, "{path}");
        let body = axum::body::to_bytes(propfind.into_body(), usize::MAX)
            .await
            .unwrap();
        let xml = String::from_utf8_lossy(&body);
        assert!(
            xml.contains(&format!("<href>{path}</href>")),
            "{path} href 未原样回显: {xml}"
        );
        // 裸的 XML 元字符会破坏 multistatus 解析。
        let href = xml
            .split("<href>")
            .nth(1)
            .and_then(|s| s.split("</href>").next())
            .unwrap_or_default();
        assert!(
            !href.contains('<') && !href.contains('&'),
            "{path} href 含裸元字符: {href}"
        );
    }
}

#[tokio::test]
async fn sccache_unsupported_method() {
    let (app, _dir) = app().await;
    let uri = format!("/sccache/{KEY}");
    for method in [Method::DELETE, dav("LOCK"), dav("MOVE"), dav("COPY")] {
        let expected = format!("{method} 应被拒绝");
        assert_eq!(
            call(&app, method, &uri, b"").await.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{expected}"
        );
    }
    assert_eq!(
        call(&app, Method::OPTIONS, "/sccache/", b"").await.status(),
        StatusCode::OK
    );
}

// ---------- Turborepo v8 ----------

#[tokio::test]
async fn turbo_status_reports_enabled() {
    let (app, _dir) = app().await;
    let resp = call(&app, Method::GET, "/v8/artifacts/status", b"").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "enabled");
}

/// PUT→201，GET 回显 `x-artifact-tag` 与 `x-artifact-duration`，
/// HEAD 带真实 Content-Length。签名开启时缺 tag 是**硬错误**而非 miss。
#[tokio::test]
async fn turbo_put_get_head_roundtrip() {
    let (app, _dir) = app().await;
    let uri = "/v8/artifacts/abc123?teamId=team_demo";

    let put = Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("x-artifact-tag", "c2lnbmF0dXJl")
        .header("x-artifact-duration", "1234")
        .header("x-artifact-sha", "deadbeef")
        .body(Body::from("tarball-bytes"))
        .unwrap();
    let put_resp = app.clone().oneshot(put).await.unwrap();
    assert_eq!(put_resp.status(), StatusCode::CREATED);

    let get = call(&app, Method::GET, uri, b"").await;
    assert_eq!(get.status(), StatusCode::OK);
    let tag = get
        .headers()
        .get("x-artifact-tag")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(tag, "c2lnbmF0dXJl");
    assert_eq!(
        get.headers()
            .get("x-artifact-duration")
            .unwrap()
            .to_str()
            .unwrap(),
        "1234"
    );
    assert_eq!(
        get.headers()
            .get("x-artifact-sha")
            .unwrap()
            .to_str()
            .unwrap(),
        "deadbeef"
    );
    let declared: usize = get
        .headers()
        .get(header::CONTENT_LENGTH)
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let body = axum::body::to_bytes(get.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(declared, body.len());
    assert_eq!(body.as_ref(), b"tarball-bytes");

    let head = call(&app, Method::HEAD, uri, b"").await;
    assert_eq!(head.status(), StatusCode::OK);
    let head_len: usize = head
        .headers()
        .get(header::CONTENT_LENGTH)
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        head_len,
        body.len(),
        "HEAD 的 Content-Length 必须是真实体积"
    );
}

/// 404 是 turbo **唯一**的 cache miss 信号；其他非 2xx 会让 turbo 报错
/// 而非降级。租户不匹配必须走 404。
#[tokio::test]
async fn turbo_miss_is_404_and_tenants_are_isolated() {
    let (app, _dir) = app().await;
    let put = Request::builder()
        .method(Method::PUT)
        .uri("/v8/artifacts/hash1?teamId=team_a")
        .body(Body::from("payload-a"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put).await.unwrap().status(),
        StatusCode::CREATED
    );

    assert_eq!(
        call(&app, Method::GET, "/v8/artifacts/hash1?teamId=team_a", b"")
            .await
            .status(),
        StatusCode::OK
    );
    // 同 hash 不同 team → 必须 404（miss），不是 403/500。
    assert_eq!(
        call(&app, Method::GET, "/v8/artifacts/hash1?teamId=team_b", b"")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    // slug 是与 teamId 独立的命名空间。
    assert_eq!(
        call(&app, Method::GET, "/v8/artifacts/hash1?slug=other", b"")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    // 完全未命中
    assert_eq!(
        call(&app, Method::GET, "/v8/artifacts/nope?teamId=team_a", b"")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

/// 先无签名上传、后带签名上传同一 hash：tag 必须被补齐。
/// 否则签名客户端 GET 时报 `ArtifactTagMissing`（硬错误，非 miss）。
#[tokio::test]
async fn turbo_backfills_tag_on_existing_entry() {
    let (app, _dir) = app().await;
    let uri = "/v8/artifacts/hash2?teamId=team_a";
    call(&app, Method::PUT, uri, b"body").await;
    assert!(
        call(&app, Method::GET, uri, b"")
            .await
            .headers()
            .get("x-artifact-tag")
            .is_none()
    );

    let put = Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .header("x-artifact-tag", "bGF0ZXI=")
        .body(Body::from("body"))
        .unwrap();
    app.clone().oneshot(put).await.unwrap();

    let get = call(&app, Method::GET, uri, b"").await;
    assert_eq!(
        get.headers()
            .get("x-artifact-tag")
            .unwrap()
            .to_str()
            .unwrap(),
        "bGF0ZXI="
    );
    // body 未被覆盖：tag 必须与所服务的 body 配对。
    let body = axum::body::to_bytes(get.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), b"body");
}

/// turbo 在 --preflight 下会先发 OPTIONS；405 会让预检失败。
#[tokio::test]
async fn turbo_options_preflight() {
    let (app, _dir) = app().await;
    let resp = call(&app, Method::OPTIONS, "/v8/artifacts/x?teamId=team_a", b"").await;
    assert!(resp.status().is_success());
    let allow = resp
        .headers()
        .get("access-control-allow-headers")
        .expect("preflight 需回 Access-Control-Allow-Headers")
        .to_str()
        .unwrap();
    assert!(allow.contains("Authorization"), "{allow}");
    assert!(allow.contains("x-artifact-tag"), "{allow}");
}
