use axum::{
    http::StatusCode,
    response::{IntoResponse, Response, Json},
};
use serde_json::json;

/// Errores personalizados de la API
/// Mantiene compatibilidad con formato JSON: {"ok": false, "error": "..."}
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("Not found")]
    NotFound,

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden")]
    Forbidden,

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Redis error: {0}")]
    CacheError(String),

    #[error("SMS sending error: {0}")]
    SmsError(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::DatabaseError(_) => (StatusCode::INTERNAL_SERVER_ERROR, "database_error"),
            ApiError::CacheError(_) => (StatusCode::INTERNAL_SERVER_ERROR, "cache_error"),
            ApiError::SmsError(_) => (StatusCode::INTERNAL_SERVER_ERROR, "sms_error"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };

        // Log error details for debugging
        tracing::error!("API Error: {:?}", self);

        // ✅ MANTIENE EL FORMATO JSON ACTUAL
        let body = Json(json!({
            "ok": false,
            "error": error_message
        }));

        (status, body).into_response()
    }
}

/// Conversión de errores MongoDB
impl From<mongodb::error::Error> for ApiError {
    fn from(err: mongodb::error::Error) -> Self {
        ApiError::DatabaseError(err.to_string())
    }
}

/// Conversión de errores Redis
impl From<redis::RedisError> for ApiError {
    fn from(err: redis::RedisError) -> Self {
        ApiError::CacheError(err.to_string())
    }
}

/// Conversión de errores genéricos
impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err.to_string())
    }
}
