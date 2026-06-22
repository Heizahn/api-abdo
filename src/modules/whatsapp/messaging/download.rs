use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension,
};

use crate::{
    auth::user_jwt::UserProfileClaims,
    cache::MEDIA_CACHE_MAX_BYTES,
    crypto::aes::decrypt_payload,
    db::WhatsAppRepository,
    error::ApiError,
    modules::whatsapp::{
        service::{MediaInfo, WhatsAppService},
        shared::authz::require_can_chat,
        shared::{apply_media_relay, resolve_service_for_phone, settings_secret},
    },
    state::AppState,
};

/// Reintentos para media download (info+body) cuando Meta/CDN falla de forma
/// transitoria. Backoff corto para no congelar la UI, pero suficiente para
/// absorber intermitencias de red o 5xx puntuales.
const MEDIA_DOWNLOAD_RETRY_DELAYS_MS: &[u64] = &[0, 700, 2_000];

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/media/{media_id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("media_id" = String, Path, description = "ID del media reportado por Meta en el webhook")),
    responses(
        (status = 200, description = "Binario del media con el Content-Type correcto",
            content_type = "application/octet-stream"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Agente no asignado al número de negocio"),
        (status = 404, description = "Media no encontrado"),
    )
)]
pub async fn get_media_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(media_id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    // 1. Mensaje que contiene el media.
    let msg = state
        .db
        .find_message_by_media_id(&media_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 2. Conversación → business_phone.
    let conv = state
        .db
        .find_conversation_by_id(&msg.conversation_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 3. Gate de acceso al módulo: SUPERADMIN siempre, o cualquier usuario
    // con `bCanChat == true`. No exigimos pertenecer a `WaSettings.agents`
    // para descargar media; eso era demasiado restrictivo para supervisión y
    // operación normal del panel.
    require_can_chat(&state, &claims.id).await?;

    // 4. Settings del negocio (credenciales del número).
    let settings = state
        .db
        .find_wa_settings_by_phone(&conv.business_phone)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "wa_settings inactivo o no encontrado para {}",
                conv.business_phone
            ))
        })?;

    // 5. Hot path: cache de Redis. Los media_id son inmutables, así que el
    // primero que haya abierto el media (o el prefetch del webhook) ya lo dejó.
    let t0 = std::time::Instant::now();
    if let Some((bytes, mime, remote_filename)) = state.redis.get_media_cache(&media_id).await {
        tracing::debug!(
            "[media] HIT {} ({} bytes, {}) redis={}ms",
            media_id,
            bytes.len(),
            mime,
            t0.elapsed().as_millis()
        );
        let filename = msg
            .media_filename
            .clone()
            .or(remote_filename)
            .unwrap_or_else(|| media_id.clone());
        return Ok(build_media_response(bytes, &mime, &filename));
    }

    // 5.5. Miss + prefetch posiblemente en vuelo: si el lock ya está tomado,
    // hay otra tarea bajándolo. Esperamos ~2s en polls de 100ms a ver si
    // aparece en cache antes de disparar una segunda descarga al Worker.
    if !state.redis.try_lock_media_prefetch(&media_id).await {
        tracing::debug!(
            "[media] MISS→WAIT {} — prefetch en vuelo, esperando",
            media_id
        );
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Some((bytes, mime, remote_filename)) =
                state.redis.get_media_cache(&media_id).await
            {
                tracing::debug!(
                    "[media] WAIT→HIT {} ({} bytes, {}) wait={}ms",
                    media_id,
                    bytes.len(),
                    mime,
                    t0.elapsed().as_millis()
                );
                let filename = msg
                    .media_filename
                    .clone()
                    .or(remote_filename)
                    .unwrap_or_else(|| media_id.clone());
                return Ok(build_media_response(bytes, &mime, &filename));
            }
        }
        // El otro task tardó demasiado o falló — seguimos con descarga propia.
        tracing::warn!(
            "[media] MISS→WAIT timeout para {} — bajando por nuestra cuenta",
            media_id
        );
    } else {
        tracing::warn!(
            "[media] MISS {} — cayendo a Meta (prefetch no completó a tiempo o falló)",
            media_id
        );
    }
    // Guard: al salir del handler liberamos el lock. Si la descarga falla,
    // otro request puede reintentar inmediatamente en vez de esperar el TTL.
    let _lock_guard = MediaPrefetchGuard {
        redis: state.redis.clone(),
        media_id: media_id.clone(),
    };

    // 6. Cache miss → descargar de Meta.
    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        return Err(ApiError::Internal(
            "wa_settings sin phone_number_id o access_token configurados".into(),
        ));
    }
    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
    let wa = apply_media_relay(
        &state,
        WhatsAppService::new(
            state.reqwest_client.clone(),
            settings.phone_number_id,
            token,
        ),
    );

    let t_meta = std::time::Instant::now();
    let (bytes, mime, remote_filename) =
        download_media_with_retry(&wa, &media_id)
            .await
            .map_err(|e| {
                tracing::error!(
                    "[media] download_media falló para {} tras {}ms: {}",
                    media_id,
                    t_meta.elapsed().as_millis(),
                    e
                );
                media_download_api_error(&e)
            })?;
    tracing::debug!(
        "[media] MISS→FETCH {} ({} bytes, {}) meta={}ms",
        media_id,
        bytes.len(),
        mime,
        t_meta.elapsed().as_millis()
    );

    // Guardar en cache fire-and-forget para la próxima request (y para los
    // demás agentes que abran el mismo chat).
    {
        let state_cl = state.clone();
        let mid_cl = media_id.clone();
        let bytes_cl = bytes.clone();
        let mime_cl = mime.clone();
        let filename_cl = remote_filename.clone();
        tokio::spawn(async move {
            state_cl
                .redis
                .set_media_cache(&mid_cl, &bytes_cl, &mime_cl, filename_cl.as_deref())
                .await;
        });
    }

    let filename = msg
        .media_filename
        .clone()
        .or(remote_filename)
        .unwrap_or_else(|| media_id.clone());
    Ok(build_media_response(bytes, &mime, &filename))
}

fn media_download_api_error(err: &str) -> ApiError {
    if is_meta_media_unavailable(err) {
        return ApiError::domain_simple(
            StatusCode::NOT_FOUND,
            "media_unavailable",
            "El archivo ya no está disponible en WhatsApp o faltan permisos para descargarlo.",
        );
    }
    ApiError::Internal(err.to_string())
}

fn is_meta_media_unavailable(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("graphmethodexception")
        && e.contains("code\":100")
        && e.contains("error_subcode\":33")
        && (e.contains("does not exist") || e.contains("missing permissions"))
}

/// Arma la respuesta HTTP con el binario y headers compartidos entre hit y miss.
/// `Cache-Control: immutable` + 30 días porque los `media_id` de Meta no cambian:
/// el browser no vuelve a pedirlo hasta un mes después.
fn build_media_response(bytes: Vec<u8>, mime: &str, filename: &str) -> axum::response::Response {
    let content_length = bytes.len();
    let mut resp = axum::response::Response::new(axum::body::Body::from(bytes));
    let headers = resp.headers_mut();
    if let Ok(v) = axum::http::HeaderValue::from_str(mime) {
        headers.insert(axum::http::header::CONTENT_TYPE, v);
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!(
        "inline; filename=\"{}\"",
        filename.replace('"', "'")
    )) {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, v);
    }
    headers.insert(
        axum::http::header::CONTENT_LENGTH,
        axum::http::HeaderValue::from(content_length),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, max-age=2592000, immutable"),
    );
    resp
}

/// Tipos de mensaje que se prefetchean al llegar por webhook.
/// Todos los tipos con media están incluidos — documentos también, pero el
/// límite de 5 MB (`MEDIA_CACHE_MAX_BYTES`) deja fuera los PDFs pesados.
pub(crate) fn should_prefetch_media(msg_type: &str) -> bool {
    matches!(
        msg_type,
        "audio" | "image" | "sticker" | "video" | "document"
    )
}

/// Descarga completa `info + body` con reintentos y backoff corto.
/// Devuelve `(bytes, mime, file_name)` o el error del último intento.
async fn download_media_with_retry(
    wa: &WhatsAppService,
    media_id: &str,
) -> Result<(Vec<u8>, String, Option<String>), String> {
    let mut last_err: Option<String> = None;
    for (attempt, delay_ms) in MEDIA_DOWNLOAD_RETRY_DELAYS_MS.iter().enumerate() {
        if *delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
        }
        match wa.download_media(media_id).await {
            Ok(ok) => return Ok(ok),
            Err(e) => {
                let err = e.to_string();
                tracing::warn!(
                    "[media] retry {}/{} media_id={} falló: {}",
                    attempt + 1,
                    MEDIA_DOWNLOAD_RETRY_DELAYS_MS.len(),
                    media_id,
                    err
                );
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "media download failed without detail".to_string()))
}

/// `download_media_info` con retry/backoff.
async fn download_media_info_with_retry(
    wa: &WhatsAppService,
    media_id: &str,
) -> Result<MediaInfo, String> {
    let mut last_err: Option<String> = None;
    for (attempt, delay_ms) in MEDIA_DOWNLOAD_RETRY_DELAYS_MS.iter().enumerate() {
        if *delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
        }
        match wa.download_media_info(media_id).await {
            Ok(info) => return Ok(info),
            Err(e) => {
                let err = e.to_string();
                tracing::warn!(
                    "prefetch_media({}): info retry {}/{} falló: {}",
                    media_id,
                    attempt + 1,
                    MEDIA_DOWNLOAD_RETRY_DELAYS_MS.len(),
                    err
                );
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "download_media_info failed without detail".to_string()))
}

/// `download_media_body` con retry/backoff.
async fn download_media_body_with_retry(
    wa: &WhatsAppService,
    media_id: &str,
    url: &str,
) -> Result<Vec<u8>, String> {
    let mut last_err: Option<String> = None;
    for (attempt, delay_ms) in MEDIA_DOWNLOAD_RETRY_DELAYS_MS.iter().enumerate() {
        if *delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
        }
        match wa.download_media_body(url).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                let err = e.to_string();
                tracing::warn!(
                    "prefetch_media({}): body retry {}/{} falló: {}",
                    media_id,
                    attempt + 1,
                    MEDIA_DOWNLOAD_RETRY_DELAYS_MS.len(),
                    err
                );
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "download_media_body failed without detail".to_string()))
}

/// Guard que libera el lock de prefetch al salir de `prefetch_media`,
/// haya terminado bien o mal. Evita que un panic o un early-return deje
/// un lock huérfano en Redis (el TTL de 60s lo limpiaría igual, pero
/// así lo liberamos apenas se puede).
struct MediaPrefetchGuard {
    redis: crate::cache::RedisClient,
    media_id: String,
}

impl Drop for MediaPrefetchGuard {
    fn drop(&mut self) {
        let redis = self.redis.clone();
        let media_id = self.media_id.clone();
        tokio::spawn(async move {
            redis.release_media_prefetch_lock(&media_id).await;
        });
    }
}

/// Descarga un media de Meta y lo guarda en Redis si pesa poco.
/// Fire-and-forget: se spawnea desde el webhook apenas llega el mensaje,
/// para que cuando el agente abra el chat el `GET /media/:id` encuentre
/// hit en Redis y responda en milisegundos.
pub(crate) async fn prefetch_media(state: Arc<AppState>, business_phone: String, media_id: String) {
    // Skip si ya está cacheado (puede pasar si el mismo media llega dos veces).
    if state.redis.get_media_cache(&media_id).await.is_some() {
        return;
    }

    // Lock para evitar descarga duplicada: si el endpoint ya está bajando
    // este media (race con el agente que abre el chat al instante), lo dejamos.
    if !state.redis.try_lock_media_prefetch(&media_id).await {
        tracing::debug!(
            "prefetch_media({}): ya hay otra tarea descargándolo",
            media_id
        );
        return;
    }
    // RAII manual: liberamos el lock al final.
    let _guard = MediaPrefetchGuard {
        redis: state.redis.clone(),
        media_id: media_id.clone(),
    };

    let wa = match resolve_service_for_phone(&state, &business_phone).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                "prefetch_media({}): no pude resolver service: {:?}",
                media_id,
                e
            );
            return;
        }
    };

    let info = match download_media_info_with_retry(&wa, &media_id).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("prefetch_media({}): info falló: {}", media_id, e);
            return;
        }
    };

    // Si Meta reporta tamaño y supera el límite, no cacheamos — lo bajará el
    // endpoint si el agente abre el media.
    if let Some(size) = info.file_size {
        if size > MEDIA_CACHE_MAX_BYTES as u64 {
            tracing::debug!(
                "prefetch_media({}): skip ({} bytes > {} max)",
                media_id,
                size,
                MEDIA_CACHE_MAX_BYTES
            );
            return;
        }
    }

    let bytes = match download_media_body_with_retry(&wa, &media_id, &info.url).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("prefetch_media({}): body falló: {}", media_id, e);
            return;
        }
    };

    // Guard tardío: si Meta no reportó `file_size` y el binario terminó siendo
    // grande, igual respetamos el límite.
    if bytes.len() > MEDIA_CACHE_MAX_BYTES {
        return;
    }

    state
        .redis
        .set_media_cache(&media_id, &bytes, &info.mime, info.file_name.as_deref())
        .await;
    tracing::debug!(
        "prefetch_media({}): cacheado {} bytes ({})",
        media_id,
        bytes.len(),
        info.mime
    );
}
