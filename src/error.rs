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

    #[allow(dead_code)]
    #[error("Bad request: {0}")]
    BadRequest(String),

    #[allow(dead_code)]
    #[error("Conflict: {0}")]
    Conflict(String),

    /// La password actual provista (`old_password`) no coincide con el hash
    /// almacenado. Se sirve como 401 `wrong_password` — diferenciable del 401
    /// genérico (`unauthorized`) para que el front pueda mostrar un mensaje
    /// específico en el flujo de cambio de contraseña.
    #[error("Wrong password")]
    WrongPassword,

    /// La nueva password es idéntica a la actual. Se sirve como 400
    /// `same_password`. Evita "cambios" sin cambio real.
    #[error("Same password")]
    SamePassword,

    /// La nueva password no cumple la policy mínima (longitud, etc.). Se
    /// sirve como 400 `weak_password`.
    #[error("Weak password")]
    WeakPassword,

    /// Ventana de 24h expirada: no se puede enviar freeform, usar template.
    #[error("Window expired: use template")]
    WindowExpired,

    /// Faltan parámetros en el componente BODY del template enviado.
    #[error("Missing template params")]
    MissingTemplateParams,

    /// La ventana de 24h del chat ya expiró (alias de sentido para casos donde
    /// el mensaje sería freeform). Se sirve como 409 `window_closed`.
    #[error("Window closed: use template")]
    WindowClosed,

    /// Falló validación de un payload. `field` es el nombre del campo que
    /// falló; `message` es texto orientativo para el front. Se sirve como
    /// 422 Unprocessable Entity con `{ ok:false, error:"validation_error", field, message }`.
    #[error("Validation error on {field}: {message}")]
    ValidationError { field: String, message: String },

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Redis error: {0}")]
    CacheError(String),

    #[allow(dead_code)]
    #[error("SMS sending error: {0}")]
    SmsError(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Error interno del servidor")]
    InternalServerError
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // ValidationError lleva payload extra (field + message), lo manejamos aparte.
        if let ApiError::ValidationError { field, message } = &self {
            tracing::error!("API Error: {:?}", self);
            let body = Json(json!({
                "ok": false,
                "error": "validation_error",
                "field": field,
                "message": message,
            }));
            return (StatusCode::UNPROCESSABLE_ENTITY, body).into_response();
        }

        let (status, error_message) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::WrongPassword => (StatusCode::UNAUTHORIZED, "wrong_password"),
            ApiError::SamePassword => (StatusCode::BAD_REQUEST, "same_password"),
            ApiError::WeakPassword => (StatusCode::BAD_REQUEST, "weak_password"),
            ApiError::WindowExpired => (StatusCode::CONFLICT, "window_expired"),
            ApiError::WindowClosed => (StatusCode::CONFLICT, "window_closed"),
            ApiError::MissingTemplateParams => (StatusCode::BAD_REQUEST, "missing_template_params"),
            ApiError::ValidationError { .. } => unreachable!(),
            ApiError::DatabaseError(_) => (StatusCode::INTERNAL_SERVER_ERROR, "database_error"),
            ApiError::CacheError(_) => (StatusCode::INTERNAL_SERVER_ERROR, "cache_error"),
            ApiError::SmsError(_) => (StatusCode::INTERNAL_SERVER_ERROR, "sms_error"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            ApiError::InternalServerError => (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
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
