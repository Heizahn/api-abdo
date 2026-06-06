use std::sync::Arc;

use axum::{
    extract::{Extension, Multipart, State},
    Json,
};
use mongodb::bson::oid::ObjectId;
use sha2::{Digest, Sha256};

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::WhatsAppRepository,
    error::ApiError,
    models::whatsapp::{MediaLimitsResponse, MediaTypeLimit, MediaUploadData, MediaUploadResponse},
    modules::whatsapp::shared::authz::require_can_chat,
    modules::whatsapp::shared::resolve_service_for_phone,
    state::AppState,
};

// Mime types aceptados por Meta Cloud API para cada tipo de upload.
// Referencia: https://developers.facebook.com/docs/whatsapp/cloud-api/reference/media
pub(crate) const MIME_IMAGE: &[&str] = &["image/jpeg", "image/png"];
pub(crate) const MIME_VIDEO: &[&str] = &["video/mp4", "video/3gpp"];
pub(crate) const MIME_AUDIO: &[&str] = &[
    "audio/aac",
    "audio/mp4",
    "audio/mpeg",
    "audio/amr",
    "audio/ogg",
];
pub(crate) const MIME_DOCUMENT: &[&str] = &[
    "application/pdf",
    "application/vnd.ms-powerpoint",
    "application/msword",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "text/plain",
];
pub(crate) const MIME_STICKER: &[&str] = &["image/webp"];

// Tamaños máximos por tipo — son los límites oficiales de Meta Cloud API
// (protocolo, iguales para todas las cuentas). Hardcoded aquí porque no hay
// caso de uso real para tunearlos por deploy/workspace.
// Sticker a 500 KB cubre tanto estáticos (Meta: 100 KB) como animados (500 KB);
// Meta rechaza server-side si el static supera su sub-límite interno.
pub(crate) const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
pub(crate) const MAX_VIDEO_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_AUDIO_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_DOCUMENT_BYTES: u64 = 100 * 1024 * 1024;
pub(crate) const MAX_STICKER_BYTES: u64 = 500 * 1024;

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/media",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body(
        content = String,
        content_type = "multipart/form-data",
        description = "Campos: `file` (binario), `type` (image|video|document|audio|sticker), `conversation_id` (ObjectId hex)",
    ),
    responses(
        (status = 200, description = "Media subido", body = MediaUploadResponse),
        (status = 400, description = "Falta un campo o es inválido"),
        (status = 403, description = "El usuario no tiene acceso al módulo de chat"),
        (status = 404, description = "Conversación no encontrada"),
        (status = 422, description = "Validación falló: campo requerido vacío, tamaño excedido, o MIME no soportado"),
    )
)]
pub async fn upload_media_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    mut multipart: Multipart,
) -> Result<Json<MediaUploadResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_mime: Option<String> = None;
    let mut file_name: Option<String> = None;
    let mut type_str: Option<String> = None;
    let mut conv_id_str: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        tracing::error!("[upload_media] multipart error: {}", e);
        ApiError::BadRequest("error leyendo multipart".into())
    })? {
        match field.name().unwrap_or("") {
            "file" => {
                file_mime = field.content_type().map(|s| s.to_string());
                file_name = field.file_name().map(|s| s.to_string());
                let data = field
                    .bytes()
                    .await
                    .map_err(|_| ApiError::BadRequest("error leyendo file".into()))?;
                file_bytes = Some(data.to_vec());
            }
            "type" => {
                type_str = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| ApiError::BadRequest("type inválido".into()))?
                        .trim()
                        .to_lowercase(),
                );
            }
            "conversation_id" => {
                conv_id_str = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| ApiError::BadRequest("conversation_id inválido".into()))?
                        .trim()
                        .to_string(),
                );
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let bytes = file_bytes.ok_or_else(|| ApiError::ValidationError {
        code: "missing_field".into(),
        field: "file".into(),
        message: "Adjuntá el archivo para subir.".into(),
    })?;
    if bytes.is_empty() {
        return Err(ApiError::ValidationError {
            code: "file_empty".into(),
            field: "file".into(),
            message: "El archivo está vacío.".into(),
        });
    }

    let conv_id_str = conv_id_str.ok_or_else(|| ApiError::ValidationError {
        code: "missing_field".into(),
        field: "conversation_id".into(),
        message: "Falta identificar la conversación.".into(),
    })?;

    // Si el front no mandó `type`, lo inferimos del Content-Type del file part.
    // Útil para clientes simples que no quieren replicar la taxonomía de Meta.
    let mime_lower = file_mime
        .as_deref()
        .unwrap_or("application/octet-stream")
        .to_lowercase();
    let type_str = match type_str {
        Some(t) => t,
        None => infer_type_from_mime(&mime_lower).ok_or_else(|| ApiError::ValidationError {
            code: "unrecognized_mime".into(),
            field: "type".into(),
            message: format!(
                "No reconocemos el tipo de archivo (`{}`). Revisá la extensión o adjuntalo con otro formato.",
                mime_lower
            ),
        })?
        .to_string(),
    };

    let (max_bytes, allowed_mimes) =
        media_type_limits(&type_str).ok_or_else(|| ApiError::ValidationError {
            code: "invalid_media_type".into(),
            field: "type".into(),
            message: "El tipo debe ser image, video, document, audio o sticker.".into(),
        })?;

    if (bytes.len() as u64) > max_bytes {
        let (label, _) = media_type_label(&type_str);
        return Err(ApiError::ValidationError {
            code: "media_too_large".into(),
            field: "file".into(),
            message: format!(
                "El {} supera el límite de {} (recibido {}). Comprimilo o usá uno más liviano.",
                label,
                human_bytes(max_bytes),
                human_bytes(bytes.len() as u64)
            ),
        });
    }

    if !allowed_mimes.iter().any(|m| *m == mime_lower) {
        let (label, formats) = media_type_label(&type_str);
        return Err(ApiError::ValidationError {
            code: "mime_not_allowed".into(),
            field: "file".into(),
            message: format!(
                "Ese formato no se puede enviar como {}. Formatos aceptados: {}.",
                label, formats
            ),
        });
    }

    // Resolver conversación/número para decidir contra qué `WaSettings`
    // subimos el binario a Meta.
    let conv_oid = ObjectId::parse_str(&conv_id_str)
        .map_err(|_| ApiError::BadRequest("conversation_id inválido".into()))?;
    let conv = state
        .db
        .find_conversation_by_id(&conv_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // SHA-256 del binario — el front lo usa para deduplicar reenvíos idénticos.
    let sha256_hex = {
        let mut h = Sha256::new();
        h.update(&bytes);
        let out = h.finalize();
        out.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    };

    let size = bytes.len() as u64;

    // Subir a Meta (sin relay — el relay sólo aplica a descargas desde lookaside).
    let wa = resolve_service_for_phone(&state, &conv.business_phone).await?;
    let media_id = wa
        .upload_media(bytes, &mime_lower, file_name.as_deref())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!(
        "[upload_media] OK media_id={} size={}B type={} mime={} conv={}",
        media_id,
        size,
        type_str,
        mime_lower,
        conv_id_str
    );

    Ok(Json(MediaUploadResponse {
        ok: true,
        data: MediaUploadData {
            media_id,
            mime_type: mime_lower,
            size,
            sha256: sha256_hex,
        },
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/media/limits",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Límites vigentes", body = MediaLimitsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_media_limits_handler() -> Json<MediaLimitsResponse> {
    let as_vec = |slice: &[&str]| slice.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    Json(MediaLimitsResponse {
        ok: true,
        image: MediaTypeLimit {
            max_bytes: MAX_IMAGE_BYTES,
            mime_types: as_vec(MIME_IMAGE),
        },
        video: MediaTypeLimit {
            max_bytes: MAX_VIDEO_BYTES,
            mime_types: as_vec(MIME_VIDEO),
        },
        audio: MediaTypeLimit {
            max_bytes: MAX_AUDIO_BYTES,
            mime_types: as_vec(MIME_AUDIO),
        },
        document: MediaTypeLimit {
            max_bytes: MAX_DOCUMENT_BYTES,
            mime_types: as_vec(MIME_DOCUMENT),
        },
        sticker: MediaTypeLimit {
            max_bytes: MAX_STICKER_BYTES,
            mime_types: as_vec(MIME_STICKER),
        },
    })
}

/// Convierte bytes a texto human-readable ("16 MB", "100 KB", "800 B").
/// Usa 1 decimal solo si aporta (no muestra "16.0 MB").
pub(crate) fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if n >= MB {
        let mb = n as f64 / MB as f64;
        if (mb.fract() * 10.0).round() == 0.0 {
            format!("{:.0} MB", mb)
        } else {
            format!("{:.1} MB", mb)
        }
    } else if n >= KB {
        format!("{} KB", (n as f64 / KB as f64).round() as u64)
    } else {
        format!("{} B", n)
    }
}

/// Label en español + lista de extensiones aceptadas para un tipo de media.
/// Usado al formatear mensajes de error user-facing.
fn media_type_label(type_str: &str) -> (&'static str, &'static str) {
    match type_str {
        "image" => ("imagen", "jpeg, png"),
        "video" => ("video", "mp4, 3gp"),
        "audio" => ("audio", "aac, amr, mp3, m4a, ogg"),
        "document" => ("documento", "pdf, doc(x), ppt(x), xls(x), txt"),
        "sticker" => ("sticker", "webp"),
        _ => ("archivo", ""),
    }
}

/// Resuelve `(max_bytes, mime_allowlist)` para un string de tipo.
fn media_type_limits(type_str: &str) -> Option<(u64, &'static [&'static str])> {
    match type_str {
        "image" => Some((MAX_IMAGE_BYTES, MIME_IMAGE)),
        "video" => Some((MAX_VIDEO_BYTES, MIME_VIDEO)),
        "audio" => Some((MAX_AUDIO_BYTES, MIME_AUDIO)),
        "document" => Some((MAX_DOCUMENT_BYTES, MIME_DOCUMENT)),
        "sticker" => Some((MAX_STICKER_BYTES, MIME_STICKER)),
        _ => None,
    }
}

/// Deriva el `type` de Meta a partir del `Content-Type` del file part.
/// Usado cuando el front sube sin mandar el campo `type` explícito.
/// `image/webp` → `sticker` (Meta sólo acepta webp en stickers, no en image).
/// `application/octet-stream` o mimes raros → `document` (catch-all).
fn infer_type_from_mime(mime: &str) -> Option<&'static str> {
    if mime == "image/webp" {
        return Some("sticker");
    }
    if mime.starts_with("image/") {
        return Some("image");
    }
    if mime.starts_with("video/") {
        return Some("video");
    }
    if mime.starts_with("audio/") {
        return Some("audio");
    }
    // PDFs, Word/Excel/PowerPoint, text/plain — todo cae en document.
    if MIME_DOCUMENT.iter().any(|m| *m == mime) {
        return Some("document");
    }
    None
}
