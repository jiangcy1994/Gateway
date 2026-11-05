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

    #[error("model not found: '{0}' is not declared by any provider; register it under a provider's models at /__admin/llm/providers")]
    ModelNotFound(String),

    #[error("no enabled provider serves model '{0}' for this protocol: declared providers are disabled or do not speak this protocol")]
    NoProviderForModel(String),

    #[error("model '{0}' is not allowed for app '{1}'")]
    ModelNotAllowedForApp(String, String),

    #[error("unauthorized: the presented credential does not match any app key")]
    UnknownAppKey,

    #[error("app '{0}' is disabled")]
    AppDisabled(String),

    #[error("invalid app: {0}")]
    InvalidApp(String),

    #[error("app id already exists: {0}")]
    DuplicateAppId(String),

    #[error("app key already exists (for app '{0}')")]
    DuplicateAppKey(String),

    #[error("app not found: {0}")]
    AppNotFound(String),

    #[error("request body too large: the LLM gateway buffers bodies up to 16 MiB to inspect the model field")]
    RequestBodyTooLarge,

    #[error("bot not found: {0}")]
    BotNotFound(String),

    #[error("invalid bot: {0}")]
    InvalidBot(String),

    #[error("bot id already exists: {0}")]
    DuplicateBotId(String),

    #[error("bot app_id already exists: {0}")]
    DuplicateBotAppId(String),

    #[error("bot {0} is disabled")]
    BotDisabled(String),

    #[error("failed to fetch access token: {0}")]
    TokenFetchFailed(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    /// 该错误的 house 状态码（`IntoResponse` 与转发路径共用，避免两处各写一张表）。
    pub fn status(&self) -> StatusCode {
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
            ApiError::ModelNotFound(_) => StatusCode::NOT_FOUND,
            ApiError::NoProviderForModel(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::ModelNotAllowedForApp(_, _) => StatusCode::FORBIDDEN,
            ApiError::UnknownAppKey => StatusCode::UNAUTHORIZED,
            ApiError::AppDisabled(_) => StatusCode::FORBIDDEN,
            ApiError::InvalidApp(_)
            | ApiError::DuplicateAppId(_)
            | ApiError::DuplicateAppKey(_) => StatusCode::BAD_REQUEST,
            ApiError::AppNotFound(_) => StatusCode::NOT_FOUND,
            ApiError::RequestBodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::BotNotFound(_) => StatusCode::NOT_FOUND,
            ApiError::InvalidBot(_)
            | ApiError::DuplicateBotId(_)
            | ApiError::DuplicateBotAppId(_) => StatusCode::BAD_REQUEST,
            ApiError::BotDisabled(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::TokenFetchFailed(_) => StatusCode::BAD_GATEWAY,
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
