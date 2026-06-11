//! API error type and its HTTP mapping. Every error renders as
//! `{"error": "<human-readable message>"}` with a meaningful status code.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rk_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Conflict(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound { kind, key } => {
                ApiError::NotFound(format!("{kind} {key:?} not found"))
            }
            StoreError::Conflict { .. } => ApiError::Conflict(err.to_string()),
            StoreError::Serialization(_) | StoreError::Backend(_) => {
                ApiError::Internal(err.to_string())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(%status, error = %self, "request failed");
        }
        (
            status,
            Json(serde_json::json!({ "error": self.to_string() })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use rk_types::ResourceKind;

    use super::*;

    #[test]
    fn store_errors_map_to_meaningful_statuses() {
        let not_found = ApiError::from(StoreError::NotFound {
            kind: ResourceKind::Pod,
            key: "default/web".to_owned(),
        });
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);

        let conflict = ApiError::from(StoreError::Conflict {
            key: "default/web".to_owned(),
            expected: 3,
            actual: 5,
        });
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        let backend = ApiError::from(StoreError::Backend("disk on fire".to_owned()));
        assert_eq!(backend.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
