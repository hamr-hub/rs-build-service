//! 缓存协议路由状态与鉴权。

use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use crate::remote::RemoteCache;

/// 缓存协议路由状态。
#[derive(Clone)]
pub struct CacheState {
    pub cache: RemoteCache,
}

impl CacheState {
    pub fn new(cache: RemoteCache) -> Self {
        Self { cache }
    }
}

/// 鉴权中间件的独立状态：只承载可选 token，与缓存实例解耦，
/// 这样鉴权层可以在 `router()` 内部就地构造（无需等 `with_state`）。
#[derive(Clone, Default)]
pub struct AuthState(pub Option<Arc<str>>);

/// Bearer 鉴权中间件。
///
/// - 未配置 token：放行（保持零配置自托管体验）。
/// - 已配置：要求 `Authorization: Bearer <token>`，常量时间比较。
///   不匹配返回 401。
///
/// 注意：sccache 与 turbo 都会把 401 当作**硬错误**而不是 cache miss，
/// 所以 token 配错的表现是「客户端构建直接失败」，而不是「命中率下降」——
/// 这比静默降级更容易发现，是有意为之。
/// 可选 Bearer token（`HOTPOT_CACHE_TOKEN`）。None = 不鉴权。
///
/// 默认不鉴权是为了单机自托管的零配置体验；一旦监听地址不是回环，
/// 缓存端点就是**构建供应链投毒面**（任何人都能 PUT 一个伪造产物，
/// 之后所有机器都会命中它）。因此对外暴露时必须设置 token。
pub async fn require_token(
    axum::extract::State(state): axum::extract::State<AuthState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.0.as_deref() else {
        return next.run(request).await;
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if presented.len() == expected.len()
        && constant_time_eq(presented.as_bytes(), expected.as_bytes())
    {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "missing or invalid bearer token",
        )
            .into_response()
    }
}

/// 常量时间字节比较：避免通过响应时间侧信道逐字节猜 token。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
