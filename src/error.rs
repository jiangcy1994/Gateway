use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("route not found: {0}")]
    RouteNotFound(String),

    #[error("invalid route: {0}")]
    InvalidRoute(String),

    #[error("route id already exists: {0}")]
    DuplicateId(String),

    #[error("route prefix already mounted: {0}")]
    DuplicatePrefix(String),

    #[error("unauthorized: a valid X-Admin-Key is required")]
    Unauthorized,

    #[error("routes file could not be read: {0}")]
    ConfigLoad(String),

    #[error("routes file could not be written: {0}")]
    Persist(String),

    #[error("provider not found: {0}")]
    ProviderNotFound(String),

    #[error("invalid provider: {0}")]
    InvalidProvider(String),

    #[error("provider id already exists: {0}")]
    DuplicateProviderId(String),

    #[error("no enabled provider for protocol '{0}'")]
    NoProviderForProtocol(String),

    #[error("quota exhausted for protocol '{0}': all providers are at their rpm or tpm limit")]
    QuotaExhausted(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::RouteNotFound(_) => StatusCode::NOT_FOUND,
            ApiError::InvalidRoute(_) | ApiError::DuplicateId(_) | ApiError::DuplicatePrefix(_) => {
                StatusCode::BAD_REQUEST
            }
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::ConfigLoad(_) | ApiError::Persist(_) | ApiError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            ApiError::ProviderNotFound(_) => StatusCode::NOT_FOUND,
            ApiError::InvalidProvider(_) | ApiError::DuplicateProviderId(_) => {
                StatusCode::BAD_REQUEST
            }
            ApiError::NoProviderForProtocol(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::QuotaExhausted(_) => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        (
            status,
            Json(json!({
                "error": { "code": status.as_u16(), "message": self.to_string() }
            })),
        )
            .into_response()
    }
}
