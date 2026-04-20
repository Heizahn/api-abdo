use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// Almacena el último payload crudo recibido de Meta (solo para debug).
static LAST_WEBHOOK_PAYLOAD: OnceLock<Mutex<Option<serde_json::Value>>> = OnceLock::new();

fn last_payload_store() -> &'static Mutex<Option<serde_json::Value>> {
    LAST_WEBHOOK_PAYLOAD.get_or_init(|| Mutex::new(None))
}
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
    Json(payload): Json<WebhookPayload>,
) -> StatusCode {
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
                    // Verificar si el número está en la configuración de wa_settings
                    let settings = match state.db.find_wa_settings_by_phone(&msg.from).await {
                        Ok(Some(s)) => s,
                        _ => {
                            tracing::info!(
                                "[webhook] número no configurado: {} | tipo: {} | id: {}",
                                msg.from, msg.msg_type, msg.id
                            );
                            continue;
                        }
                    };

                    // Normalizar E.164 → formato local venezolano para buscar en Clients
                    let local_phone = wa_to_local_phone(&msg.from);

                    // Verificar si el remitente está registrado como cliente ISP
                    if state.db.find_customer_by_phone(&local_phone).await.is_none() {
                        tracing::info!(
                            "[webhook] número no registrado en clientes: {} (local: {}) | tipo: {} | body: {:?}",
                            msg.from, local_phone, msg.msg_type,
                            msg.text.as_ref().map(|t| &t.body)
                        );
                        continue;
                    }

                    let agents = settings.agents.clone();

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

    let doc = WaSettings {
        id: None,
        phone,
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
    state.db
        .update_wa_settings(&oid, payload.agents, payload.active)
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
// HELPERS INTERNOS
// ============================================

/// E.164 sin "+" → formato local venezolano ("04141234567")
fn wa_to_local_phone(wa_phone: &str) -> String {
    if let Some(rest) = wa_phone.strip_prefix("58") {
        format!("0{}", rest)
    } else {
        wa_phone.to_string()
    }
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

fn settings_to_item(s: WaSettings) -> SettingsItem {
    SettingsItem {
        id: s.id.map(|o| o.to_hex()).unwrap_or_default(),
        phone: s.phone,
        agents: s.agents,
        active: s.active,
        created_at: s.created_at.to_string(),
        updated_at: s.updated_at.to_string(),
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
