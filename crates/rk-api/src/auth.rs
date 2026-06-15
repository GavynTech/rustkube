//! Bearer-token authentication. When the API server is started with a token,
//! every request except `/healthz` (kept open for liveness checks and load
//! balancers) must carry `Authorization: Bearer <token>`. The check is a
//! constant-time comparison so a wrong token leaks no timing signal.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use serde_json::json;

/// axum middleware: reject anything without the configured bearer token.
/// Installed only when a token is configured (see `rk-api` `main`).
pub async fn require_bearer(
    State(expected): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    match presented {
        Some(token) if tokens_match(token.as_bytes(), expected.as_bytes()) => {
            next.run(request).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized: a valid bearer token is required" })),
        )
            .into_response(),
    }
}

/// Length-checked, constant-time byte comparison.
fn tokens_match(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt as _;

    use super::*;

    fn guarded_app() -> Router {
        Router::new()
            .route("/api/v1/pods", get(|| async { "pods" }))
            .route("/healthz", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                Arc::<str>::from("s3cr3t"),
                require_bearer,
            ))
    }

    async fn status(req: Request<Body>) -> StatusCode {
        guarded_app().oneshot(req).await.unwrap().status()
    }

    fn bearer(token: &str) -> Request<Body> {
        Request::builder()
            .uri("/api/v1/pods")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn correct_token_is_allowed() {
        assert_eq!(status(bearer("s3cr3t")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_or_missing_token_is_401() {
        assert_eq!(status(bearer("nope")).await, StatusCode::UNAUTHORIZED);
        let bare = Request::builder()
            .uri("/api/v1/pods")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(bare).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn healthz_stays_open() {
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(req).await, StatusCode::OK);
    }

    #[test]
    fn tokens_match_is_length_and_content_sensitive() {
        assert!(tokens_match(b"abc", b"abc"));
        assert!(!tokens_match(b"abc", b"abd"));
        assert!(!tokens_match(b"abc", b"abcd"));
    }
}
