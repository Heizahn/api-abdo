use std::sync::Arc;

use axum::{
    extract::{Extension, Multipart, State},
    http::StatusCode,
    Json,
};
use sha2::{Digest, Sha256};

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{StoreTemplateMediaInput, WaTemplateMediaRepository, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::{HeaderMediaUploadData, HeaderMediaUploadResponse},
    modules::whatsapp::shared::authz::require_can_chat,
    state::AppState,
};

/// Límites de mime + tamaño por `format` impuestos por Meta para headers de
/// template. Cualquier cosa fuera de esto rebota client-side antes de llegar
/// a la Resumable Upload API.
fn header_media_limits(format: &str) -> Option<(&'static [&'static str], u64)> {
    match format.to_uppercase().as_str() {
        "IMAGE" => Some((&["image/jpeg", "image/png"], 5 * 1024 * 1024)),
        "VIDEO" => Some((&["video/mp4", "video/3gpp"], 16 * 1024 * 1024)),
        "DOCUMENT" => Some((&["application/pdf"], 100 * 1024 * 1024)),
        _ => None,
    }
}

/// SHA-256 en hex minúsculas. Usado para dedup (`wa_template_media.files` tiene
/// índice único por `(metadata.phone_number_id, metadata.sha256)`).
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

/// `POST /v1/auth-user/whatsapp/templates/header-media` — multipart upload.
/// Persiste el binario en GridFS con dedup por SHA-256. El front usa el
/// `media_id` devuelto como `example.header_handle[0]` al crear/editar un
/// template; el swap real a handle Meta ocurre en `create_template_handler` /
/// `update_template_handler` cuando llaman a la Resumable Upload API.
#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/templates/header-media",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    request_body(
        content = String,
        content_type = "multipart/form-data",
        description = "Campos: `file` (binario), `phone_number_id` (string), `format` (IMAGE|VIDEO|DOCUMENT)",
    ),
    responses(
        (status = 200, description = "Media persistida en GridFS", body = HeaderMediaUploadResponse),
        (status = 400, description = "invalid_file_type | invalid_format | file_required | file_empty"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "phone_number_not_found"),
        (status = 413, description = "file_too_large"),
        (status = 503, description = "app_id_not_configured"),
    )
)]
pub async fn upload_template_header_media_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    mut multipart: Multipart,
) -> Result<Json<HeaderMediaUploadResponse>, ApiError> {
    let uploader = require_can_chat(&state, &claims.id).await?;

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_mime: Option<String> = None;
    let mut phone_number_id: Option<String> = None;
    let mut format: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        tracing::error!("[upload_template_header_media] multipart error: {}", e);
        ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "invalid_multipart",
            "Error leyendo el multipart",
        )
    })? {
        match field.name().unwrap_or("") {
            "file" => {
                file_mime = field.content_type().map(|s| s.to_string());
                let data = field.bytes().await.map_err(|_| {
                    ApiError::domain_with_field(
                        StatusCode::BAD_REQUEST,
                        "file_required",
                        "file",
                        "No se pudo leer el archivo adjunto",
                    )
                })?;
                file_bytes = Some(data.to_vec());
            }
            "phone_number_id" => {
                phone_number_id = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| {
                            ApiError::domain_with_field(
                                StatusCode::BAD_REQUEST,
                                "invalid_field",
                                "phone_number_id",
                                "phone_number_id inválido",
                            )
                        })?
                        .trim()
                        .to_string(),
                );
            }
            "format" => {
                format = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| {
                            ApiError::domain_with_field(
                                StatusCode::BAD_REQUEST,
                                "invalid_field",
                                "format",
                                "format inválido",
                            )
                        })?
                        .trim()
                        .to_uppercase(),
                );
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let bytes = file_bytes.ok_or_else(|| {
        ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "file_required",
            "file",
            "Adjuntá el archivo a subir",
        )
    })?;
    if bytes.is_empty() {
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "file_empty",
            "file",
            "El archivo está vacío",
        ));
    }
    let phone_number_id = phone_number_id.ok_or_else(|| {
        ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "missing_field",
            "phone_number_id",
            "phone_number_id es requerido",
        )
    })?;
    let format = format.ok_or_else(|| {
        ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "missing_field",
            "format",
            "format es requerido",
        )
    })?;

    let (allowed_mimes, max_size) = header_media_limits(&format).ok_or_else(|| {
        ApiError::domain_with_details(
            StatusCode::BAD_REQUEST,
            "invalid_format",
            "Formato no soportado. Usa IMAGE, VIDEO o DOCUMENT",
            serde_json::json!({ "field": "format", "received": format }),
        )
    })?;

    let mime = file_mime
        .as_deref()
        .unwrap_or("application/octet-stream")
        .to_lowercase();
    if !allowed_mimes.iter().any(|m| *m == mime.as_str()) {
        return Err(ApiError::domain_with_details(
            StatusCode::BAD_REQUEST,
            "invalid_file_type",
            "Tipo MIME no permitido para este formato",
            serde_json::json!({ "allowed_mime_types": allowed_mimes, "received": mime }),
        ));
    }
    let size = bytes.len() as u64;
    if size > max_size {
        return Err(ApiError::domain_with_details(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            "El archivo supera el tamaño máximo permitido",
            serde_json::json!({ "max_size": max_size, "actual_size": size }),
        ));
    }

    let _settings = state
        .db
        .find_wa_settings_by_phone_number_id(&phone_number_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_with_field(
                StatusCode::NOT_FOUND,
                "phone_number_not_found",
                "phone_number_id",
                "El número de WhatsApp no está configurado",
            )
        })?;

    let sha = sha256_hex(&bytes);
    let stored = state
        .db
        .store_template_media(StoreTemplateMediaInput {
            phone_number_id: &phone_number_id,
            format: &format,
            mime_type: &mime,
            sha256: &sha,
            bytes: &bytes,
            uploaded_by: &claims.id,
            uploaded_by_name: &uploader.name,
        })
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(HeaderMediaUploadResponse {
        ok: true,
        data: HeaderMediaUploadData {
            media_id: stored.id.to_hex(),
            mime_type: stored.mime_type,
            file_size: stored.file_size,
            sha256: stored.sha256,
        },
    }))
}
