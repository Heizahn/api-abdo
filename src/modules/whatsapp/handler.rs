use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::OnceLock;
use tokio::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

/// Almacena el último payload crudo recibido de Meta (solo para debug).
static LAST_WEBHOOK_PAYLOAD: OnceLock<Mutex<Option<serde_json::Value>>> = OnceLock::new();

fn last_payload_store() -> &'static Mutex<Option<serde_json::Value>> {
    LAST_WEBHOOK_PAYLOAD.get_or_init(|| Mutex::new(None))
}
use mongodb::bson::{oid::ObjectId, DateTime};
use std::sync::Arc;

use crate::{
    auth::user_jwt::UserProfileClaims,
    crypto::aes::{decrypt_payload, encrypt_payload},
    db::WhatsAppRepository,
    error::ApiError,
    models::whatsapp::*,
    state::AppState,
};

use super::assignment::assign_conversation;

use super::service::WhatsAppService;
use super::ws::{broadcast_all, broadcast_except, WsServerEvent};

// ============================================
// WEBHOOK (público)
// ============================================

#[derive(serde::Deserialize)]
pub struct WebhookVerifyParams {
    #[serde(rename = "hub.mode")]
    pub mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub challenge: Option<String>,
}

/// GET /v1/webhook/whatsapp
/// Verificación del webhook por parte de Meta.
pub async fn verify_webhook(
    Query(params): Query<WebhookVerifyParams>,
) -> impl IntoResponse {
    let expected = std::env::var("WHATSAPP_VERIFY_TOKEN").unwrap_or_default();

    if params.mode.as_deref() == Some("subscribe")
        && params.verify_token.as_deref() == Some(expected.as_str())
    {
        tracing::info!("WhatsApp webhook verificado correctamente");
        (StatusCode::OK, params.challenge.unwrap_or_default())
    } else {
        tracing::warn!("WhatsApp webhook: token inválido");
        (StatusCode::FORBIDDEN, "token_invalido".to_string())
    }
}

/// GET /v1/auth-user/whatsapp/debug/last-webhook
/// Retorna el último payload crudo recibido de Meta. Solo para diagnóstico.
pub async fn debug_last_webhook_handler() -> Json<serde_json::Value> {
    let store = last_payload_store().lock().await;
    match store.as_ref() {
        Some(payload) => Json(serde_json::json!({ "ok": true, "received": true, "payload": payload })),
        None => Json(serde_json::json!({ "ok": true, "received": false, "payload": null })),
    }
}

/// POST /v1/webhook/whatsapp
/// Recibe notificaciones de Meta (mensajes entrantes + actualizaciones de estado).
/// Meta espera siempre HTTP 200 — cualquier otro código provoca reenvíos.
pub async fn receive_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    // Verificar firma HMAC-SHA256 si WHATSAPP_APP_SECRET está configurado
    if let Ok(app_secret) = std::env::var("WHATSAPP_APP_SECRET") {
        if !app_secret.is_empty() {
            let header_val = headers
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !verify_meta_signature(app_secret.as_bytes(), &body, header_val) {
                tracing::warn!("[webhook] firma inválida — request rechazada");
                return StatusCode::FORBIDDEN;
            }
        }
    }

    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("[webhook] JSON inválido: {}", e);
            return StatusCode::OK;
        }
    };

    // Guardar payload crudo para diagnóstico
    {
        let raw = serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
        *last_payload_store().lock().await = Some(raw);
    }
    let entries = match payload.entry {
        Some(e) => e,
        None => return StatusCode::OK,
    };

    for entry in entries {
        let changes = match entry.changes {
            Some(c) => c,
            None => continue,
        };

        for change in changes {
            if change.field.as_deref() != Some("messages") {
                continue;
            }
            let value = match change.value {
                Some(v) => v,
                None => continue,
            };

            // Procesar actualizaciones de estado (delivered / read / failed)
            if let Some(statuses) = value.statuses {
                for s in statuses {
                    if s.status == "failed" {
                        if let Some(errs) = s.errors.as_ref() {
                            for e in errs {
                                tracing::warn!(
                                    "[webhook] mensaje {} falló: code={:?} title={:?} message={:?}",
                                    s.id, e.code, e.title, e.message
                                );
                            }
                        } else {
                            tracing::warn!("[webhook] mensaje {} falló sin detalles", s.id);
                        }
                    }
                    match state.db.update_message_status(&s.id, &s.status).await {
                        Ok(Some(updated)) => {
                            let event = WsServerEvent::MensajeActualizado {
                                conversation_id: updated.conversation_id.to_hex(),
                                message_id: updated.wa_message_id.clone(),
                                status: s.status.clone(),
                            };
                            tracing::info!(
                                "[webhook] status {} → broadcast MENSAJE_ACTUALIZADO (wa_id={}, conv={})",
                                s.status, updated.wa_message_id, updated.conversation_id.to_hex()
                            );
                            broadcast_all(&state.ws_registry, &event).await;
                        }
                        Ok(None) => {
                            tracing::debug!(
                                "[webhook] status {} para wa_id={} sin doc en DB (ignorado)",
                                s.status, s.id
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                "[webhook] update_message_status error (wa_id={}, status={}): {}",
                                s.id, s.status, e
                            );
                        }
                    }
                }
            }

            // Procesar mensajes entrantes
            if let Some(messages) = value.messages {
                let contacts = value.contacts.unwrap_or_default();

                // El número del negocio que recibió el mensaje (normalizado a E.164 sin "+")
                let business_phone_raw = value.metadata
                    .as_ref()
                    .and_then(|m| m.display_phone_number.clone())
                    .unwrap_or_default();
                let business_phone = normalize_to_e164(&business_phone_raw);

                // find_wa_settings_by_phone ya filtra por active: true
                let settings = match state.db.find_wa_settings_by_phone(&business_phone).await {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        tracing::info!(
                            "[webhook] número de negocio no configurado o inactivo: raw={} norm={}",
                            business_phone_raw, business_phone
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::error!("[webhook] error buscando wa_settings: {}", e);
                        continue;
                    }
                };

                for msg in messages {

                    let agents = settings.agents.clone();

                    let name = contacts.iter()
                        .find(|c| c.wa_id.as_deref() == Some(&msg.from))
                        .and_then(|c| c.profile.as_ref())
                        .and_then(|p| p.name.clone());

                    // Upsert conversación (clave compuesta: contacto + número de negocio)
                    let (conv, conv_created) = match state.db.upsert_conversation(&msg.from, &business_phone, name).await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::error!("upsert_conversation error: {}", e);
                            continue;
                        }
                    };
                    let conv_id = match conv.id {
                        Some(id) => id,
                        None => continue,
                    };

                    // Si la conversación estaba cerrada, reabrirla en pending (sin dueño).
                    // El auto-assign de abajo la reasignará al agente con menos carga.
                    let was_reopened = if conv.status == "closed" {
                        match state.db.reopen_conversation(&conv_id).await {
                            Ok(changed) => changed,
                            Err(e) => {
                                tracing::warn!("reopen_conversation error: {}", e);
                                false
                            }
                        }
                    } else {
                        false
                    };

                    // Extraer contenido según tipo (body, media_id, mime, filename)
                    let extract_media = |m: Option<&InboundMedia>| m
                        .map(|x| (x.caption.clone(), x.id.clone(), x.mime_type.clone(), x.filename.clone()))
                        .unwrap_or((None, None, None, None));
                    let (body, media_id, media_mime_type, media_filename) = match msg.msg_type.as_str() {
                        "text" => (msg.text.as_ref().map(|t| t.body.clone()), None, None, None),
                        "image" => extract_media(msg.image.as_ref()),
                        "document" => extract_media(msg.document.as_ref()),
                        "audio" => extract_media(msg.audio.as_ref()),
                        "video" => extract_media(msg.video.as_ref()),
                        "sticker" => msg.sticker.as_ref()
                            .map(|m| (None, m.id.clone(), m.mime_type.clone(), None))
                            .unwrap_or((None, None, None, None)),
                        "location" => {
                            let label = msg.location.as_ref().and_then(|l| {
                                l.name.clone().or_else(|| l.address.clone()).or_else(|| {
                                    match (l.latitude, l.longitude) {
                                        (Some(lat), Some(lng)) => Some(format!("{},{}", lat, lng)),
                                        _ => None,
                                    }
                                })
                            });
                            (label, None, None, None)
                        }
                        _ => (None, None, None, None),
                    };

                    // Voice note: sólo relevante en `audio`. Meta envía `voice: true`
                    // para push-to-talk y `false` para archivos de audio subidos.
                    let voice = msg.msg_type == "audio"
                        && msg.audio.as_ref().and_then(|a| a.voice).unwrap_or(false);

                    let preview = body.clone().unwrap_or_else(|| format!("[{}]", msg.msg_type));

                    tracing::info!(
                        "[webhook] guardando mensaje de cliente registrado: {} | tipo: {} | preview: {}",
                        msg.from, msg.msg_type, preview
                    );

                    // Timestamp real desde Meta (Unix seconds en string), fallback a ahora.
                    let msg_ts = msg.timestamp.as_deref()
                        .and_then(parse_unix_seconds_to_bson)
                        .unwrap_or_else(DateTime::now);

                    let wa_msg = WaMessage {
                        id: None,
                        conversation_id: conv_id,
                        wa_message_id: msg.id.clone(),
                        direction: "in".to_string(),
                        msg_type: msg.msg_type.clone(),
                        body,
                        media_id,
                        media_mime_type,
                        media_filename,
                        status: None,
                        sent_by: None,
                        idempotency_key: None,
                        reply_to_wa_message_id: msg.context.as_ref().map(|c| c.id.clone()),
                        url_preview: None,
                        voice,
                        timestamp: msg_ts,
                    };

                    let saved = match state.db.save_message(wa_msg).await {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::error!("save_message error: {}", e);
                            continue;
                        }
                    };

                    if let Err(e) = state.db.touch_conversation(&conv_id, &preview, true, Some(msg_ts)).await {
                        tracing::warn!("touch_conversation error: {}", e);
                    }

                    // Releer conversación para emitir estado actualizado (unread_count, last_*).
                    let conv_now = state
                        .db
                        .find_conversation_by_id(&conv_id)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(conv);

                    // Si la conversación es nueva, avisar al front antes del mensaje.
                    if conv_created {
                        let ws_name = Some(settings.workspace_name.clone())
                            .filter(|w| !w.is_empty());
                        let new_ev = WsServerEvent::ConversacionNueva {
                            conversation: conv_to_item(conv_now.clone(), false, None, ws_name),
                        };
                        broadcast_all(&state.ws_registry, &new_ev).await;
                    } else if was_reopened {
                        // Cerrada → pending: el front debe re-integrarla en la bandeja activa.
                        let reopened_ev = WsServerEvent::ChatEstadoCambio {
                            conversation_id: conv_id.to_hex(),
                            new_status: "pending".to_string(),
                        };
                        broadcast_all(&state.ws_registry, &reopened_ev).await;
                    }

                    // MENSAJE_NUEVO a todos los conectados; el front filtra por conversación abierta.
                    let reply_to = resolve_reply_to_for_one(&state, &saved).await;
                    let saved_oid = saved.id;
                    let preview_text = saved.body.clone();
                    let message_item = msg_to_item(saved, None, reply_to);
                    let agent_count = state.ws_registry.read().await.len();
                    tracing::info!(
                        "[webhook] broadcast MENSAJE_NUEVO wa_id={} conv={} → {} agente(s) conectados",
                        message_item.wa_message_id, conv_id.to_hex(), agent_count
                    );
                    let msg_ev = WsServerEvent::MensajeNuevo {
                        conversation_id: conv_id.to_hex(),
                        message: message_item,
                    };
                    broadcast_all(&state.ws_registry, &msg_ev).await;

                    // URL preview: fire-and-forget. Si el cuerpo trae una URL,
                    // el job fetchea OG tags y emite URL_PREVIEW_READY cuando termina.
                    if let (Some(msg_oid), Some(text)) = (saved_oid, preview_text) {
                        super::url_preview::spawn_preview_job(
                            state.clone(),
                            msg_oid,
                            conv_id,
                            text,
                        );
                    }

                    // Auto-asignación: solo si sigue pending sin dueño.
                    if conv_now.assigned_to.is_none() {
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            assign_conversation(state_clone, conv_id, agents).await;
                        });
                    }
                }
            }
        }
    }

    StatusCode::OK
}

// ============================================
// ENDPOINTS DE STAFF/ADMIN (user JWT)
// ============================================

#[derive(serde::Deserialize)]
pub struct ConversationsQuery {
    pub status: Option<String>,
    pub assigned_to: Option<String>,
    pub business_phone: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

#[derive(serde::Deserialize)]
pub struct MessagesQuery {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("status" = Option<String>, Query, description = "Filtrar por estado: pending | in_progress | closed"),
        ("assigned_to" = Option<String>, Query, description = "Filtrar por UUID de agente"),
        ("business_phone" = Option<String>, Query, description = "Filtrar por número de negocio (E.164 sin '+')"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco para paginación (copiar de next_cursor)"),
        ("limit" = Option<i64>, Query, description = "Resultados por página (default: 20, max: 100)"),
    ),
    responses(
        (status = 200, description = "Lista de conversaciones", body = ConversationsListResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_conversations_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<ConversationsQuery>,
) -> Result<Json<ConversationsListResponse>, ApiError> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let business_phone_norm = q.business_phone.as_deref().map(normalize_to_e164);

    let convs = state.db
        .get_conversations(
            q.status.as_deref(),
            q.assigned_to.as_deref(),
            business_phone_norm.as_deref(),
            q.cursor.as_deref(),
            limit,
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let next_cursor = if (convs.len() as i64) < limit {
        None
    } else {
        convs.last().and_then(|c| {
            Some(format!(
                "{}_{}",
                c.last_message_at.timestamp_millis(),
                c.id?.to_hex()
            ))
        })
    };

    // Batch-fetch last_opened_at del agente actual para todas las conversaciones.
    let ids: Vec<ObjectId> = convs.iter().filter_map(|c| c.id).collect();
    let opens = state.db
        .get_conversation_opens(&claims.id, &ids)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    // Batch-fetch workspace_name por business_phone único.
    let mut unique_phones: Vec<String> = convs.iter().map(|c| c.business_phone.clone()).collect();
    unique_phones.sort();
    unique_phones.dedup();
    let workspaces = state.db
        .get_workspace_names(&unique_phones)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let data = convs
        .into_iter()
        .map(|c| {
            let last_opened = c.id.and_then(|id| opens.get(&id).copied());
            let ws = workspaces.get(&c.business_phone).cloned();
            conv_to_item(c, false, last_opened, ws)
        })
        .collect();

    Ok(Json(ConversationsListResponse { ok: true, data, next_cursor }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Detalle de conversación", body = ConversationDetailResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let conv = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let opens = state.db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv.business_phone).await;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        conversation: conv_to_item(conv, true, last_opened, workspace_name),
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/{id}/messages",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "ID de la conversación"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco (copiar de next_cursor)"),
        ("limit" = Option<i64>, Query, description = "Mensajes por página (default: 50, max: 200)"),
    ),
    responses(
        (status = 200, description = "Detalle de conversación + mensajes (más recientes primero)", body = ConversationMessagesResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_conversation_messages_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<ConversationMessagesResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let is_first_page = q.cursor.is_none();

    let messages = state.db
        .get_messages(&oid, q.cursor.as_deref(), limit)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let agent_names = resolve_sent_by_names(&state, &messages).await;
    let reply_items = resolve_reply_to_items(&state, &messages).await;

    let next_cursor = if (messages.len() as i64) < limit {
        None
    } else {
        messages.last().and_then(|m| {
            Some(format!(
                "{}_{}",
                m.timestamp.timestamp_millis(),
                m.id?.to_hex()
            ))
        })
    };

    // Registrar "chat abierto" por este agente (siempre, incluso en paginaciones).
    if let Err(e) = state.db.record_conversation_open(&claims.id, &oid).await {
        tracing::warn!("record_conversation_open error: {}", e);
    }

    // Transición pending → in_progress: sólo en la primera página, si el
    // agente actual es el asignado y la conversación sigue pending. El detalle
    // actualizado se obtiene con GET /conversations/:id — acá solo emitimos el
    // evento WS para que la UI reaccione.
    if is_first_page
        && conv.status == "pending"
        && conv.assigned_to.as_deref() == Some(claims.id.as_str())
    {
        if let Err(e) = state.db.update_conversation_status(&oid, "in_progress").await {
            tracing::warn!("update_conversation_status error: {}", e);
        } else {
            let ev = WsServerEvent::ChatEstadoCambio {
                conversation_id: id.clone(),
                new_status: "in_progress".to_string(),
            };
            broadcast_all(&state.ws_registry, &ev).await;
        }
    }

    Ok(Json(ConversationMessagesResponse {
        ok: true,
        data: messages
            .into_iter()
            .map(|m| {
                let name = m.sent_by.as_deref().and_then(|id| agent_names.get(id).cloned());
                let rto = m.reply_to_wa_message_id.as_deref()
                    .and_then(|wid| reply_items.get(wid).cloned());
                msg_to_item(m, name, rto)
            })
            .collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/messages",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    request_body = SendMessageRequest,
    responses(
        (status = 200, description = "Mensaje enviado", body = SendMessageResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn send_message_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // Lookup idempotente (fuente de verdad: DB, por `(conv_id, idempotency_key)`).
    // - sent/delivered/read → devolver el mismo mensaje (no reenviar a Meta).
    // - failed               → reintentar envío, actualizar `wa_message_id` + status.
    // - None (sin status)    → devolver como está (estado intermedio, no reenviamos).
    if let Some(key) = payload.idempotency_key.as_deref() {
        if let Some(existing) = state.db
            .find_message_by_idempotency(&oid, key)
            .await
            .map_err(|e| ApiError::DatabaseError(e))?
        {
            let existing_id = existing.id;
            let is_failed = existing.status.as_deref() == Some("failed");

            if !is_failed {
                let name = existing.sent_by.as_deref().map(|_| claims.name.clone());
                let rto = resolve_reply_to_for_one(&state, &existing).await;
                let item = msg_to_item(existing, name, rto);
                return Ok(Json(SendMessageResponse {
                    ok: true,
                    message_id: item.id.clone(),
                    message: item,
                }));
            }

            // Retry: reenviar a Meta con la configuración del negocio y actualizar el doc.
            // Se reusa el `reply_to` original del mensaje para mantener la cita.
            let wa = resolve_service_for_phone(&state, &conv.business_phone).await?;
            let retry_reply_to = existing.reply_to_wa_message_id.clone();
            let new_wa_id = wa.send_text(&conv.phone, &payload.content, retry_reply_to.as_deref())
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;

            let msg_oid = existing_id
                .ok_or_else(|| ApiError::Internal("mensaje previo sin _id".into()))?;
            let updated = state.db
                .update_message_retry(&msg_oid, &new_wa_id, "sent")
                .await
                .map_err(|e| ApiError::DatabaseError(e))?
                .ok_or_else(|| ApiError::Internal("no se pudo actualizar mensaje tras reintento".into()))?;

            state.db
                .touch_conversation(&oid, &payload.content, false, None)
                .await
                .map_err(|e| ApiError::DatabaseError(e))?;

            let rto = resolve_reply_to_for_one(&state, &updated).await;
            let item = msg_to_item(updated, Some(claims.name.clone()), rto);

            // Broadcast del retry — status vuelve a "sent", el front actualiza la burbuja.
            let ev = WsServerEvent::MensajeNuevo {
                conversation_id: id.clone(),
                message: item.clone(),
            };
            broadcast_all(&state.ws_registry, &ev).await;

            return Ok(Json(SendMessageResponse {
                ok: true,
                message_id: item.id.clone(),
                message: item,
            }));
        }
    }

    // Envío nuevo.
    let wa = resolve_service_for_phone(&state, &conv.business_phone).await?;

    let wa_id = wa.send_text(&conv.phone, &payload.content, payload.reply_to.as_deref())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let msg = WaMessage {
        id: None,
        conversation_id: oid,
        wa_message_id: wa_id,
        direction: "out".to_string(),
        msg_type: "text".to_string(),
        body: Some(payload.content.clone()),
        media_id: None,
        media_mime_type: None,
        media_filename: None,
        status: Some("sent".to_string()),
        sent_by: Some(claims.id.clone()),
        idempotency_key: payload.idempotency_key.clone(),
        reply_to_wa_message_id: payload.reply_to.clone(),
        url_preview: None,
        voice: false,
        timestamp: DateTime::now(),
    };

    let saved = state.db.save_message(msg).await.map_err(|e| ApiError::DatabaseError(e))?;
    state.db.touch_conversation(&oid, &payload.content, false, None).await.map_err(|e| ApiError::DatabaseError(e))?;

    let rto = resolve_reply_to_for_one(&state, &saved).await;
    let saved_oid = saved.id;
    let item = msg_to_item(saved, Some(claims.name.clone()), rto);

    // Broadcast del mensaje outbound. El front deduplica contra `idempotency_key`
    // si ya recibió la respuesta HTTP.
    let ev = WsServerEvent::MensajeNuevo {
        conversation_id: id.clone(),
        message: item.clone(),
    };
    broadcast_all(&state.ws_registry, &ev).await;

    // URL preview async: si el agente mandó una URL, fetcheamos OG tags y
    // emitimos URL_PREVIEW_READY cuando termina. El preview respeta cache
    // Redis por URL, así que re-enviar el mismo link no re-fetchea.
    if let Some(msg_oid) = saved_oid {
        super::url_preview::spawn_preview_job(
            state.clone(),
            msg_oid,
            oid,
            payload.content.clone(),
        );
    }

    Ok(Json(SendMessageResponse {
        ok: true,
        message_id: item.id.clone(),
        message: item,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/mark-read",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Mensajes marcados como leídos", body = MarkReadResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn mark_read_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<MarkReadResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // Actualizar status de inbound en DB y obtener los que cambiaron.
    let changed_ids = state.db
        .mark_inbound_as_read(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    // Resetear contador local en la conversación.
    let _ = state.db.reset_unread(&oid).await;

    // Notificar a Meta (ticks azules + mic azul en voice notes) para cada
    // inbound del batch. Meta NO propaga `read` a mensajes anteriores — en
    // particular, los audios sólo muestran el mic azul en el teléfono del
    // cliente si se llama `status: "read"` sobre ese `wa_message_id` puntual.
    // Best-effort: si falta credencial o Meta responde error, logueamos y
    // seguimos (no bloquea el endpoint, va en spawn).
    if !changed_ids.is_empty() {
        match resolve_service_for_phone(&state, &conv.business_phone).await {
            Ok(wa) => {
                let ids_to_ack = changed_ids.clone();
                let conv_hex = oid.to_hex();
                tokio::spawn(async move {
                    let mut ok = 0usize;
                    let mut err = 0usize;
                    for wamid in &ids_to_ack {
                        match wa.mark_as_read(wamid).await {
                            Ok(()) => ok += 1,
                            Err(e) => {
                                err += 1;
                                tracing::warn!(
                                    "[mark-read] Meta mark_as_read falló conv={} wamid={}: {}",
                                    conv_hex, wamid, e
                                );
                            }
                        }
                    }
                    tracing::info!(
                        "[mark-read] Meta ACK conv={} total={} ok={} err={}",
                        conv_hex, ids_to_ack.len(), ok, err
                    );
                });
            }
            Err(e) => {
                tracing::warn!("[mark-read] no se pudo resolver WhatsAppService: {:?}", e);
            }
        }
    }

    // Broadcast del batch. El front propaga `status: "read"` en la UI local.
    if !changed_ids.is_empty() {
        let ev = WsServerEvent::MensajesVistos {
            conversation_id: id.clone(),
            message_ids: changed_ids.clone(),
            status: "read".to_string(),
        };
        broadcast_all(&state.ws_registry, &ev).await;
    }

    Ok(Json(MarkReadResponse { ok: true, message_ids: changed_ids }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/take",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Conversación tomada (idempotente si ya era del agente)", body = TakeConversationResponse),
        (status = 409, description = "Ya fue tomada por otro agente o no está pending"),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn take_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<TakeConversationResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let existing = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let was_already_mine = existing.assigned_to.as_deref() == Some(claims.id.as_str());

    // `take_conversation` solo avanza si status=pending Y (sin dueño O dueño=self).
    let taken = state.db
        .take_conversation(&oid, &claims.id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let conv = match taken {
        Some(c) => c,
        None => return Err(ApiError::Conflict("conversacion_ya_tomada".into())),
    };

    // Sólo incrementamos la carga si es la primera vez (no idempotente).
    if !was_already_mine {
        state.redis.incr_agent_load(&claims.id).await;

        // CHAT_TOMADO al resto: el agente que tomó ya tiene la respuesta HTTP.
        let ev = WsServerEvent::ChatTomado {
            conversation_id: id.clone(),
            taken_by: claims.id.clone(),
            status: conv.status.clone(),
        };
        broadcast_except(&state.ws_registry, &claims.id, &ev).await;
    }

    // `last_opened_at` del agente (si ya había abierto antes).
    let opens = state.db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv.business_phone).await;

    Ok(Json(TakeConversationResponse {
        ok: true,
        conversation: conv_to_item(conv, true, last_opened, workspace_name),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/transfer",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    request_body = TransferConversationRequest,
    responses(
        (status = 200, description = "Conversación transferida", body = ConversationDetailResponse),
        (status = 404, description = "Conversación o usuario destino no encontrado"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn transfer_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<TransferConversationRequest>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // Validar que el usuario destino exista.
    use crate::db::UserRepository;
    let _target = state.db
        .find_user_by_id(&payload.user_id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or_else(|| ApiError::NotFound)?;

    let from_agent = conv.assigned_to.clone();

    state.db.assign_conversation(&oid, Some(&payload.user_id))
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    if let Some(prev) = from_agent.as_deref() {
        if prev != payload.user_id {
            state.redis.decr_agent_load(prev).await;
        }
    }
    state.redis.incr_agent_load(&payload.user_id).await;

    if let Some(note) = payload.note.as_deref() {
        tracing::info!(
            "[transfer] conv={} de {:?} → {} por {} ({}): {}",
            id, from_agent, payload.user_id, claims.id, claims.name, note
        );
    }

    let conv_after = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let opens = state.db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv_after.business_phone).await;
    let conv_item = conv_to_item(conv_after, true, last_opened, workspace_name);

    // Emitir tras tener el item listo — incluye el estado actualizado con workspace_name y assigned_to nuevo.
    let ev = WsServerEvent::ChatTransferido {
        conversation_id: id.clone(),
        from_user_id: from_agent,
        to_user_id: payload.user_id.clone(),
        conversation: conv_item.clone(),
    };
    broadcast_all(&state.ws_registry, &ev).await;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        conversation: conv_item,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/close",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Conversación cerrada", body = ConversationDetailResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn close_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // Capturar al agente ANTES de cerrar — `close_conversation` desasigna.
    let prev_agent = conv.assigned_to.clone();

    state.db.close_conversation(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    if let Some(agent) = prev_agent.as_deref() {
        state.redis.decr_agent_load(agent).await;
    }

    let ev = WsServerEvent::ChatCerrado { conversation_id: id.clone() };
    broadcast_all(&state.ws_registry, &ev).await;

    let conv_after = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let opens = state.db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv_after.business_phone).await;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        conversation: conv_to_item(conv_after, true, last_opened, workspace_name),
    }))
}

// ============================================
// AGENTES TRANSFERIBLES
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/transferable-agents",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Usuarios con permiso para atender chats (bCanChat == true)", body = TransferableAgentsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_transferable_agents_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<TransferableAgentsResponse>, ApiError> {
    use crate::db::UserRepository;
    let users = state.db.find_chat_agents().await.map_err(|e| ApiError::DatabaseError(e))?;
    let data = users
        .into_iter()
        .map(|u| TransferableAgentItem {
            id: u.id,
            name: u.name,
            email: u.email,
            role: u.role,
        })
        .collect();
    Ok(Json(TransferableAgentsResponse { ok: true, data }))
}

// ============================================
// SETTINGS — Configuración de números y agentes
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/settings",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Lista de configuraciones", body = SettingsListResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_settings_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SettingsListResponse>, ApiError> {
    let items = state.db.get_all_wa_settings().await.map_err(|e| ApiError::DatabaseError(e))?;
    Ok(Json(SettingsListResponse {
        ok: true,
        data: items.into_iter().map(settings_to_item).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/settings",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = CreateSettingsRequest,
    responses(
        (status = 200, description = "Configuración creada", body = SettingsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn create_settings_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateSettingsRequest>,
) -> Result<Json<SettingsResponse>, ApiError> {
    // Normalizar el número a E.164 venezolano sin "+"
    let phone = normalize_to_e164(&payload.phone);
    let now = mongodb::bson::DateTime::now();

    let access_token = validate_access_token(&payload.access_token)?;
    if payload.phone_number_id.trim().is_empty() {
        return Err(ApiError::BadRequest("phone_number_id requerido".into()));
    }

    let encrypted = encrypt_payload(&settings_secret(), access_token);

    let doc = WaSettings {
        id: None,
        phone,
        workspace_name: payload.workspace_name,
        phone_number_id: payload.phone_number_id,
        access_token: encrypted,
        agents: payload.agents,
        active: true,
        created_at: now,
        updated_at: now,
    };

    let created = state.db.create_wa_settings(doc).await.map_err(|e| ApiError::DatabaseError(e))?;
    Ok(Json(SettingsResponse { ok: true, data: settings_to_item(created) }))
}

#[utoipa::path(
    put,
    path = "/v1/auth-user/whatsapp/settings/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la configuración")),
    request_body = UpdateSettingsRequest,
    responses(
        (status = 200, description = "Configuración actualizada", body = UpdateResponse),
        (status = 404, description = "No encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn update_settings_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateSettingsRequest>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    // Cifrar access_token si vino con valor. `None` o vacío ⇒ no tocar el guardado.
    let encrypted_token = match payload.access_token.as_deref() {
        Some(raw) if !raw.trim().is_empty() => {
            let clean = validate_access_token(raw)?;
            Some(encrypt_payload(&settings_secret(), clean))
        }
        _ => None,
    };

    state.db
        .update_wa_settings(
            &oid,
            payload.workspace_name,
            payload.phone_number_id,
            encrypted_token,
            payload.agents,
            payload.active,
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    Ok(Json(UpdateResponse { ok: true }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/settings/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la configuración")),
    responses(
        (status = 200, description = "Configuración eliminada", body = UpdateResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn delete_settings_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    state.db.delete_wa_settings(&oid).await.map_err(|e| ApiError::DatabaseError(e))?;
    Ok(Json(UpdateResponse { ok: true }))
}

// ============================================
// MEDIA (descarga proxy)
// ============================================

/// Proxy de descarga para media subido por el cliente. El binario real vive en la
/// CDN de Meta y sólo es accesible con el access token del negocio — por eso la
/// ruta pasa por el backend en vez de entregar la URL directa al front.
///
/// Autorización: el agente debe estar en `WaSettings.agents` del `business_phone`
/// de la conversación a la que pertenece el media.
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
    let msg = state.db
        .find_message_by_media_id(&media_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 2. Conversación → business_phone.
    let conv = state.db
        .find_conversation_by_id(&msg.conversation_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 3. Settings del negocio (auth + credenciales en un solo lookup).
    let settings = state.db
        .find_wa_settings_by_phone(&conv.business_phone)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::Internal(format!(
            "wa_settings inactivo o no encontrado para {}",
            conv.business_phone
        )))?;

    if !settings.agents.iter().any(|id| id == &claims.id) {
        return Err(ApiError::Forbidden);
    }
    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        return Err(ApiError::Internal(
            "wa_settings sin phone_number_id o access_token configurados".into(),
        ));
    }

    // 4. Service con el token descifrado → descarga.
    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
    let wa = WhatsAppService::new(state.reqwest_client.clone(), settings.phone_number_id, token);

    let (bytes, mime, remote_filename) = wa.download_media(&media_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // 5. Respuesta binaria. `inline` para que el browser lo renderice (imagen/pdf)
    // en lugar de forzar la descarga. Front puede hacer `<img src="...">`.
    let filename = msg.media_filename
        .clone()
        .or(remote_filename)
        .unwrap_or_else(|| media_id.clone());

    let mut resp = axum::response::Response::new(axum::body::Body::from(bytes));
    let headers = resp.headers_mut();
    if let Ok(v) = axum::http::HeaderValue::from_str(&mime) {
        headers.insert(axum::http::header::CONTENT_TYPE, v);
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("inline; filename=\"{}\"", filename.replace('"', "'"))) {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, v);
    }
    // Cache agresivo: el `media_id` es estable e inmutable en Meta.
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, max-age=86400"),
    );
    Ok(resp)
}

// ============================================
// HELPERS INTERNOS
// ============================================

/// Verifica la firma `X-Hub-Signature-256` de Meta: `sha256=<hex>` sobre el body crudo.
fn verify_meta_signature(app_secret: &[u8], body: &[u8], header_val: &str) -> bool {
    let expected_hex = match header_val.strip_prefix("sha256=") {
        Some(h) => h,
        None => return false,
    };
    let expected_bytes = match hex_decode(expected_hex) {
        Some(b) => b,
        None => return false,
    };
    let mut mac = match HmacSha256::new_from_slice(app_secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify_slice(&expected_bytes).is_ok()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Secreto AES para cifrar `WaSettings.access_token` en reposo.
/// Reutilizamos `JWT_SECRET` — alta entropía y estrictamente privado del backend.
fn settings_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

/// Valida un access_token de Meta. Un token legítimo es un string continuo
/// base64url-ish sin espacios ni comillas. Cualquier carácter extraño suele
/// indicar copy-paste con varias variables (ej: pegar una línea de `.env`).
fn validate_access_token(raw: &str) -> Result<&str, ApiError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(ApiError::BadRequest("access_token requerido".into()));
    }
    if t.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
        return Err(ApiError::BadRequest(
            "access_token inválido: contiene espacios o comillas".into(),
        ));
    }
    Ok(t)
}

/// Resuelve el `WhatsAppService` para el `business_phone` de una conversación:
/// busca `WaSettings`, descifra el `access_token` y construye el cliente.
async fn resolve_service_for_phone(
    state: &Arc<AppState>,
    business_phone: &str,
) -> Result<WhatsAppService, ApiError> {
    let settings = state
        .db
        .find_wa_settings_by_phone(business_phone)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::Internal(format!(
            "wa_settings inactivo o no encontrado para {}",
            business_phone
        )))?;

    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        return Err(ApiError::Internal(
            "wa_settings sin phone_number_id o access_token configurados".into(),
        ));
    }

    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;

    Ok(WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id,
        token,
    ))
}

/// Normaliza cualquier formato de número venezolano a E.164 sin "+" (ej: "584141234567")
fn normalize_to_e164(phone: &str) -> String {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.starts_with("58") {
        digits
    } else if let Some(rest) = digits.strip_prefix('0') {
        format!("58{}", rest)
    } else {
        format!("58{}", digits)
    }
}

// ============================================
// HELPERS DE MAPEO
// ============================================

fn iso8601(dt: DateTime) -> String {
    dt.try_to_rfc3339_string().unwrap_or_default()
}

fn conv_to_item(
    c: WaConversation,
    include_client_id: bool,
    last_opened_at: Option<DateTime>,
    workspace_name: Option<String>,
) -> ConversationItem {
    ConversationItem {
        id: c.id.map(|o| o.to_hex()).unwrap_or_default(),
        customer_phone: c.phone,
        customer_name: c.name,
        business_phone: c.business_phone,
        workspace_name,
        status: c.status,
        assigned_to: c.assigned_to,
        last_message_at: iso8601(c.last_message_at),
        last_message_preview: c.last_message_preview,
        unread_count: c.unread_count,
        created_at: iso8601(c.created_at),
        client_id: if include_client_id { c.client_id.map(|o| o.to_hex()) } else { None },
        last_opened_at: last_opened_at.map(iso8601),
    }
}

/// Atajo para handlers que tocan una sola conversación: resuelve `workspace_name`
/// por su `business_phone` vía `WaSettings`.
async fn resolve_workspace_name(state: &Arc<AppState>, business_phone: &str) -> Option<String> {
    if business_phone.is_empty() {
        return None;
    }
    state
        .db
        .get_workspace_names(&[business_phone.to_string()])
        .await
        .ok()
        .and_then(|m| m.get(business_phone).cloned())
}

fn settings_to_item(s: WaSettings) -> SettingsItem {
    SettingsItem {
        id: s.id.map(|o| o.to_hex()).unwrap_or_default(),
        phone: s.phone,
        workspace_name: s.workspace_name,
        phone_number_id: s.phone_number_id,
        has_access_token: !s.access_token.is_empty(),
        agents: s.agents,
        active: s.active,
        created_at: iso8601(s.created_at),
        updated_at: iso8601(s.updated_at),
    }
}

fn msg_to_item(
    m: WaMessage,
    from_user_name: Option<String>,
    reply_to: Option<ReplyToItem>,
) -> MessageItem {
    MessageItem {
        id: m.id.map(|o| o.to_hex()).unwrap_or_default(),
        conversation_id: m.conversation_id.to_hex(),
        wa_message_id: m.wa_message_id,
        direction: m.direction,
        msg_type: m.msg_type,
        content: m.body,
        media_id: m.media_id,
        media_mime_type: m.media_mime_type,
        media_filename: m.media_filename,
        status: m.status,
        from_user_id: m.sent_by,
        from_user_name,
        idempotency_key: m.idempotency_key,
        reply_to,
        url_preview: m.url_preview,
        voice: m.voice,
        created_at: iso8601(m.timestamp),
    }
}

/// Atajo usado por jobs async (`url_preview`) para armar un `MessageItem`
/// completo a partir de un `WaMessage` recién releído: resuelve `sent_by_name`
/// y `reply_to` en un solo call. Costo: 1-2 queries a `Users` / `WaMessages`.
pub(super) async fn build_message_item(state: &Arc<AppState>, m: WaMessage) -> MessageItem {
    use crate::db::UserRepository;
    let name = match m.sent_by.as_deref() {
        Some(id) => state.db.find_user_by_id(id).await.ok().flatten().map(|u| u.name),
        None => None,
    };
    let reply_to = resolve_reply_to_for_one(state, &m).await;
    msg_to_item(m, name, reply_to)
}

/// Trunca el cuerpo del mensaje citado a ~80 chars (seguro en UTF-8).
/// Se usa sólo para preview en la UI; el mensaje original completo sigue
/// disponible por su `wa_message_id`.
fn preview_truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars { out.push('…'); break; }
        out.push(c);
    }
    out
}

/// Atajo para un solo mensaje: reusa el helper batch y devuelve el `ReplyToItem`
/// correspondiente si existe.
async fn resolve_reply_to_for_one(
    state: &Arc<AppState>,
    m: &WaMessage,
) -> Option<ReplyToItem> {
    let wid = m.reply_to_wa_message_id.as_ref()?;
    let items = resolve_reply_to_items(state, std::slice::from_ref(m)).await;
    items.get(wid).cloned()
}

/// Batch-resuelve los `reply_to` de un conjunto de mensajes en un solo query a
/// `WaMessages` (+ uno a `Users` para los nombres de agentes).
///
/// Devuelve un mapa `wa_message_id citado → ReplyToItem` listo para armar el
/// `MessageItem`. Mensajes cuyo `reply_to_wa_message_id` no existe en DB
/// (ej. mensajes anteriores al deploy del feature) quedan fuera del mapa y
/// el front recibirá `reply_to: null`.
async fn resolve_reply_to_items(
    state: &Arc<AppState>,
    messages: &[WaMessage],
) -> std::collections::HashMap<String, ReplyToItem> {
    use crate::db::UserRepository;

    // Recolecto los wamid citados, dedup.
    let mut wa_ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.reply_to_wa_message_id.clone())
        .collect();
    wa_ids.sort();
    wa_ids.dedup();
    if wa_ids.is_empty() {
        return std::collections::HashMap::new();
    }

    // Batch lookup del mensaje original.
    let originals = match state.db.find_messages_by_wa_ids(&wa_ids).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("resolve_reply_to_items find_messages_by_wa_ids error: {}", e);
            return std::collections::HashMap::new();
        }
    };

    // Nombres de agentes para los originales outbound — un batch sobre Users.
    let mut sender_ids: Vec<String> = originals.values()
        .filter_map(|m| m.sent_by.clone())
        .collect();
    sender_ids.sort();
    sender_ids.dedup();
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for id in sender_ids {
        if let Ok(Some(u)) = state.db.find_user_by_id(&id).await {
            names.insert(id, u.name);
        }
    }

    // Ensamblar ReplyToItems.
    originals.into_iter().map(|(wa_id, m)| {
        let preview_content = m.body.as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| preview_truncate(s, 80));
        let from_user_name = m.sent_by.as_deref().and_then(|id| names.get(id).cloned());
        let item = ReplyToItem {
            wa_message_id: wa_id.clone(),
            preview_content,
            preview_type: m.msg_type,
            direction: m.direction,
            from_user_name,
        };
        (wa_id, item)
    }).collect()
}

/// Convierte un timestamp de Meta (Unix seconds en string) a `bson::DateTime`.
fn parse_unix_seconds_to_bson(s: &str) -> Option<DateTime> {
    let secs: i64 = s.parse().ok()?;
    Some(DateTime::from_millis(secs.checked_mul(1000)?))
}

/// Resuelve nombres de agentes para un batch de mensajes, deduplicando UUIDs
/// y leyendo de `Users` en paralelo.
async fn resolve_sent_by_names(
    state: &Arc<AppState>,
    messages: &[WaMessage],
) -> std::collections::HashMap<String, String> {
    use crate::db::UserRepository;

    let mut ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.sent_by.clone())
        .collect();
    ids.sort();
    ids.dedup();

    let mut out = std::collections::HashMap::new();
    for id in ids {
        if let Ok(Some(u)) = state.db.find_user_by_id(&id).await {
            out.insert(id, u.name);
        }
    }
    out
}
