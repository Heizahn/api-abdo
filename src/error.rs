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

    /// La conversación no se puede tomar porque su `status` actual no es
    /// `pending` ni `closed` (típicamente ya está `in_progress` de otro agente).
    /// Se sirve como 409 `conversacion_no_tomable`.
    #[error("Conversation not takeable")]
    ConversationNotTakeable,

    /// La conversación está `closed` y el envío no es de tipo template. Los
    /// mensajes freeform/interactive requieren una plantilla para reabrir el
    /// chat (escape hatch de la ventana 24h de Meta). Se sirve como 409
    /// `conversacion_cerrada_requiere_plantilla`.
    #[error("Closed conversation requires template")]
    ClosedRequiresTemplate,

    /// Falló validación de un payload.
    /// - `code`: identificador estable para que el front mapee a UI/i18n
    ///   sin parsear el mensaje (ej: `media_too_large`, `missing_field`).
    /// - `field`: nombre del campo culpable (o `"file"`, `"interactive"`, etc.).
    /// - `message`: texto human-readable en español listo para mostrar al usuario.
    ///
    /// Se sirve como 422 con
    /// `{ ok:false, error:"validation_error", code, field, message }`.
    #[error("Validation error [{code}] on {field}: {message}")]
    ValidationError { code: String, field: String, message: String },

    /// Error de dominio con código estable + status HTTP arbitrario + payload
    /// estructurado opcional. Pensado para errores que necesitan más contexto que
    /// `ValidationError` (ej: lista de propósitos que bloquean un delete, error
    /// upstream de Meta con código y mensaje, rate limit con retry_after).
    ///
    /// Se sirve como el `status` indicado con shape:
    /// `{ ok: false, error: "<code>", code, field?, message, details? }`.
    #[error("Domain error [{code}]: {message}")]
    Domain {
        status: StatusCode,
        code: String,
        field: Option<String>,
        message: String,
        details: Option<serde_json::Value>,
    },

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
        if let ApiError::ValidationError { code, field, message } = &self {
            tracing::error!("API Error: {:?}", self);
            let body = Json(json!({
                "ok": false,
                "error": "validation_error",
                "code": code,
                "field": field,
                "message": message,
            }));
            return (StatusCode::UNPROCESSABLE_ENTITY, body).into_response();
        }

        // Domain lleva status arbitrario + payload estructurado opcional.
        if let ApiError::Domain { status, code, field, message, details } = self {
            tracing::error!("API Error: Domain code={} field={:?} message={}", code, field, message);
            let mut body = serde_json::json!({
                "ok": false,
                "error": code,
                "code": code,
                "message": message,
            });
            if let Some(f) = field { body["field"] = serde_json::Value::String(f); }
            if let Some(d) = details { body["details"] = d; }
            return (status, Json(body)).into_response();
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
            ApiError::ConversationNotTakeable => (StatusCode::CONFLICT, "conversacion_no_tomable"),
            ApiError::ClosedRequiresTemplate => (StatusCode::CONFLICT, "conversacion_cerrada_requiere_plantilla"),
            ApiError::MissingTemplateParams => (StatusCode::BAD_REQUEST, "missing_template_params"),
            ApiError::ValidationError { .. } => unreachable!(),
            ApiError::Domain { .. } => unreachable!(),
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

impl ApiError {
    pub fn domain_simple(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError::Domain {
            status,
            code: code.into(),
            field: None,
            message: message.into(),
            details: None,
        }
    }

    pub fn domain_with_field(
        status: StatusCode,
        code: impl Into<String>,
        field: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        ApiError::Domain {
            status,
            code: code.into(),
            field: Some(field.into()),
            message: message.into(),
            details: None,
        }
    }

    pub fn domain_with_details(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        ApiError::Domain {
            status,
            code: code.into(),
            field: None,
            message: message.into(),
            details: Some(details),
        }
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
