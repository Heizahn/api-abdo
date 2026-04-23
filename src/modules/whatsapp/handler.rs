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
                            // Si este mensaje era el último de la conversación, propagar el
                            // nuevo status al preview del listado (checkmarks en vivo).
                            match state.db
                                .update_conversation_status_if_last(
                                    &updated.conversation_id,
                                    &updated.wa_message_id,
                                    &s.status,
                                )
                                .await
                            {
                                Ok(true) => tracing::debug!(
                                    "[webhook] last_message_status={} propagado a conv {}",
                                    s.status, updated.conversation_id.to_hex()
                                ),
                                Ok(false) => {} // no era el último — sin propagar
                                Err(e) => tracing::warn!(
                                    "[webhook] update_conversation_status_if_last error: {}", e
                                ),
                            }

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
                            // Preview: nombre del lugar → dirección → "Ubicación" genérico.
                            // Las coordenadas ya van en el campo `location`
                            // estructurado; no las ponemos en el preview.
                            let label = msg.location.as_ref().and_then(|l| {
                                l.name.clone().or_else(|| l.address.clone())
                            }).unwrap_or_else(|| "Ubicación".to_string());
                            (Some(label), None, None, None)
                        }
                        // Respuesta a botón/lista interactivo: Meta envía
                        // `interactive.button_reply.{id,title}` o
                        // `interactive.list_reply.{id,title,description}`.
                        // Guardamos el `title` elegido como body (para preview) y
                        // el objeto crudo en `interactive_payload` para que el
                        // front pueda renderizar el contexto completo.
                        "interactive" => {
                            let txt = msg.interactive.as_ref().and_then(|v| {
                                v.get("button_reply").and_then(|b| b.get("title"))
                                    .or_else(|| v.get("list_reply").and_then(|l| l.get("title")))
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
                            });
                            (txt, None, None, None)
                        }
                        // Legacy: botón de template (quick-reply de template).
                        // Meta envía `button.text` con el label tapped.
                        "button" => {
                            let txt = msg.button.as_ref()
                                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()));
                            (txt, None, None, None)
                        }
                        // Tarjeta de contacto: Meta manda un array; usamos el
                        // nombre del primero para el preview del listado.
                        "contacts" => {
                            let name = msg.contacts.as_ref()
                                .and_then(|v| v.as_array())
                                .and_then(|arr| arr.first())
                                .and_then(|c| c.get("name"))
                                .and_then(|n| {
                                    n.get("formatted_name")
                                        .or_else(|| n.get("first_name"))
                                        .and_then(|x| x.as_str())
                                        .map(|s| s.to_string())
                                });
                            (name, None, None, None)
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

                    // Para inbound de tipo `interactive`/`button`, preservamos el
                    // objeto crudo de Meta (incluye `button_reply`/`list_reply`)
                    // para que el front pueda renderizar la burbuja con el
                    // contexto completo de la selección.
                    let interactive_payload = match msg.msg_type.as_str() {
                        "interactive" => msg.interactive.clone(),
                        "button" => msg.button.clone(),
                        _ => None,
                    };

                    // Payload completo de contactos compartidos (vCard).
                    let contacts_payload = if msg.msg_type == "contacts" {
                        msg.contacts.clone()
                    } else {
                        None
                    };

                    // Datos estructurados de ubicación para que el front
                    // renderice el mapa (iframe de OSM/Google, img estática,
                    // o link a maps — lo decide el front).
                    let location_payload = if msg.msg_type == "location" {
                        msg.location.as_ref().and_then(|l| {
                            match (l.latitude, l.longitude) {
                                (Some(lat), Some(lng)) => Some(crate::models::whatsapp::LocationPayload {
                                    latitude: lat,
                                    longitude: lng,
                                    name: l.name.clone(),
                                    address: l.address.clone(),
                                }),
                                _ => None,
                            }
                        })
                    } else {
                        None
                    };

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
                        template_name: None,
                        template_language: None,
                        template_components: None,
                        interactive_payload,
                        contacts_payload,
                        location: location_payload,
                        timestamp: msg_ts,
                    };

                    let saved = match state.db.save_message(wa_msg).await {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::error!("save_message error: {}", e);
                            continue;
                        }
                    };

                    // Prefetch del binario: el agente casi siempre abre el
                    // media a los pocos segundos. Si ya está en Redis cuando
                    // hace el GET, responde en ms en vez de 2 viajes a Meta.
                    if let Some(ref mid) = saved.media_id {
                        if should_prefetch_media(&saved.msg_type) {
                            let state_cl = state.clone();
                            let phone_cl = conv.business_phone.clone();
                            let mid_cl = mid.clone();
                            tokio::spawn(async move {
                                prefetch_media(state_cl, phone_cl, mid_cl).await;
                            });
                        }
                    }

                    let touch = crate::db::ConversationTouch {
                        preview: &preview,
                        msg_type: &msg.msg_type,
                        direction: "in",
                        wa_message_id: &msg.id,
                        from_user_id: None,
                        media_filename: saved.media_filename.as_deref(),
                        status: None,
                        increment_unread: true,
                        last_message_at: Some(msg_ts),
                    };
                    if let Err(e) = state.db.touch_conversation(&conv_id, touch).await {
                        tracing::warn!("touch_conversation error: {}", e);
                    }

                    // Actualizar `last_inbound_at` → reabre la ventana de 24h.
                    if let Err(e) = state.db.update_last_inbound_at(&conv_id, msg_ts).await {
                        tracing::warn!("update_last_inbound_at error: {}", e);
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
                        let resolved = resolve_customer_name(&state, &conv_now).await;
                        let new_ev = WsServerEvent::ConversacionNueva {
                            conversation: conv_to_item(conv_now.clone(), false, None, ws_name, resolved, None),
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

                    // Ventana de 24h: el inbound reabre la ventana. Emitimos el
                    // evento siempre para que los countdowns del front se
                    // re-sincronicen con el nuevo `freeform_expires_at`.
                    let (can_send_freeform, freeform_expires_at) =
                        compute_freeform_state(Some(msg_ts));
                    let estado_ev = WsServerEvent::ConversacionEstado {
                        conversation_id: conv_id.to_hex(),
                        last_inbound_at: Some(iso8601(msg_ts)),
                        can_send_freeform,
                        freeform_expires_at,
                    };
                    broadcast_all(&state.ws_registry, &estado_ev).await;

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

#[derive(serde::Deserialize)]
pub struct ConversationStatsQuery {
    pub business_phone: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/stats",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("business_phone" = Option<String>, Query, description = "Filtrar el scope a un solo número de negocio (E.164 sin '+'). Si se omite, cuenta sobre todos los números."),
    ),
    responses(
        (status = 200, description = "Contadores de conversaciones por categoría", body = ConversationStatsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn conversations_stats_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<ConversationStatsQuery>,
) -> Result<Json<ConversationStatsResponse>, ApiError> {
    let business_phone_norm = q.business_phone.as_deref().map(normalize_to_e164);
    let stats = state
        .db
        .get_conversation_stats(business_phone_norm.as_deref(), &claims.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(ConversationStatsResponse { ok: true, data: stats }))
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

    // Batch-resolve nombre del contacto contra Clients: primero por client_id,
    // luego por teléfono para los que no tienen link. Evita N+1 en listados.
    let (names_by_id, names_by_phone) = {
        use crate::db::ProfileRepository;
        let client_ids: Vec<ObjectId> = convs.iter().filter_map(|c| c.client_id).collect();
        let mut customer_phones: Vec<String> = convs.iter()
            .filter(|c| c.client_id.is_none())
            .map(|c| c.phone.clone())
            .collect();
        customer_phones.sort();
        customer_phones.dedup();
        let (ids_res, phones_res) = tokio::join!(
            state.db.get_client_names_by_ids(&client_ids),
            state.db.get_client_names_by_phones(&customer_phones),
        );
        (
            ids_res.map_err(ApiError::DatabaseError)?,
            phones_res.map_err(ApiError::DatabaseError)?,
        )
    };

    // Batch-resolve nombres de agentes (autor del último mensaje outbound).
    let agent_names = resolve_last_message_agent_names(&state, &convs).await;

    let data = convs
        .into_iter()
        .map(|c| {
            let last_opened = c.id.and_then(|id| opens.get(&id).copied());
            let ws = workspaces.get(&c.business_phone).cloned();
            let resolved = c.client_id
                .and_then(|id| names_by_id.get(&id).cloned())
                .or_else(|| names_by_phone.get(&c.phone).cloned());
            let agent_name = c.last_message_from_user_id
                .as_ref()
                .and_then(|id| agent_names.get(id).cloned());
            conv_to_item(c, false, last_opened, ws, resolved, agent_name)
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
    let resolved = resolve_customer_name(&state, &conv).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv).await;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        conversation: conv_to_item(conv, true, last_opened, workspace_name, resolved, agent_name),
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

    // Decidir modo: template (siempre permitido) vs texto (sólo dentro de la
    // ventana de 24h). El discriminador `type: "template"` o la presencia del
    // campo `template` activan el modo template.
    let mode = resolve_send_mode(&payload, &conv)?;

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
            let existing_msg_type = existing.msg_type.clone();
            let existing_media_filename = existing.media_filename.clone();
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
            let preview_url_flag = payload.preview_url.unwrap_or(false);
            let (new_wa_id, preview) = match &mode {
                SendMode::Text { content } => {
                    let wa_id = wa.send_text(&conv.phone, content, retry_reply_to.as_deref(), preview_url_flag)
                        .await
                        .map_err(|e| ApiError::Internal(e.to_string()))?;
                    (wa_id, content.clone())
                }
                SendMode::Template { tpl } => {
                    let components_value = tpl.components.as_ref()
                        .map(|v| serde_json::Value::Array(v.clone()));
                    let wa_id = wa.send_template(
                            &conv.phone,
                            &tpl.name,
                            &tpl.language,
                            components_value.as_ref(),
                        )
                        .await
                        .map_err(|e| ApiError::Internal(e.to_string()))?;
                    (wa_id, template_preview(tpl))
                }
                SendMode::Interactive { payload: inter } => {
                    let wa_id = wa.send_interactive(&conv.phone, inter, retry_reply_to.as_deref())
                        .await
                        .map_err(|e| ApiError::Internal(e.to_string()))?;
                    (wa_id, interactive_preview(inter))
                }
            };

            let msg_oid = existing_id
                .ok_or_else(|| ApiError::Internal("mensaje previo sin _id".into()))?;
            let updated = state.db
                .update_message_retry(&msg_oid, &new_wa_id, "sent")
                .await
                .map_err(|e| ApiError::DatabaseError(e))?
                .ok_or_else(|| ApiError::Internal("no se pudo actualizar mensaje tras reintento".into()))?;

            let touch = crate::db::ConversationTouch {
                preview: &preview,
                msg_type: &existing_msg_type,
                direction: "out",
                wa_message_id: &new_wa_id,
                from_user_id: Some(claims.id.as_str()),
                media_filename: existing_media_filename.as_deref(),
                status: Some("sent"),
                increment_unread: false,
                last_message_at: None,
            };
            state.db
                .touch_conversation(&oid, touch)
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

    let preview_url_flag = payload.preview_url.unwrap_or(false);
    let (wa_id, msg_type, body, preview, tpl_fields, interactive_payload) = match &mode {
        SendMode::Text { content } => {
            let wa_id = wa.send_text(&conv.phone, content, payload.reply_to.as_deref(), preview_url_flag)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            (wa_id, "text".to_string(), Some(content.clone()), content.clone(), None, None)
        }
        SendMode::Template { tpl } => {
            let components_value = tpl.components.as_ref()
                .map(|v| serde_json::Value::Array(v.clone()));
            let wa_id = wa.send_template(
                    &conv.phone,
                    &tpl.name,
                    &tpl.language,
                    components_value.as_ref(),
                )
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let prev = template_preview(tpl);
            let body = tpl.rendered_text.clone().or_else(|| Some(prev.clone()));
            let fields = TemplateFields {
                name: tpl.name.clone(),
                language: tpl.language.clone(),
                components: tpl.components.as_ref().map(|v| serde_json::Value::Array(v.clone())),
            };
            (wa_id, "template".to_string(), body, prev, Some(fields), None)
        }
        SendMode::Interactive { payload: inter } => {
            let wa_id = wa.send_interactive(&conv.phone, inter, payload.reply_to.as_deref())
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let prev = interactive_preview(inter);
            // `body` guarda el texto humanizado (mismo valor que `preview`) para que
            // endpoints que listan sin `interactive_payload` sigan teniendo algo.
            (wa_id, "interactive".to_string(), Some(prev.clone()), prev, None, Some(inter.clone()))
        }
    };

    let is_text_mode = matches!(mode, SendMode::Text { .. });

    let msg = WaMessage {
        id: None,
        conversation_id: oid,
        wa_message_id: wa_id,
        direction: "out".to_string(),
        msg_type,
        body,
        media_id: None,
        media_mime_type: None,
        media_filename: None,
        status: Some("sent".to_string()),
        sent_by: Some(claims.id.clone()),
        idempotency_key: payload.idempotency_key.clone(),
        reply_to_wa_message_id: payload.reply_to.clone(),
        url_preview: None,
        voice: false,
        template_name: tpl_fields.as_ref().map(|f| f.name.clone()),
        template_language: tpl_fields.as_ref().map(|f| f.language.clone()),
        template_components: tpl_fields.and_then(|f| f.components),
        interactive_payload,
        contacts_payload: None,
        location: None,
        timestamp: DateTime::now(),
    };

    let saved = state.db.save_message(msg).await.map_err(|e| ApiError::DatabaseError(e))?;
    let touch = crate::db::ConversationTouch {
        preview: &preview,
        msg_type: &saved.msg_type,
        direction: "out",
        wa_message_id: &saved.wa_message_id,
        from_user_id: Some(claims.id.as_str()),
        media_filename: saved.media_filename.as_deref(),
        status: Some("sent"),
        increment_unread: false,
        last_message_at: None,
    };
    state.db.touch_conversation(&oid, touch).await.map_err(|e| ApiError::DatabaseError(e))?;

    // Si el mensaje nació de un quick-reply guardado, bumpear el contador de uso.
    // Best-effort: no bloquea la respuesta ni la afecta si falla.
    if let Some(qr_id) = payload.quick_reply_id.as_deref() {
        if let Ok(qr_oid) = ObjectId::parse_str(qr_id) {
            let db = state.db.clone();
            tokio::spawn(async move {
                if let Err(e) = db.increment_quick_reply_use(&qr_oid).await {
                    tracing::warn!("[send_message] increment_quick_reply_use falló id={}: {}", qr_oid, e);
                }
            });
        }
    }

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

    // URL preview sólo para texto: los templates no llevan URLs que el
    // usuario pueda escribir de forma libre.
    if is_text_mode {
        if let Some(msg_oid) = saved_oid {
            super::url_preview::spawn_preview_job(
                state.clone(),
                msg_oid,
                oid,
                preview.clone(),
            );
        }
    }

    Ok(Json(SendMessageResponse {
        ok: true,
        message_id: item.id.clone(),
        message: item,
    }))
}

/// Decide el modo de envío según el payload y la ventana de 24h.
enum SendMode {
    Text { content: String },
    Template { tpl: SendTemplatePayload },
    Interactive { payload: serde_json::Value },
}

struct TemplateFields {
    name: String,
    language: String,
    components: Option<serde_json::Value>,
}

fn resolve_send_mode(
    payload: &SendMessageRequest,
    conv: &WaConversation,
) -> Result<SendMode, ApiError> {
    // Activamos modo template si viene `type="template"` o si `template` está
    // presente. Ambos caminos requieren el objeto `template`.
    let template_mode = payload.msg_type.as_deref().map(|t| t.eq_ignore_ascii_case("template"))
        .unwrap_or(false)
        || payload.template.is_some();

    if template_mode {
        let tpl = payload.template.as_ref()
            .ok_or(ApiError::MissingTemplateParams)?;

        let name = tpl.name.trim();
        let language = tpl.language.trim();
        if name.is_empty() || language.is_empty() {
            return Err(ApiError::MissingTemplateParams);
        }
        return Ok(SendMode::Template { tpl: tpl.clone() });
    }

    // Interactive: requiere ventana de 24h abierta (igual que texto freeform).
    let interactive_mode = payload.msg_type.as_deref().map(|t| t.eq_ignore_ascii_case("interactive"))
        .unwrap_or(false)
        || payload.interactive.is_some();

    if interactive_mode {
        let inter = payload.interactive.as_ref().ok_or_else(|| ApiError::BadRequest(
            "interactive requerido cuando type=interactive".into(),
        ))?;

        if !is_within_24h(conv.last_inbound_at) {
            return Err(ApiError::WindowClosed);
        }
        return Ok(SendMode::Interactive { payload: inter.clone() });
    }

    let content = payload.content.as_deref().unwrap_or("").trim();
    if content.is_empty() {
        return Err(ApiError::BadRequest(
            "content requerido (o template para envíos fuera de 24h)".into(),
        ));
    }

    if !is_within_24h(conv.last_inbound_at) {
        return Err(ApiError::WindowExpired);
    }

    Ok(SendMode::Text { content: content.to_string() })
}

/// Resumen legible de un payload `interactive` para persistir como preview
/// y poblar el feed de "último mensaje" de la conversación. No hace
/// validación — si el payload viene mal armado, devuelve un fallback genérico.
fn interactive_preview(payload: &serde_json::Value) -> String {
    // Preferimos el texto del body, luego del header, luego un fallback.
    if let Some(b) = payload.get("body").and_then(|b| b.get("text")).and_then(|t| t.as_str()) {
        let t = b.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(h) = payload.get("header").and_then(|h| h.get("text")).and_then(|t| t.as_str()) {
        let t = h.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "[mensaje interactivo]".to_string()
}

fn template_preview(tpl: &SendTemplatePayload) -> String {
    if let Some(rendered) = tpl.rendered_text.as_deref() {
        let t = rendered.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    format!("[plantilla: {}]", tpl.name)
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
        (status = 200, description = "Conversación tomada o reasignada. Idempotente si ya era del agente. Acepta reasignación: si el chat es `pending` asignado a otro agente, lo transfiere al agente actual.", body = TakeConversationResponse),
        (status = 409, description = "La conversación no está en `pending` (está en `in_progress` o `closed`)"),
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

    let prev_owner = existing.assigned_to.clone();
    let was_already_mine = prev_owner.as_deref() == Some(claims.id.as_str());

    // `take_conversation` ahora permite reasignación: sólo exige `status=pending`.
    let taken = state.db
        .take_conversation(&oid, &claims.id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let conv = match taken {
        Some(c) => c,
        None => return Err(ApiError::Conflict("conversacion_no_pending".into())),
    };

    // Ajuste de carga: si había un dueño distinto a mí, le bajamos la carga.
    // Si yo no era dueño, me sube la carga.
    if !was_already_mine {
        state.redis.incr_agent_load(&claims.id).await;
        if let Some(prev) = prev_owner.as_deref() {
            if prev != claims.id {
                state.redis.decr_agent_load(prev).await;
            }
        }
    }

    // Resolver datos adicionales que van tanto en la respuesta HTTP como en el
    // evento WS (para que el resto de agentes vea la conversación actualizada).
    let opens = state.db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv).await;

    // Broadcast a los demás agentes:
    // - Si había otro dueño previo (reasignación): CHAT_TRANSFERIDO con
    //   conversación completa para que el front reemplace su estado local.
    // - Si no había dueño, o ya era mía y sólo es idempotente: CHAT_TOMADO.
    if !was_already_mine {
        let conv_item = conv_to_item(
            conv.clone(),
            true,
            last_opened,
            workspace_name.clone(),
            resolved.clone(),
            agent_name.clone(),
        );
        let ev = match prev_owner.as_deref() {
            Some(prev) if prev != claims.id => WsServerEvent::ChatTransferido {
                conversation_id: id.clone(),
                from_user_id: Some(prev.to_string()),
                to_user_id: claims.id.clone(),
                conversation: conv_item,
            },
            _ => WsServerEvent::ChatTomado {
                conversation_id: id.clone(),
                taken_by: claims.id.clone(),
                status: conv.status.clone(),
            },
        };
        broadcast_except(&state.ws_registry, &claims.id, &ev).await;
    }

    Ok(Json(TakeConversationResponse {
        ok: true,
        conversation: conv_to_item(conv, true, last_opened, workspace_name, resolved, agent_name),
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
    let resolved = resolve_customer_name(&state, &conv_after).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv_after).await;
    let conv_item = conv_to_item(conv_after, true, last_opened, workspace_name, resolved, agent_name);

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
    let resolved = resolve_customer_name(&state, &conv_after).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv_after).await;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        conversation: conv_to_item(conv_after, true, last_opened, workspace_name, resolved, agent_name),
    }))
}

// ============================================
// INICIAR CONVERSACIÓN (agent outbound first)
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/initiate",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = InitiateConversationRequest,
    responses(
        (status = 200, description = "Template enviado y conversación creada/reutilizada", body = SendMessageResponse),
        (status = 400, description = "Parámetros inválidos o template mal formado"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene permiso de chat o no pertenece al workspace"),
        (status = 404, description = "Workspace (business_phone_id) no encontrado"),
    )
)]
pub async fn initiate_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(payload): Json<InitiateConversationRequest>,
) -> Result<Json<SendMessageResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let workspace_oid = ObjectId::parse_str(payload.business_phone_id.trim())
        .map_err(|_| ApiError::BadRequest("business_phone_id inválido".into()))?;

    let settings = state.db
        .find_wa_settings_by_id(&workspace_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    if !settings.agents.iter().any(|a| a == &claims.id) {
        return Err(ApiError::Forbidden);
    }

    if !settings.active {
        return Err(ApiError::BadRequest("workspace inactivo".into()));
    }

    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        return Err(ApiError::BadRequest(
            "workspace sin phone_number_id o access_token configurados".into(),
        ));
    }

    let tpl = payload.template;
    let tpl_name = tpl.name.trim();
    let tpl_lang = tpl.language.trim();
    if tpl_name.is_empty() || tpl_lang.is_empty() {
        return Err(ApiError::MissingTemplateParams);
    }

    let idempotency_key = payload.idempotency_key.trim().to_string();
    if idempotency_key.is_empty() {
        return Err(ApiError::BadRequest("idempotency_key requerido".into()));
    }

    let to = normalize_to_e164(&payload.to);
    if to.is_empty() {
        return Err(ApiError::BadRequest("to inválido".into()));
    }

    // Linkear con cliente ISP si el teléfono matchea (best-effort — el link
    // sirve para mostrar datos del cliente en la UI, no bloquea el envío).
    let client_id = {
        use crate::db::ProfileRepository;
        state.db
            .find_clients_by_phone(&to)
            .await
            .ok()
            .and_then(|list| list.into_iter().next().map(|c| c._id))
    };

    // Upsert conversación. El nombre lo dejamos en None — si hay inbound
    // posterior, Meta lo trae y se actualiza automáticamente.
    let (conv, conv_created) = state.db
        .upsert_conversation(&to, &settings.phone, None)
        .await
        .map_err(ApiError::DatabaseError)?;
    let conv_id = conv.id
        .ok_or_else(|| ApiError::Internal("conversación sin _id tras upsert".into()))?;

    // Si se creó nueva y matcheó cliente, persistir el link. No reescribimos
    // client_id en conversaciones existentes para no pisar un link manual.
    if conv_created {
        if let Some(cid) = client_id {
            if let Err(e) = state.db.update_conversation_client_id(&conv_id, &cid).await {
                tracing::warn!("initiate: no se pudo vincular client_id: {}", e);
            }
        }
    }

    // Asignar al iniciador si la conversación no tiene dueño. Esto evita que
    // el auto-assign la reasigne a otro agente al primer inbound.
    let needs_assign = conv.assigned_to.is_none();
    if needs_assign {
        if let Err(e) = state.db.assign_conversation(&conv_id, Some(&claims.id)).await {
            tracing::warn!("initiate: assign_conversation error: {}", e);
        } else {
            state.redis.incr_agent_load(&claims.id).await;
        }
    }

    // Idempotencia: si ya existe un mensaje con la misma key para esta
    // conversación, devolverlo sin re-enviar (salvo que esté `failed`).
    if let Some(existing) = state.db
        .find_message_by_idempotency(&conv_id, &idempotency_key)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        let is_failed = existing.status.as_deref() == Some("failed");
        if !is_failed {
            let rto = resolve_reply_to_for_one(&state, &existing).await;
            let item = msg_to_item(existing, Some(claims.name.clone()), rto);
            return Ok(Json(SendMessageResponse {
                ok: true,
                message_id: item.id.clone(),
                message: item,
            }));
        }
    }

    // Descifrar access_token y construir el cliente Meta.
    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
    let wa = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id.clone(),
        token,
    );

    let components_value = tpl.components.as_ref()
        .map(|v| serde_json::Value::Array(v.clone()));
    let wa_id = wa.send_template(&to, tpl_name, tpl_lang, components_value.as_ref())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let preview = tpl.rendered_text.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("[plantilla: {}]", tpl_name));

    let msg = WaMessage {
        id: None,
        conversation_id: conv_id,
        wa_message_id: wa_id,
        direction: "out".to_string(),
        msg_type: "template".to_string(),
        body: Some(preview.clone()),
        media_id: None,
        media_mime_type: None,
        media_filename: None,
        status: Some("sent".to_string()),
        sent_by: Some(claims.id.clone()),
        idempotency_key: Some(idempotency_key),
        reply_to_wa_message_id: None,
        url_preview: None,
        voice: false,
        template_name: Some(tpl_name.to_string()),
        template_language: Some(tpl_lang.to_string()),
        template_components: components_value,
        interactive_payload: None,
        contacts_payload: None,
        location: None,
        timestamp: DateTime::now(),
    };

    let saved = state.db.save_message(msg).await.map_err(ApiError::DatabaseError)?;
    let touch = crate::db::ConversationTouch {
        preview: &preview,
        msg_type: &saved.msg_type,
        direction: "out",
        wa_message_id: &saved.wa_message_id,
        from_user_id: Some(claims.id.as_str()),
        media_filename: saved.media_filename.as_deref(),
        status: Some("sent"),
        increment_unread: false,
        last_message_at: None,
    };
    state.db.touch_conversation(&conv_id, touch)
        .await
        .map_err(ApiError::DatabaseError)?;

    // Releer para emitir `ConversacionNueva` con el estado final (assigned_to,
    // client_id, etc).
    let conv_now = state.db
        .find_conversation_by_id(&conv_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .unwrap_or(conv);

    let rto = resolve_reply_to_for_one(&state, &saved).await;
    let item = msg_to_item(saved, Some(claims.name.clone()), rto);

    if conv_created {
        let ws_name = Some(settings.workspace_name.clone()).filter(|w| !w.is_empty());
        let resolved = resolve_customer_name(&state, &conv_now).await;
        // El último mensaje es el template recién enviado por `claims`, así que
        // el nombre del agente sale del token — evitamos un round-trip a Users.
        let agent_name = Some(claims.name.clone());
        let new_ev = WsServerEvent::ConversacionNueva {
            conversation: conv_to_item(conv_now, false, None, ws_name, resolved, agent_name),
        };
        broadcast_all(&state.ws_registry, &new_ev).await;
    }

    let msg_ev = WsServerEvent::MensajeNuevo {
        conversation_id: conv_id.to_hex(),
        message: item.clone(),
    };
    broadcast_all(&state.ws_registry, &msg_ev).await;

    Ok(Json(SendMessageResponse {
        ok: true,
        message_id: item.id.clone(),
        message: item,
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
    let waba_id = payload.whatsapp_business_account_id.trim().to_string();
    if waba_id.is_empty() {
        return Err(ApiError::BadRequest("whatsapp_business_account_id requerido".into()));
    }

    let encrypted = encrypt_payload(&settings_secret(), access_token);

    let doc = WaSettings {
        id: None,
        phone,
        workspace_name: payload.workspace_name,
        phone_number_id: payload.phone_number_id,
        whatsapp_business_account_id: waba_id,
        access_token: encrypted,
        agents: payload.agents,
        active: true,
        purposes: payload.purposes.unwrap_or_default(),
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

    // WABA id: `Some("")` se ignora (permitir payloads sin borrar el campo).
    let waba = payload.whatsapp_business_account_id
        .and_then(|v| {
            let t = v.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });

    state.db
        .update_wa_settings(
            &oid,
            payload.workspace_name,
            payload.phone_number_id,
            waba,
            encrypted_token,
            payload.agents,
            payload.active,
            payload.purposes,
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

    // 3. Settings del negocio (auth del agente).
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

    // 4. Hot path: cache de Redis. Los media_id son inmutables, así que el
    // primero que haya abierto el media (o el prefetch del webhook) ya lo dejó.
    let t0 = std::time::Instant::now();
    if let Some((bytes, mime, remote_filename)) = state.redis.get_media_cache(&media_id).await {
        tracing::info!(
            "[media] HIT {} ({} bytes, {}) redis={}ms",
            media_id, bytes.len(), mime, t0.elapsed().as_millis()
        );
        let filename = msg.media_filename
            .clone()
            .or(remote_filename)
            .unwrap_or_else(|| media_id.clone());
        return Ok(build_media_response(bytes, &mime, &filename));
    }

    // 4.5. Miss + prefetch posiblemente en vuelo: si el lock ya está tomado,
    // hay otra tarea bajándolo. Esperamos ~2s en polls de 100ms a ver si
    // aparece en cache antes de disparar una segunda descarga al Worker.
    if !state.redis.try_lock_media_prefetch(&media_id).await {
        tracing::info!("[media] MISS→WAIT {} — prefetch en vuelo, esperando", media_id);
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Some((bytes, mime, remote_filename)) = state.redis.get_media_cache(&media_id).await {
                tracing::info!(
                    "[media] WAIT→HIT {} ({} bytes, {}) wait={}ms",
                    media_id, bytes.len(), mime, t0.elapsed().as_millis()
                );
                let filename = msg.media_filename
                    .clone()
                    .or(remote_filename)
                    .unwrap_or_else(|| media_id.clone());
                return Ok(build_media_response(bytes, &mime, &filename));
            }
        }
        // El otro task tardó demasiado o falló — seguimos con descarga propia.
        tracing::warn!("[media] MISS→WAIT timeout para {} — bajando por nuestra cuenta", media_id);
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

    // 5. Cache miss → descargar de Meta.
    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        return Err(ApiError::Internal(
            "wa_settings sin phone_number_id o access_token configurados".into(),
        ));
    }
    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
    let wa = apply_media_relay(
        &state,
        WhatsAppService::new(state.reqwest_client.clone(), settings.phone_number_id, token),
    );

    let t_meta = std::time::Instant::now();
    let (bytes, mime, remote_filename) = wa.download_media(&media_id)
        .await
        .map_err(|e| {
            tracing::error!(
                "[media] download_media falló para {} tras {}ms: {}",
                media_id, t_meta.elapsed().as_millis(), e
            );
            ApiError::Internal(e.to_string())
        })?;
    tracing::info!(
        "[media] MISS→FETCH {} ({} bytes, {}) meta={}ms",
        media_id, bytes.len(), mime, t_meta.elapsed().as_millis()
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
            state_cl.redis.set_media_cache(
                &mid_cl,
                &bytes_cl,
                &mime_cl,
                filename_cl.as_deref(),
            ).await;
        });
    }

    let filename = msg.media_filename
        .clone()
        .or(remote_filename)
        .unwrap_or_else(|| media_id.clone());
    Ok(build_media_response(bytes, &mime, &filename))
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

// ============================================
// QUICK REPLIES (mensajes rápidos)
// ============================================

#[derive(serde::Deserialize)]
pub struct QuickRepliesQuery {
    /// Hex de `WaSettings._id`. Si viene, filtra a ese workspace puntual
    /// (el agente debe pertenecer a él o devuelve lista vacía).
    pub workspace_id: Option<String>,
    /// Si viene, filtra por `active = bool`. Omitir para traer todos.
    pub active: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/quick-replies",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("workspace_id" = Option<String>, Query, description = "Filtrar por workspace puntual (hex de WaSettings._id)"),
        ("active" = Option<bool>, Query, description = "Filtrar por estado activo (true/false)"),
    ),
    responses(
        (status = 200, description = "Lista de snippets visibles para el agente", body = QuickRepliesListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene permiso de chat"),
    )
)]
pub async fn list_quick_replies_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<QuickRepliesQuery>,
) -> Result<Json<QuickRepliesListResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let workspaces = state.db.get_user_workspaces(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let filter_oid = match q.workspace_id.as_deref() {
        Some(hex) => Some(
            ObjectId::parse_str(hex)
                .map_err(|_| ApiError::BadRequest("workspace_id inválido".into()))?,
        ),
        None => None,
    };

    let docs = state.db
        .list_quick_replies(&workspaces, filter_oid.as_ref(), q.active)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(QuickRepliesListResponse {
        ok: true,
        data: docs.into_iter().map(quick_reply_to_item).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/quick-replies",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = CreateQuickReplyRequest,
    responses(
        (status = 200, description = "Snippet creado", body = QuickReplyResponse),
        (status = 400, description = "Validación falló"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene permiso de chat o no es agente en los workspaces indicados"),
    )
)]
pub async fn create_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(payload): Json<CreateQuickReplyRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    use super::quick_reply_validation::{validate_quick_reply, ValidatedQuickReply};

    require_can_chat(&state, &claims.id).await?;

    let title = payload.title.trim().to_string();
    let content = payload.content.trim().to_string();
    let workspace_oids = parse_and_validate_workspaces(&state, &claims.id, &payload.workspace_ids).await?;
    let footer = payload.footer.as_ref().map(|s| s.trim().to_string());

    validate_quick_reply(&ValidatedQuickReply {
        title: &title,
        content: &content,
        workspace_ids_len: workspace_oids.len(),
        header: payload.header.as_ref(),
        footer: footer.as_deref(),
        buttons: payload.buttons.as_deref(),
        list: payload.list.as_ref(),
        cta_url: payload.cta_url.as_ref(),
    })?;

    let now = DateTime::now();
    let doc = WaQuickReply {
        id: None,
        title,
        content,
        workspace_ids: workspace_oids,
        created_by: claims.id.clone(),
        created_by_name: claims.name.clone(),
        created_at: now,
        updated_at: now,
        active: payload.active.unwrap_or(true),
        header: payload.header,
        footer,
        buttons: payload.buttons,
        list: payload.list,
        cta_url: payload.cta_url,
        use_count: 0,
        last_used_at: None,
    };

    let saved = state.db.create_quick_reply(doc)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(saved),
    }))
}

#[utoipa::path(
    put,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet")),
    request_body = UpdateQuickReplyRequest,
    responses(
        (status = 200, description = "Snippet actualizado", body = QuickReplyResponse),
        (status = 400, description = "Validación falló"),
        (status = 404, description = "Snippet no encontrado o no visible para el agente"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn update_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateQuickReplyRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    use super::quick_reply_validation::{validate_quick_reply, ValidatedQuickReply};
    use crate::db::UpdateQuickReplyPatch;

    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let existing = state.db.find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let user_workspaces = state.db.get_user_workspaces(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !has_workspace_overlap(&existing.workspace_ids, &user_workspaces) {
        // No revelamos existencia del doc al user que no tiene acceso.
        return Err(ApiError::NotFound);
    }

    // Normalización + parse de campos planos
    let title_new = payload.title.as_ref().map(|t| t.trim().to_string());
    let content_new = payload.content.as_ref().map(|c| c.trim().to_string());
    let workspace_oids = match &payload.workspace_ids {
        Some(list) => Some(parse_and_validate_workspaces(&state, &claims.id, list).await?),
        None => None,
    };
    let footer_patch: Option<Option<String>> = payload.footer.as_ref().map(|opt| {
        opt.as_ref().map(|s| s.trim().to_string())
    });

    // Merge patch + existing → estado final, para validar el doc completo.
    let merged_title = title_new.clone().unwrap_or_else(|| existing.title.clone());
    let merged_content = content_new.clone().unwrap_or_else(|| existing.content.clone());
    let merged_ws_len = workspace_oids
        .as_ref()
        .map(|v| v.len())
        .unwrap_or(existing.workspace_ids.len());

    // Campos nullable: Some(Some) → nuevo valor, Some(None) → clear, None → mantener existente.
    let merged_header: Option<QuickReplyHeader> = match &payload.header {
        Some(Some(h)) => Some(h.clone()),
        Some(None) => None,
        None => existing.header.clone(),
    };
    let merged_footer: Option<String> = match &footer_patch {
        Some(Some(f)) => Some(f.clone()),
        Some(None) => None,
        None => existing.footer.clone(),
    };
    let merged_buttons: Option<Vec<QuickReplyButton>> = match &payload.buttons {
        Some(Some(b)) => Some(b.clone()),
        Some(None) => None,
        None => existing.buttons.clone(),
    };
    let merged_list: Option<QuickReplyList> = match &payload.list {
        Some(Some(l)) => Some(l.clone()),
        Some(None) => None,
        None => existing.list.clone(),
    };
    let merged_cta: Option<QuickReplyCtaUrl> = match &payload.cta_url {
        Some(Some(c)) => Some(c.clone()),
        Some(None) => None,
        None => existing.cta_url.clone(),
    };

    validate_quick_reply(&ValidatedQuickReply {
        title: &merged_title,
        content: &merged_content,
        workspace_ids_len: merged_ws_len,
        header: merged_header.as_ref(),
        footer: merged_footer.as_deref(),
        buttons: merged_buttons.as_deref(),
        list: merged_list.as_ref(),
        cta_url: merged_cta.as_ref(),
    })?;

    let patch = UpdateQuickReplyPatch {
        title: title_new,
        content: content_new,
        workspace_ids: workspace_oids,
        active: payload.active,
        header: payload.header,
        footer: footer_patch,
        buttons: payload.buttons,
        list: payload.list,
        cta_url: payload.cta_url,
    };

    let updated = state.db
        .update_quick_reply(&oid, patch)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(updated),
    }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet")),
    responses(
        (status = 200, description = "Snippet eliminado", body = UpdateResponse),
        (status = 404, description = "Snippet no encontrado o no visible para el agente"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn delete_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<UpdateResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let existing = state.db.find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let user_workspaces = state.db.get_user_workspaces(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !has_workspace_overlap(&existing.workspace_ids, &user_workspaces) {
        return Err(ApiError::NotFound);
    }

    let deleted = state.db.delete_quick_reply(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !deleted {
        return Err(ApiError::NotFound);
    }
    Ok(Json(UpdateResponse { ok: true }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}/active",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet")),
    request_body = ToggleActiveRequest,
    responses(
        (status = 200, description = "Estado actualizado", body = QuickReplyResponse),
        (status = 404, description = "Snippet no encontrado o no visible para el agente"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn set_quick_reply_active_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<ToggleActiveRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let existing = state.db.find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let user_workspaces = state.db.get_user_workspaces(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !has_workspace_overlap(&existing.workspace_ids, &user_workspaces) {
        return Err(ApiError::NotFound);
    }

    let updated = state.db.set_quick_reply_active(&oid, payload.active)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(updated),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}/duplicate",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet original")),
    request_body = DuplicateQuickReplyRequest,
    responses(
        (status = 200, description = "Snippet duplicado", body = QuickReplyResponse),
        (status = 400, description = "Validación falló"),
        (status = 404, description = "Snippet original no encontrado o no visible"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn duplicate_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<DuplicateQuickReplyRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let original = state.db.find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let user_workspaces = state.db.get_user_workspaces(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !has_workspace_overlap(&original.workspace_ids, &user_workspaces) {
        return Err(ApiError::NotFound);
    }

    let title = match payload.title.as_deref() {
        Some(t) => {
            let trimmed = t.trim().to_string();
            if trimmed.is_empty() || trimmed.chars().count() > 100 {
                return Err(ApiError::ValidationError {
                    field: "title".into(),
                    message: "El título debe tener entre 1 y 100 caracteres".into(),
                });
            }
            trimmed
        }
        None => {
            let proposed = format!("{} (copia)", original.title);
            // Truncar si supera 100 chars — nunca falla por el suffix.
            proposed.chars().take(100).collect::<String>()
        }
    };
    let workspace_oids = match payload.workspace_ids {
        Some(list) => parse_and_validate_workspaces(&state, &claims.id, &list).await?,
        None => original.workspace_ids.clone(),
    };

    let now = DateTime::now();
    let doc = WaQuickReply {
        id: None,
        title,
        content: original.content.clone(),
        workspace_ids: workspace_oids,
        created_by: claims.id.clone(),
        created_by_name: claims.name.clone(),
        created_at: now,
        updated_at: now,
        // La copia nace activa, con use_count en 0. El resto de campos
        // interactivos se heredan tal cual del original.
        active: true,
        header: original.header.clone(),
        footer: original.footer.clone(),
        buttons: original.buttons.clone(),
        list: original.list.clone(),
        cta_url: original.cta_url.clone(),
        use_count: 0,
        last_used_at: None,
    };

    let saved = state.db.create_quick_reply(doc)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(saved),
    }))
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

    let svc = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id,
        token,
    );
    Ok(apply_media_relay(&state, svc))
}

/// Aplica el relay de Cloudflare al service si ambas env vars están seteadas
/// en el Config. No-op cuando no hay relay configurado (dev, o red arreglada).
fn apply_media_relay(state: &Arc<AppState>, svc: WhatsAppService) -> WhatsAppService {
    match (
        state.config.wa_media_relay_url.as_ref(),
        state.config.wa_media_relay_secret.as_ref(),
    ) {
        (Some(url), Some(secret)) => svc.with_media_relay(super::service::MediaRelay {
            url: url.clone(),
            secret: secret.clone(),
        }),
        _ => svc,
    }
}

/// Tamaño máximo para el prefetch automático del webhook. Encima de esto el
/// binario se descarga on-demand en `get_media_handler` — evita inflar Redis
/// con PDFs/videos pesados que el agente podría ni abrir.
const MEDIA_PREFETCH_MAX_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

/// Tipos de mensaje que se prefetchean al llegar por webhook.
/// Todos los tipos con media están incluidos — documentos también, pero el
/// límite de 5 MB (`MEDIA_PREFETCH_MAX_BYTES`) deja fuera los PDFs pesados.
fn should_prefetch_media(msg_type: &str) -> bool {
    matches!(msg_type, "audio" | "image" | "sticker" | "video" | "document")
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
pub(crate) async fn prefetch_media(
    state: Arc<AppState>,
    business_phone: String,
    media_id: String,
) {
    // Skip si ya está cacheado (puede pasar si el mismo media llega dos veces).
    if state.redis.get_media_cache(&media_id).await.is_some() {
        return;
    }

    // Lock para evitar descarga duplicada: si el endpoint ya está bajando
    // este media (race con el agente que abre el chat al instante), lo dejamos.
    if !state.redis.try_lock_media_prefetch(&media_id).await {
        tracing::debug!("prefetch_media({}): ya hay otra tarea descargándolo", media_id);
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
            tracing::warn!("prefetch_media({}): no pude resolver service: {:?}", media_id, e);
            return;
        }
    };

    let info = match wa.download_media_info(&media_id).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("prefetch_media({}): info falló: {:?}", media_id, e);
            return;
        }
    };

    // Si Meta reporta tamaño y supera el límite, no cacheamos — lo bajará el
    // endpoint si el agente abre el media.
    if let Some(size) = info.file_size {
        if size > MEDIA_PREFETCH_MAX_BYTES {
            tracing::debug!(
                "prefetch_media({}): skip ({} bytes > {} max)",
                media_id, size, MEDIA_PREFETCH_MAX_BYTES
            );
            return;
        }
    }

    let bytes = match wa.download_media_body(&info.url).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("prefetch_media({}): body falló: {:?}", media_id, e);
            return;
        }
    };

    // Guard tardío: si Meta no reportó `file_size` y el binario terminó siendo
    // grande, igual respetamos el límite.
    if (bytes.len() as u64) > MEDIA_PREFETCH_MAX_BYTES {
        return;
    }

    state.redis.set_media_cache(&media_id, &bytes, &info.mime, info.file_name.as_deref()).await;
    tracing::info!(
        "prefetch_media({}): cacheado {} bytes ({})",
        media_id, bytes.len(), info.mime
    );
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
    resolved_name: Option<String>,
    last_message_from_user_name: Option<String>,
) -> ConversationItem {
    let (can_send_freeform, expires_iso) = compute_freeform_state(c.last_inbound_at);
    // Prioridad: DB (Clients.sName) → WhatsApp profile (c.name) → null
    let customer_name = resolved_name
        .filter(|s| !s.trim().is_empty())
        .or(c.name);
    ConversationItem {
        id: c.id.map(|o| o.to_hex()).unwrap_or_default(),
        customer_phone: c.phone,
        customer_name,
        business_phone: c.business_phone,
        workspace_name,
        status: c.status,
        assigned_to: c.assigned_to,
        last_message_at: iso8601(c.last_message_at),
        last_message_preview: c.last_message_preview,
        last_message_type: c.last_message_type,
        last_message_direction: c.last_message_direction,
        last_message_status: c.last_message_status,
        last_message_media_filename: c.last_message_media_filename,
        last_message_from_user_id: c.last_message_from_user_id,
        last_message_from_user_name,
        unread_count: c.unread_count,
        created_at: iso8601(c.created_at),
        client_id: if include_client_id { c.client_id.map(|o| o.to_hex()) } else { None },
        last_opened_at: last_opened_at.map(iso8601),
        last_inbound_at: c.last_inbound_at.map(iso8601),
        can_send_freeform,
        freeform_expires_at: expires_iso,
    }
}

/// Resuelve el nombre del contacto para una conversación contra `Clients`:
/// si tiene `client_id` linkeado lo usa; si no, intenta por teléfono. Devuelve
/// `None` cuando no matchea en DB — el caller cae a `WaConversation.name`.
async fn resolve_customer_name(state: &Arc<AppState>, conv: &WaConversation) -> Option<String> {
    use crate::db::ProfileRepository;
    if let Some(cid) = conv.client_id {
        let map = state.db.get_client_names_by_ids(&[cid]).await.ok()?;
        if let Some(n) = map.get(&cid).cloned() {
            return Some(n);
        }
    }
    let map = state.db
        .get_client_names_by_phones(&[conv.phone.clone()])
        .await
        .ok()?;
    map.get(&conv.phone).cloned()
}

/// Ventana de 24h desde `last_inbound_at`. Usado por el gate de envío freeform,
/// por `conv_to_item` y por el WS event `CONVERSACION_ESTADO`.
pub(super) fn is_within_24h(last_inbound_at: Option<DateTime>) -> bool {
    match last_inbound_at {
        Some(t) => {
            let now = DateTime::now().timestamp_millis();
            let then = t.timestamp_millis();
            (now - then) <= 24 * 60 * 60 * 1000
        }
        None => false,
    }
}

/// Devuelve `(can_send_freeform, freeform_expires_at_iso)`.
fn compute_freeform_state(last_inbound_at: Option<DateTime>) -> (bool, Option<String>) {
    match last_inbound_at {
        Some(t) => {
            let expires = DateTime::from_millis(t.timestamp_millis() + 24 * 60 * 60 * 1000);
            (is_within_24h(Some(t)), Some(iso8601(expires)))
        }
        None => (false, None),
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
        whatsapp_business_account_id: s.whatsapp_business_account_id,
        has_access_token: !s.access_token.is_empty(),
        agents: s.agents,
        active: s.active,
        purposes: s.purposes,
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
        template_name: m.template_name,
        template_language: m.template_language,
        template_components: m.template_components,
        interactive_payload: m.interactive_payload,
        contacts_payload: m.contacts_payload,
        location: m.location,
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

/// Batch-resolución de nombres de agentes para listados de conversaciones,
/// a partir de `last_message_from_user_id`. Dedup + 1 lookup por UUID único.
async fn resolve_last_message_agent_names(
    state: &Arc<AppState>,
    convs: &[WaConversation],
) -> std::collections::HashMap<String, String> {
    use crate::db::UserRepository;

    let mut ids: Vec<String> = convs
        .iter()
        .filter_map(|c| c.last_message_from_user_id.clone())
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

/// Resuelve el nombre del agente del último mensaje de una conversación
/// puntual (detalle). Devuelve `None` si no hay autor o no se encuentra.
async fn resolve_last_message_agent_name_one(
    state: &Arc<AppState>,
    conv: &WaConversation,
) -> Option<String> {
    use crate::db::UserRepository;
    let id = conv.last_message_from_user_id.as_deref()?;
    state.db.find_user_by_id(id).await.ok().flatten().map(|u| u.name)
}

// ============================================
// HELPERS — QUICK REPLIES
// ============================================

/// Exige `bCanChat == true`. El `user_jwt_auth_middleware` solo valida que el
/// token sea de staff, pero el permiso de chat es un campo extra en `Users`.
async fn require_can_chat(state: &Arc<AppState>, user_id: &str) -> Result<(), ApiError> {
    use crate::db::UserRepository;
    let user = state.db.find_user_by_id(user_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::Forbidden)?;
    if !user.can_chat {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// Parsea `workspace_ids` de hex → ObjectId, valida mínimo 1, existencia en
/// `WaSettings` y que el usuario sea agente en **todos** ellos.
async fn parse_and_validate_workspaces(
    state: &Arc<AppState>,
    user_id: &str,
    raw: &[String],
) -> Result<Vec<ObjectId>, ApiError> {
    if raw.is_empty() {
        return Err(ApiError::BadRequest("workspace_ids requiere al menos 1".into()));
    }
    let mut oids = Vec::with_capacity(raw.len());
    for s in raw {
        let oid = ObjectId::parse_str(s)
            .map_err(|_| ApiError::BadRequest(format!("workspace_id inválido: {}", s)))?;
        oids.push(oid);
    }
    oids.sort();
    oids.dedup();

    let user_workspaces = state.db.get_user_workspaces(user_id)
        .await
        .map_err(ApiError::DatabaseError)?;
    for w in &oids {
        if !user_workspaces.contains(w) {
            return Err(ApiError::Forbidden);
        }
    }
    // Sanity: cada id debe existir en WaSettings. (El check anterior ya lo
    // implica en la práctica, pero mantenemos la validación explícita.)
    if !state.db.wa_settings_exist(&oids).await.map_err(ApiError::DatabaseError)? {
        return Err(ApiError::BadRequest("algún workspace_id no existe".into()));
    }
    Ok(oids)
}

fn has_workspace_overlap(a: &[ObjectId], b: &[ObjectId]) -> bool {
    a.iter().any(|x| b.contains(x))
}

fn quick_reply_to_item(q: WaQuickReply) -> QuickReplyItem {
    QuickReplyItem {
        id: q.id.map(|o| o.to_hex()).unwrap_or_default(),
        title: q.title,
        content: q.content,
        workspace_ids: q.workspace_ids.into_iter().map(|o| o.to_hex()).collect(),
        created_by: q.created_by,
        created_by_name: q.created_by_name,
        created_at: iso8601(q.created_at),
        updated_at: iso8601(q.updated_at),
        active: q.active,
        header: q.header,
        footer: q.footer,
        buttons: q.buttons,
        list: q.list,
        cta_url: q.cta_url,
        use_count: q.use_count,
        last_used_at: q.last_used_at.map(iso8601),
    }
}

// ============================================
// TEMPLATES (Meta Cloud API)
// ============================================

#[derive(serde::Deserialize)]
pub struct TemplatesQuery {
    /// `phone_number_id` del workspace (lo identifica en Meta). El backend
    /// resuelve el WABA asociado para llamar a Meta.
    pub phone_number_id: String,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/templates",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("phone_number_id" = String, Query, description = "phone_number_id del workspace"),
    ),
    responses(
        (status = 200, description = "Templates APPROVED del WABA", body = TemplatesListResponse),
        (status = 400, description = "Parámetros inválidos o WABA no configurado"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene permiso de chat o no es agente del workspace"),
        (status = 404, description = "Workspace no encontrado"),
    )
)]
pub async fn list_templates_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<TemplatesQuery>,
) -> Result<Json<TemplatesListResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let phone_number_id = q.phone_number_id.trim();
    if phone_number_id.is_empty() {
        return Err(ApiError::BadRequest("phone_number_id requerido".into()));
    }

    let settings = state.db
        .find_wa_settings_by_phone_number_id(phone_number_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    if !settings.agents.iter().any(|a| a == &claims.id) {
        return Err(ApiError::Forbidden);
    }

    let waba_id = settings.whatsapp_business_account_id.trim().to_string();
    if waba_id.is_empty() {
        return Err(ApiError::BadRequest(
            "workspace sin whatsapp_business_account_id configurado".into(),
        ));
    }

    if let Some(cached) = state.redis.get_templates(&waba_id).await {
        return Ok(Json(TemplatesListResponse {
            ok: true,
            data: parse_templates(&cached),
        }));
    }

    if settings.access_token.is_empty() {
        return Err(ApiError::BadRequest(
            "workspace sin access_token configurado".into(),
        ));
    }
    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;

    let wa = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id.clone(),
        token,
    );

    let json = wa.list_templates(&waba_id).await.map_err(|e| {
        tracing::warn!("list_templates Meta error: {:#}", e);
        ApiError::Internal("no se pudieron obtener templates de Meta".into())
    })?;

    state.redis.set_templates(&waba_id, &json).await;

    Ok(Json(TemplatesListResponse {
        ok: true,
        data: parse_templates(&json),
    }))
}

/// Extrae templates APPROVED desde la respuesta de Meta y calcula
/// `body_placeholders` contando `{{N}}` únicos dentro del componente BODY.
fn parse_templates(raw: &serde_json::Value) -> Vec<WhatsAppTemplate> {
    let items = match raw.get("data").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };

    items.iter().filter_map(|t| {
        let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if !status.eq_ignore_ascii_case("APPROVED") {
            return None;
        }
        let name = t.get("name").and_then(|v| v.as_str())?.to_string();
        if !name.starts_with("sistema_abdo") {
            return None;
        }
        let language = t.get("language").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let category = t.get("category").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let components = t.get("components")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let body_placeholders = components.iter()
            .filter(|c| c.get("type").and_then(|v| v.as_str())
                .map(|s| s.eq_ignore_ascii_case("BODY"))
                .unwrap_or(false))
            .find_map(|c| c.get("text").and_then(|v| v.as_str()))
            .map(count_placeholders)
            .unwrap_or(0);

        Some(WhatsAppTemplate {
            name,
            language,
            category,
            status: status.to_string(),
            components,
            body_placeholders,
        })
    }).collect()
}

/// Cuenta placeholders únicos `{{1}}..{{N}}` en un string. Devuelve el máximo
/// índice encontrado (los placeholders en Meta son consecutivos).
fn count_placeholders(text: &str) -> u32 {
    let bytes = text.as_bytes();
    let mut max_idx: u32 = 0;
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let mut j = i + 2;
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start && j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                if let Ok(n) = std::str::from_utf8(&bytes[start..j]).unwrap_or("").parse::<u32>() {
                    if n > max_idx {
                        max_idx = n;
                    }
                }
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    max_idx
}
