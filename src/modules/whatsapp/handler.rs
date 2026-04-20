use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use mongodb::bson::{oid::ObjectId, DateTime};
use std::sync::Arc;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::*,
    state::AppState,
};

use super::assignment::assign_conversation;

/// Número autorizado para soporte por ahora.
/// Mover a env var (WHATSAPP_SUPPORT_NUMBER) cuando se abra a más números.
const SUPPORT_NUMBER: &str = "584222236777";

use super::service::WhatsAppService;

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

/// POST /v1/webhook/whatsapp
/// Recibe notificaciones de Meta (mensajes entrantes + actualizaciones de estado).
/// Meta espera siempre HTTP 200 — cualquier otro código provoca reenvíos.
pub async fn receive_webhook(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<WebhookPayload>,
) -> StatusCode {
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

            // Procesar actualizaciones de estado (delivered / read)
            if let Some(statuses) = value.statuses {
                for s in statuses {
                    if let Err(e) = state.db.update_message_status(&s.id, &s.status).await {
                        tracing::warn!("update_message_status error: {}", e);
                    }
                }
            }

            // Procesar mensajes entrantes
            if let Some(messages) = value.messages {
                let contacts = value.contacts.unwrap_or_default();

                for msg in messages {
                    // Solo procesamos mensajes del número autorizado por ahora
                    if msg.from != SUPPORT_NUMBER {
                        tracing::info!(
                            "[webhook] número no autorizado: {} | tipo: {} | id: {}",
                            msg.from, msg.msg_type, msg.id
                        );
                        continue;
                    }

                    // Normalizar E.164 → formato local venezolano para buscar en Clients
                    let local_phone = wa_to_local_phone(&msg.from);

                    // Verificar si el número existe en la base de clientes ISP
                    if state.db.find_customer_by_phone(&local_phone).await.is_none() {
                        tracing::info!(
                            "[webhook] número no registrado en clientes: {} (local: {}) | tipo: {} | body: {:?}",
                            msg.from, local_phone, msg.msg_type,
                            msg.text.as_ref().map(|t| &t.body)
                        );
                        continue;
                    }

                    let name = contacts.iter()
                        .find(|c| c.wa_id.as_deref() == Some(&msg.from))
                        .and_then(|c| c.profile.as_ref())
                        .and_then(|p| p.name.clone());

                    // Upsert conversación
                    let conv = match state.db.upsert_conversation(&msg.from, name).await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!("upsert_conversation error: {}", e);
                            continue;
                        }
                    };
                    let conv_id = match conv.id {
                        Some(id) => id,
                        None => continue,
                    };

                    // Extraer contenido según tipo
                    let (body, media_id) = match msg.msg_type.as_str() {
                        "text" => (msg.text.map(|t| t.body), None),
                        "image" => {
                            let m = msg.image.unwrap_or(InboundMedia { id: None, caption: None });
                            (m.caption, m.id)
                        }
                        "document" => {
                            let m = msg.document.unwrap_or(InboundMedia { id: None, caption: None });
                            (m.caption, m.id)
                        }
                        "audio" => {
                            let m = msg.audio.unwrap_or(InboundMedia { id: None, caption: None });
                            (m.caption, m.id)
                        }
                        "video" => {
                            let m = msg.video.unwrap_or(InboundMedia { id: None, caption: None });
                            (m.caption, m.id)
                        }
                        _ => (None, None),
                    };

                    let preview = body.clone().unwrap_or_else(|| format!("[{}]", msg.msg_type));

                    tracing::info!(
                        "[webhook] guardando mensaje de cliente registrado: {} | tipo: {} | preview: {}",
                        msg.from, msg.msg_type, preview
                    );

                    let wa_msg = WaMessage {
                        id: None,
                        conversation_id: conv_id,
                        wa_message_id: msg.id.clone(),
                        direction: "inbound".to_string(),
                        msg_type: msg.msg_type.clone(),
                        body,
                        media_id,
                        status: None,
                        sent_by: None,
                        timestamp: DateTime::now(),
                    };

                    if let Err(e) = state.db.save_message(wa_msg).await {
                        tracing::error!("save_message error: {}", e);
                        continue;
                    }

                    if let Err(e) = state.db.touch_conversation(&conv_id, &preview, true).await {
                        tracing::warn!("touch_conversation error: {}", e);
                    }

                    // Marcar como leído en WhatsApp (ticks azules)
                    if let Ok(wa) = WhatsAppService::from_env(state.reqwest_client.clone()) {
                        let _ = wa.mark_as_read(&msg.id).await;
                    }

                    // Si la conversación no tiene agente asignado, disparar asignación automática
                    if conv.assigned_to.is_none() {
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            assign_conversation(state_clone, conv_id).await;
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
    pub page: Option<u64>,
    pub limit: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("status" = Option<String>, Query, description = "Filtrar por estado: open | closed | waiting"),
        ("assigned_to" = Option<String>, Query, description = "Filtrar por UUID de agente"),
        ("page" = Option<u64>, Query, description = "Página (default: 1)"),
        ("limit" = Option<i64>, Query, description = "Resultados por página (default: 20)"),
    ),
    responses(
        (status = 200, description = "Lista de conversaciones", body = ConversationsListResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_conversations_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ConversationsQuery>,
) -> Result<Json<ConversationsListResponse>, ApiError> {
    let page = q.page.unwrap_or(1).max(1);
    let limit = q.limit.unwrap_or(20).min(100);
    let skip = (page - 1) * limit as u64;

    let (convs, total) = state.db
        .get_conversations(
            q.status.as_deref(),
            q.assigned_to.as_deref(),
            skip,
            limit,
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let data = convs.into_iter().map(conv_to_list_item).collect();

    Ok(Json(ConversationsListResponse { ok: true, data, total }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/{id}/messages",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "ID de la conversación"),
        ("page" = Option<u64>, Query, description = "Página (default: 1)"),
        ("limit" = Option<i64>, Query, description = "Mensajes por página (default: 50)"),
    ),
    responses(
        (status = 200, description = "Detalle de conversación + mensajes", body = ConversationMessagesResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_conversation_messages_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<ConversationsQuery>,
) -> Result<Json<ConversationMessagesResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // Reset unread al abrir la conversación
    let _ = state.db.reset_unread(&oid).await;

    let page = q.page.unwrap_or(1).max(1);
    let limit = q.limit.unwrap_or(50).min(200);
    let skip = (page - 1) * limit as u64;

    let (messages, total) = state.db
        .get_messages(&oid, skip, limit)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    Ok(Json(ConversationMessagesResponse {
        ok: true,
        conversation: conv_to_detail(conv),
        messages: messages.into_iter().map(msg_to_item).collect(),
        total,
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

    let wa = WhatsAppService::from_env(state.reqwest_client.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let wa_id = wa.send_text(&conv.phone, &payload.body)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let msg = WaMessage {
        id: None,
        conversation_id: oid,
        wa_message_id: wa_id.clone(),
        direction: "outbound".to_string(),
        msg_type: "text".to_string(),
        body: Some(payload.body.clone()),
        media_id: None,
        status: Some("sent".to_string()),
        sent_by: Some(claims.id.clone()),
        timestamp: DateTime::now(),
    };

    state.db.save_message(msg).await.map_err(|e| ApiError::DatabaseError(e))?;
    state.db.touch_conversation(&oid, &payload.body, false).await.map_err(|e| ApiError::DatabaseError(e))?;

    Ok(Json(SendMessageResponse { ok: true, message_id: wa_id }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/conversations/{id}/status",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    request_body = UpdateConversationStatusRequest,
    responses(
        (status = 200, description = "Estado actualizado", body = UpdateResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn update_status_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateConversationStatusRequest>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let valid = ["open", "closed", "waiting"];
    if !valid.contains(&payload.status.as_str()) {
        return Err(ApiError::BadRequest("status debe ser open | closed | waiting".into()));
    }

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    state.db.update_conversation_status(&oid, &payload.status)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    Ok(Json(UpdateResponse { ok: true }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/conversations/{id}/assign",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    request_body = AssignConversationRequest,
    responses(
        (status = 200, description = "Conversación asignada", body = UpdateResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn assign_conversation_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<AssignConversationRequest>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    state.db.assign_conversation(&oid, payload.assigned_to.as_deref())
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    Ok(Json(UpdateResponse { ok: true }))
}

// ============================================
// HELPERS INTERNOS
// ============================================

/// Convierte número WhatsApp E.164 sin "+" (ej: "584141234567") al formato local venezolano ("04141234567").
fn wa_to_local_phone(wa_phone: &str) -> String {
    if let Some(rest) = wa_phone.strip_prefix("58") {
        format!("0{}", rest)
    } else {
        wa_phone.to_string()
    }
}

// ============================================
// HELPERS DE MAPEO
// ============================================

fn conv_to_list_item(c: WaConversation) -> ConversationListItem {
    ConversationListItem {
        id: c.id.map(|o| o.to_hex()).unwrap_or_default(),
        phone: c.phone,
        name: c.name,
        status: c.status,
        assigned_to: c.assigned_to,
        last_message_at: c.last_message_at.to_string(),
        last_message_preview: c.last_message_preview,
        unread_count: c.unread_count,
    }
}

fn conv_to_detail(c: WaConversation) -> ConversationDetail {
    ConversationDetail {
        id: c.id.map(|o| o.to_hex()).unwrap_or_default(),
        phone: c.phone,
        name: c.name,
        client_id: c.client_id.map(|o| o.to_hex()),
        status: c.status,
        assigned_to: c.assigned_to,
        last_message_at: c.last_message_at.to_string(),
        unread_count: c.unread_count,
    }
}

fn msg_to_item(m: WaMessage) -> MessageItem {
    MessageItem {
        id: m.id.map(|o| o.to_hex()).unwrap_or_default(),
        wa_message_id: m.wa_message_id,
        direction: m.direction,
        msg_type: m.msg_type,
        body: m.body,
        status: m.status,
        sent_by: m.sent_by,
        timestamp: m.timestamp.to_string(),
    }
}
