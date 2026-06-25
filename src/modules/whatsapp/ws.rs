use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::{
    auth::{
        http_auth::{compat_ws_query_enabled, read_staff_access_token},
        user_jwt::UserJwtService,
    },
    db::{SalesRepository, UserRepository, WhatsAppRepository},
    models::whatsapp::{ConversationItem, MessageItem, TicketItem},
    state::{AppState, WsRegistry},
};

const WS_OUTBOX_CAPACITY: usize = 512;
const NO_ACCESS_ROLE: f32 = -1.0;
const SUPERADMIN_ROLE: f32 = 0.0;
const ACCOUNTING_ROLE: f32 = 1.0;
const ACCOUNTING_MESSAGING_ROLE: f32 = 1.5;

// ============================================
// TIPOS DE EVENTOS
// ============================================

#[derive(Debug, Deserialize)]
#[serde(tag = "tipo", content = "datos")]
pub enum WsClientEvent {
    /// Handshake opcional que el front envía tras conectar.
    #[serde(rename = "CONECTAR")]
    Conectar { usuario_id: String, nombre: String },

    /// (Opcional) Marca una conversación como "activa" para este socket.
    /// Hoy es no-op: el backend emite por broadcast y el front filtra.
    /// Lo dejamos aceptado para compatibilidad futura.
    #[serde(rename = "SUSCRIBIR_CONVERSACION")]
    SuscribirConversacion { conversation_id: String },
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "tipo", content = "datos")]
pub enum WsServerEvent {
    /// Mensaje nuevo en una conversación existente.
    #[serde(rename = "MENSAJE_NUEVO")]
    MensajeNuevo {
        conversation_id: String,
        message: MessageItem,
    },

    /// Se creó una conversación nueva (primer mensaje inbound de ese `(customer_phone, business_phone)`).
    #[serde(rename = "CONVERSACION_NUEVA")]
    ConversacionNueva { conversation: ConversationItem },

    /// Un agente tomó una conversación pendiente o cerrada.
    #[serde(rename = "CHAT_TOMADO")]
    ChatTomado {
        conversation_id: String,
        taken_by: String,
        /// Nombre del agente que tomó la conv (resuelto contra `Users.sName`).
        /// `null` solo si el usuario fue borrado entre el take y el broadcast
        /// (caso rarísimo). Sin este campo el front no puede patchear la
        /// sidebar y la lista muestra el nombre del agente anterior.
        #[serde(skip_serializing_if = "Option::is_none")]
        taken_by_name: Option<String>,
        status: String,
        /// Estado previo al take: `"pending"` (toma normal) o `"closed"` (reopen+take vía template).
        previous_status: String,
    },

    /// Transferencia entre agentes. `from_user_id` puede ser null si la conversación no tenía dueño.
    #[serde(rename = "CHAT_TRANSFERIDO")]
    ChatTransferido {
        conversation_id: String,
        from_user_id: Option<String>,
        to_user_id: String,
        /// Estado actualizado de la conversación (incluye `workspace_name` y `assigned_to` nuevo).
        conversation: ConversationItem,
    },

    /// Conversación cerrada.
    #[serde(rename = "CHAT_CERRADO")]
    ChatCerrado { conversation_id: String },

    /// Conversación reabierta (closed → pending). Incluye el item completo
    /// para que el front mergee sin refetch — `assigned_to` queda `null`
    /// porque al cerrar se libera al agente.
    #[serde(rename = "CHAT_REABIERTO")]
    ChatReabierto {
        conversation_id: String,
        conversation: ConversationItem,
    },

    /// Cambio de status de un mensaje (sent/delivered/read/failed).
    #[serde(rename = "MENSAJE_ACTUALIZADO")]
    MensajeActualizado {
        conversation_id: String,
        message_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta_error_code: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta_error_title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta_error_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta_error_details: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        failed_at: Option<String>,
    },

    /// Mensaje existente mutado (edición o revocación).
    /// Se emite cuando el webhook trae `edit`/`revoke` con `context.id` válido.
    #[serde(rename = "MENSAJE_MODIFICADO")]
    MensajeModificado {
        conversation_id: String,
        message: MessageItem,
        /// Tipo de delta aplicado: "edit" o "revoke".
        change_type: String,
    },

    /// Preview de URL listo para un mensaje ya existente. El front debe
    /// mergear `message.url_preview` en la burbuja que ya tiene, sin crear
    /// duplicado (el `id` coincide con el `MessageItem` ya entregado vía
    /// `MENSAJE_NUEVO` o el response HTTP de envío).
    #[serde(rename = "URL_PREVIEW_READY")]
    UrlPreviewReady {
        conversation_id: String,
        message: MessageItem,
    },

    /// Batch de inbound marcados como leídos por el agente (visto en la UI).
    /// `message_ids` son los `wa_message_id` (los mismos que llegan en `MENSAJE_ACTUALIZADO`).
    #[serde(rename = "MENSAJES_VISTOS")]
    MensajesVistos {
        conversation_id: String,
        message_ids: Vec<String>,
        status: String,
    },

    /// Cambio de estado de una conversación (pending → in_progress, etc).
    /// Se emite cuando el primer `GET /messages` del agente asignado dispara la transición.
    #[serde(rename = "CHAT_ESTADO_CAMBIO")]
    ChatEstadoCambio {
        conversation_id: String,
        new_status: String,
    },

    /// Cambio en el estado de la ventana de 24h (freeform) de una conversación.
    /// Se emite al recibir un inbound (reinicia la ventana) y al expirar las 24h
    /// sin inbound. El front mergea los campos en la conversación por `conversation_id`.
    #[serde(rename = "CONVERSACION_ESTADO")]
    ConversacionEstado {
        conversation_id: String,
        last_inbound_at: Option<String>,
        can_send_freeform: bool,
        freeform_expires_at: Option<String>,
        /// `true` cuando Meta está rate-limitando con error 131049. El front
        /// debe deshabilitar el envío y mostrar el aviso correspondiente.
        meta_throttled: bool,
        /// ISO-8601 hasta cuándo dura el cooldown. `null` si no aplica.
        meta_throttle_until: Option<String>,
    },

    /// Push personal al agente destino cuando un ticket queda asignado a él.
    /// Scope: sólo `assigned_to_id` — no broadcast.
    #[serde(rename = "TICKET_ASIGNADO")]
    TicketAsignado {
        ticket: TicketItem,
        assigned_by_name: String,
    },

    /// Cambio de estado o asignación de un ticket existente. Scope:
    /// creador + asignado actual + SUPERADMIN. Se emite por `send_to_user`
    /// a cada destinatario (no broadcast global).
    #[serde(rename = "TICKET_ACTUALIZADO")]
    TicketActualizado {
        ticket: TicketItem,
        previous_status: String,
        changed_by_name: String,
    },

    /// La IA quedó pausada para esta conversación (un humano la atiende).
    /// Lo dispara la tool `request_human` o, en una iteración futura, un
    /// "take" manual desde la UI.
    ///
    /// `reason` es uno de: `"request_human"`, `"support_handoff"`,
    /// `"sales_handoff"`, `"urgent_reactivation_handoff"`,
    /// `"transfer_to_agent_failed"`, `"manual"`, etc. `by` es `"ai_agent"`
    /// cuando el origen fue una tool del loop, o el UUID del usuario que pausó
    /// manualmente.
    #[serde(rename = "IA_PAUSADA")]
    IaPausada {
        conversation_id: String,
        reason: String,
        by: String,
    },

    /// La IA volvió a atender esta conversación (transferencia entre
    /// agentes IA, o reactivación manual). Si la transición la dispara
    /// `transfer_to_agent`, `to_agent_id` es el agente IA destino. En
    /// reactivación manual viene `null`.
    #[serde(rename = "IA_REACTIVADA")]
    IaReactivada {
        conversation_id: String,
        reason: String,
        by: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        to_agent_id: Option<String>,
    },

    /// La IA procesó uno o varios inbounds de esta conversación. NO equivale
    /// a "leído por humano" (`unread_count` no cambia). Permite al front
    /// pintar un indicador 🤖 en cada mensaje del burst y mostrar
    /// "IA respondió hace X" en el listado.
    #[serde(rename = "IA_PROCESO_MENSAJE")]
    IaProcesoMensaje {
        conversation_id: String,
        /// `_id` (hex) de cada inbound del burst que la IA procesó.
        message_ids: Vec<String>,
        /// ISO-8601 UTC.
        ai_processed_at: String,
    },

    /// El estado IA de una conversación cambió (o fue limpiado).
    /// Se emite tras cada dispatch que modifique `aiConvState` y tras un reset manual.
    /// El front mergea los campos en la conversación por `conversation_id`.
    /// Cuando `ai_conv_state` es `null`, el estado fue borrado (reopen / reset manual).
    /// El campo `ai_conv_state` SIEMPRE se serializa (sin skip_serializing_if) —
    /// en clear events va explícito como `"ai_conv_state": null`, según contrato del front.
    #[serde(rename = "CONVERSACION_ESTADO_IA")]
    ConversacionEstadoIa {
        conversation_id: String,
        ai_conv_state: Option<serde_json::Value>,
    },

    /// Reacción aplicada o removida sobre un mensaje existente.
    /// `emoji = ""` significa que la reacción fue removida desde ese lado.
    /// `sender_name` viene sólo cuando `from = "agent"` (claims.name); en
    /// reacciones del cliente es `None`.
    #[serde(rename = "REACCION_MENSAJE")]
    ReaccionMensaje {
        conversation_id: String,
        /// ObjectId hex del `WaMessage` actualizado.
        message_id: String,
        /// `wa_message_id` (wamid…) del mismo mensaje.
        wa_message_id: String,
        /// Emoji crudo o `""` para remoción.
        emoji: String,
        /// `"customer"` | `"agent"`.
        from: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sender_name: Option<String>,
    },

    #[serde(rename = "ERROR")]
    Error { error: String },

    /// Confirmación de conexión tras el upgrade WebSocket.
    #[serde(rename = "CONECTADO")]
    Conectado { usuario_id: String },

    /// Snapshot de badges enviado al conectar. Contiene los 3 contadores
    /// según el rol y flags del usuario. Campos sin acceso llegan como 0.
    #[serde(rename = "BADGES_SNAPSHOT")]
    BadgesSnapshot { data: BadgesSnapshotData },

    /// Evento reactivo: cambio en el conteo de reportes de pago pendientes.
    /// Audience: nRole ∈ {0.0, 1.0, 1.5}. Fan-out vía `broadcast_to_roles`.
    ///
    /// Emit sites (// EMIT BADGE: REPORTE_PAGO_PENDIENTE):
    /// - payments/handler.rs::report_payment_handler (client create)
    /// - payments/handler.rs::report_payment_user_handler (staff create)
    /// - ai_agent/tools.rs report_payment tool
    /// - payments/handler.rs::approve_payment_report_handler (NEW)
    /// - payments/handler.rs::reject_payment_report_handler (NEW)
    #[serde(rename = "REPORTE_PAGO_PENDIENTE")]
    ReportePagoPendiente { data: ReportePagoPendienteData },

    /// Evento reactivo: cambio en el conteo de conversaciones con mensajes no leídos.
    /// Audience: bCanChat == true OR nRole == 0 (superadmin). Fan-out vía `broadcast_to_chat_users`.
    ///
    /// Emit sites (// EMIT BADGE: CONVERSACION_NO_LEIDA):
    /// - whatsapp/handler.rs (after touch_conversation(increment_unread=true))
    /// - whatsapp/handler.rs mark_read_handler (after reset_unread)
    #[serde(rename = "CONVERSACION_NO_LEIDA")]
    ConversacionNoLeida { data: ConversacionNoLeidaData },

    /// Evento reactivo: cambio en el conteo de tickets con status "open".
    /// Audience: bCanChat == true OR nRole == 0 (superadmin). Fan-out vía `broadcast_to_chat_users`.
    ///
    /// Emit sites (// EMIT BADGE: TICKET_PENDIENTE):
    /// - whatsapp/tickets.rs::create_ticket_handler (after insert)
    /// - whatsapp/tickets.rs::transfer_and_ticket_handler (after insert)
    /// - whatsapp/tickets.rs::update_ticket_handler (only when crossing open boundary)
    /// - ai_agent/tools.rs ticket tool (if exists)
    #[serde(rename = "TICKET_PENDIENTE")]
    TicketPendiente { data: TicketPendienteData },
}

// ============================================
// DATA STRUCTS PARA BADGE EVENTS
// ============================================

/// Payload para BADGES_SNAPSHOT — snapshot completo de los 3 inboxes al conectar.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub struct BadgesSnapshotData {
    pub payment_reports_pending: u64,
    pub wa_conversations_unread: u64,
    pub wa_tickets_open: u64,
}

/// Payload para REPORTE_PAGO_PENDIENTE — delta tras cualquier cambio de estado de un reporte.
/// `previous_state` es None en inserts nuevos.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub struct ReportePagoPendienteData {
    pub pending_total: u64,
    pub report_id: String,
    pub previous_state: Option<String>,
    pub new_state: String,
}

/// Payload para CONVERSACION_NO_LEIDA — delta tras touch_conversation o reset_unread.
/// `delta` es +1 cuando la conversación pasa de 0→1 mensajes no leídos,
/// -1 cuando pasa de N→0 (mark-read). `pending_total` es siempre autoritativo.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub struct ConversacionNoLeidaData {
    pub pending_total: u64,
    pub conversation_id: String,
    pub delta: i32,
}

/// Payload para TICKET_PENDIENTE — delta tras cualquier transición que cruce el estado "open".
/// `previous_status` es None en tickets recién creados.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub struct TicketPendienteData {
    pub pending_total: u64,
    pub ticket_id: String,
    pub previous_status: Option<String>,
    pub new_status: String,
}

// ============================================
// HELPERS DE BROADCAST
// ============================================

/// Envía un evento a un agente específico.
pub async fn send_to_agent(registry: &WsRegistry, agent_id: &str, event: &WsServerEvent) {
    let json = match serde_json::to_string(event) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("[ws] serialize error: {}", e);
            return;
        }
    };
    let sender = {
        let registry = registry.read().await;
        registry.get(agent_id).cloned()
    };
    if let Some(sender) = sender {
        let _ = sender.try_send(json);
    }
}

/// Envía un payload JSON (string) a un agente específico.
/// Drop silencioso si el agente no está conectado.
pub async fn send_to_user(registry: &WsRegistry, user_id: &str, payload: String) {
    let sender = {
        let registry = registry.read().await;
        registry.get(user_id).cloned()
    };
    if let Some(sender) = sender {
        let _ = sender.try_send(payload);
    }
}

/// Resuelve los agentes del `phone_number_id` desde `WaSettings.agents` y emite
/// el `payload` JSON a cada uno via `WsRegistry::send_to_user`. Drop silencioso
/// si el agente no está conectado (mismo comportamiento que mensajes).
///
/// `payload` ya viene serializado como `String` (JSON con `tipo` + `datos`).
pub async fn emit_to_phone_number_agents(
    state: &Arc<AppState>,
    phone_number_id: &str,
    payload: String,
) {
    // Resolver WaSettings via state.db
    let settings = match state
        .db
        .find_wa_settings_by_phone_number_id(phone_number_id)
        .await
    {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!(
                "[ws] WaSettings not found for phone_number_id: {}",
                phone_number_id
            );
            return;
        }
        Err(e) => {
            tracing::error!("[ws] error finding WaSettings: {}", e);
            return;
        }
    };

    // Emitir a cada agente en settings.agents
    for agent_id in &settings.agents {
        send_to_user(&state.ws_registry, agent_id, payload.clone()).await;
    }
}

/// Broadcast de un evento a todos los agentes conectados.
pub async fn broadcast_all(registry: &WsRegistry, event: &WsServerEvent) {
    let json = match serde_json::to_string(event) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("[ws] serialize error: {}", e);
            return;
        }
    };
    let registry = registry.read().await;
    for sender in registry.values() {
        let _ = sender.try_send(json.clone());
    }
}

/// Broadcast un payload JSON pre-serializado a todos los usuarios cuyo `nRole` ∈ `roles`.
///
/// Filtros DB: visible == true, bIsBot != true. Dedupes por user_id.
/// Best-effort: fallas de envío por usuario son silenciosas (usuario desconectado).
///
/// Callsites (// EMIT BADGE: REPORTE_PAGO_PENDIENTE):
/// - payments/handler.rs::report_payment_handler (client create)
/// - payments/handler.rs::report_payment_user_handler (staff create)
/// - ai_agent/tools.rs report_payment tool
/// - payments/handler.rs::approve_payment_report_handler (NEW)
/// - payments/handler.rs::reject_payment_report_handler (NEW)
pub async fn broadcast_to_roles(
    state: &Arc<AppState>,
    roles: &[f32],
    payload: String,
) -> Result<(), String> {
    let user_ids = state
        .db
        .find_users_by_roles(roles)
        .await
        .map_err(|e| format!("find_users_by_roles failed: {}", e))?;
    // Dedup: the DB query is already filtered but collect into HashSet for safety.
    let unique: std::collections::HashSet<String> = user_ids.into_iter().collect();
    for uid in unique {
        send_to_user(&state.ws_registry, &uid, payload.clone()).await;
    }
    Ok(())
}

/// Broadcast un payload JSON pre-serializado a todos los usuarios con acceso al inbox WA:
/// `bCanChat == true` o `nRole == 0` (superadmin).
///
/// Filtros DB: visible == true, bIsBot != true.
/// Best-effort: fallas de envío por usuario son silenciosas.
///
/// Callsites (// EMIT BADGE: CONVERSACION_NO_LEIDA):
/// - whatsapp/handler.rs (after touch_conversation(increment_unread=true))
/// - whatsapp/handler.rs mark_read_handler (after reset_unread)
///
/// Callsites (// EMIT BADGE: TICKET_PENDIENTE):
/// - whatsapp/tickets.rs::create_ticket_handler
/// - whatsapp/tickets.rs::transfer_and_ticket_handler
/// - whatsapp/tickets.rs::update_ticket_handler (crossing open boundary only)
/// - ai_agent/tools.rs ticket tool (if exists)
pub async fn broadcast_to_chat_users(state: &Arc<AppState>, payload: String) -> Result<(), String> {
    let user_ids = state
        .db
        .find_chat_user_ids()
        .await
        .map_err(|e| format!("find_chat_user_ids failed: {}", e))?;
    let unique: std::collections::HashSet<String> = user_ids.into_iter().collect();
    for uid in unique {
        send_to_user(&state.ws_registry, &uid, payload.clone()).await;
    }
    Ok(())
}

/// Broadcast a todos los agentes conectados excepto el indicado.
/// Útil para eventos como CHAT_TOMADO, donde el que tomó ya tiene la respuesta HTTP.
pub async fn broadcast_except(registry: &WsRegistry, skip_agent_id: &str, event: &WsServerEvent) {
    let json = match serde_json::to_string(event) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("[ws] serialize error: {}", e);
            return;
        }
    };
    let registry = registry.read().await;
    for (agent_id, sender) in registry.iter() {
        if agent_id == skip_agent_id {
            continue;
        }
        let _ = sender.try_send(json.clone());
    }
}

// ============================================
// HANDLER DE CONEXIÓN WEBSOCKET
// ============================================

#[derive(Deserialize)]
pub struct WsConnectParams {
    token: Option<String>,
}

/// GET /v1/ws/chat
/// Upgrade a WebSocket. Auth primaria vía cookie HttpOnly.
/// `?token=` se acepta sólo en ventana de compatibilidad temporal.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Option<Query<WsConnectParams>>,
) -> Response {
    let cookie_token = read_staff_access_token(&headers);
    let query_token = if compat_ws_query_enabled(&state.config) {
        query.and_then(|q| q.0.token)
    } else {
        None
    };

    let Some(token) = cookie_token.or(query_token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    // Validar JWT antes del upgrade.
    let jwt = UserJwtService::new();
    let claims = match jwt.verify_token(&token) {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    // Gate explícito de acceso WS (panel interno):
    // - usuario visible y no bot
    // - rol válido (nRole != -1)
    // - acceso por chat (bCanChat) o rol interno elegible para panel (0/1/1.5)
    let user = match state.db.find_user_by_id(&claims.id).await {
        Ok(Some(u)) => u,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let role_eligible = user.role == SUPERADMIN_ROLE
        || user.role == ACCOUNTING_ROLE
        || user.role == ACCOUNTING_MESSAGING_ROLE;
    let ws_eligible = user.can_chat || role_eligible;
    if !user.visible || user.is_bot || user.role == NO_ACCESS_ROLE || !ws_eligible {
        return StatusCode::FORBIDDEN.into_response();
    }

    ws.on_upgrade(move |socket| handle_socket(socket, state, claims.id, claims.name))
        .into_response()
}

async fn handle_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    user_id: String,
    user_name: String,
) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(WS_OUTBOX_CAPACITY);

    // Registrar en WsRegistry
    {
        let mut registry = state.ws_registry.write().await;
        registry.insert(user_id.clone(), tx);
    }
    tracing::debug!("[ws] agente conectado: {} ({})", user_name, user_id);

    // Confirmar conexión
    let connected_msg = serde_json::to_string(&WsServerEvent::Conectado {
        usuario_id: user_id.clone(),
    })
    .unwrap_or_default();
    let _ = sink.send(Message::Text(connected_msg)).await;

    // Emitir BADGES_SNAPSHOT justo después de CONECTADO, antes del send_task.
    // Enviamos directo al sink (lo poseemos aún) para evitar cualquier race con el relay channel.
    // nRole == 0.0, 1.0, 1.5 son exactos en f32 (potencias/sumas de potencias de 2).
    {
        let (role_opt, can_chat) = state
            .db
            .get_user_role_and_can_chat(&user_id)
            .await
            .unwrap_or((None, false));

        let role_eligible =
            matches!(role_opt, Some(r) if r == 0.0_f32 || r == 1.0_f32 || r == 1.5_f32);
        let wa_eligible = can_chat || matches!(role_opt, Some(r) if r == 0.0_f32);

        let (reports, unread, tickets) = tokio::join!(
            async {
                if role_eligible {
                    state.db.count_pending_reports().await.unwrap_or(0)
                } else {
                    0
                }
            },
            async {
                if wa_eligible {
                    state.db.count_unread_conversations().await.unwrap_or(0)
                } else {
                    0
                }
            },
            async {
                if wa_eligible {
                    state.db.count_open_tickets().await.unwrap_or(0)
                } else {
                    0
                }
            },
        );

        let snapshot = WsServerEvent::BadgesSnapshot {
            data: BadgesSnapshotData {
                payment_reports_pending: reports,
                wa_conversations_unread: unread,
                wa_tickets_open: tickets,
            },
        };
        match serde_json::to_string(&snapshot) {
            Ok(payload) => {
                let _ = sink.send(Message::Text(payload)).await;
            }
            Err(e) => {
                tracing::error!("[ws] serialize BADGES_SNAPSHOT: {}", e);
            }
        }
    }

    // Task: reenviar mensajes del canal al socket
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Loop principal: escuchar mensajes del frontend
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => {
                handle_client_message(&user_id, &user_name, &text).await;
            }
            Message::Close(_) => break,
            Message::Ping(data) => {
                // Pong es manejado automáticamente por axum
                let _ = data;
            }
            _ => {}
        }
    }

    // Desconexión: limpiar registro
    {
        let mut registry = state.ws_registry.write().await;
        registry.remove(&user_id);
    }
    send_task.abort();
    tracing::debug!("[ws] agente desconectado: {} ({})", user_name, user_id);
}

async fn handle_client_message(user_id: &str, user_name: &str, text: &str) {
    let event: WsClientEvent = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(_) => {
            tracing::warn!("[ws] mensaje inválido de {}: {}", user_id, text);
            return;
        }
    };

    match event {
        WsClientEvent::Conectar { usuario_id, nombre } => {
            tracing::debug!("[ws] CONECTAR recibido de {} ({})", nombre, usuario_id);
            let _ = user_name;
        }

        WsClientEvent::SuscribirConversacion { conversation_id } => {
            tracing::debug!(
                "[ws] SUSCRIBIR_CONVERSACION de {}: {}",
                user_id,
                conversation_id
            );
            // No-op: los eventos van por broadcast y el front filtra.
        }
    }
}

/// `WA_TEMPLATE_CREATED { template: WaTemplateItem }`
pub fn build_template_created_event(template: &crate::models::whatsapp::WaTemplateItem) -> String {
    match serde_json::to_string(&serde_json::json!({
        "tipo": "WA_TEMPLATE_CREATED",
        "datos": {
            "template": template
        }
    })) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("[ws] serialize template_created error: {}", e);
            String::new()
        }
    }
}

/// `WA_TEMPLATE_UPDATED { template, prev_status? }` — `prev_status` se omite
/// (no se serializa) si es `None`.
pub fn build_template_updated_event(
    template: &crate::models::whatsapp::WaTemplateItem,
    prev_status: Option<crate::models::whatsapp::WaTemplateStatus>,
) -> String {
    let datos = if let Some(status) = prev_status {
        serde_json::json!({
            "template": template,
            "prev_status": status
        })
    } else {
        serde_json::json!({
            "template": template
        })
    };

    match serde_json::to_string(&serde_json::json!({
        "tipo": "WA_TEMPLATE_UPDATED",
        "datos": datos
    })) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("[ws] serialize template_updated error: {}", e);
            String::new()
        }
    }
}

/// `WA_TEMPLATE_DELETED { id, name, language, phone_number_id }`
pub fn build_template_deleted_event(
    id: &str,
    name: &str,
    language: &str,
    phone_number_id: &str,
) -> String {
    match serde_json::to_string(&serde_json::json!({
        "tipo": "WA_TEMPLATE_DELETED",
        "datos": {
            "id": id,
            "name": name,
            "language": language,
            "phone_number_id": phone_number_id
        }
    })) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("[ws] serialize template_deleted error: {}", e);
            String::new()
        }
    }
}

#[allow(dead_code)]
async fn send_error(state: &Arc<AppState>, user_id: &str, error: &str) {
    tracing::warn!("[ws] error para agente {}: {}", user_id, error);
    send_to_agent(
        &state.ws_registry,
        user_id,
        &WsServerEvent::Error {
            error: error.to_string(),
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::sync::RwLock;

    fn make_registry() -> WsRegistry {
        Arc::new(RwLock::new(HashMap::new()))
    }

    /// Normal delivery: sender present, receiver gets exact payload.
    #[tokio::test]
    async fn send_to_user_delivers_to_existing_user() {
        let registry = make_registry();
        let (tx, mut rx) = mpsc::channel::<String>(8);
        registry.write().await.insert("u1".to_string(), tx);

        send_to_user(&registry, "u1", "payload".to_string()).await;

        assert_eq!(rx.recv().await, Some("payload".to_string()));
    }

    /// Silent when user absent: empty registry, must not panic.
    #[tokio::test]
    async fn send_to_user_silent_when_user_absent() {
        let registry = make_registry();
        // Should return normally without panic
        send_to_user(&registry, "no-such-user", "payload".to_string()).await;
    }

    /// Silent when sender closed: receiver dropped, must not panic.
    #[tokio::test]
    async fn send_to_user_silent_when_sender_closed() {
        let registry = make_registry();
        let (tx, rx) = mpsc::channel::<String>(8);
        registry.write().await.insert("u2".to_string(), tx);
        drop(rx);

        // Sender is closed — should return normally without panic
        send_to_user(&registry, "u2", "payload".to_string()).await;
    }

    fn sample_message_item() -> MessageItem {
        MessageItem {
            id: "507f191e810c19729de860ea".to_string(),
            conversation_id: "507f191e810c19729de860eb".to_string(),
            wa_message_id: "wamid.message".to_string(),
            direction: "in".to_string(),
            msg_type: "text".to_string(),
            content: Some("hello".to_string()),
            media_id: None,
            media_mime_type: None,
            media_filename: None,
            status: Some("read".to_string()),
            meta_error_code: None,
            meta_error_title: None,
            meta_error_message: None,
            meta_error_details: None,
            failed_at: None,
            from_user_id: None,
            from_user_name: None,
            source: None,
            campaign_id: None,
            campaign_recipient_id: None,
            idempotency_key: None,
            reply_to: None,
            is_forwarded: None,
            is_frequently_forwarded: None,
            url_preview: None,
            voice: false,
            template_name: None,
            template_language: None,
            template_components: None,
            interactive_payload: None,
            contacts_payload: None,
            location: None,
            reactions: vec![],
            raw_payload: None,
            audio_transcription: None,
            ai_processed_at: None,
            created_at: "2026-06-05T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn mensaje_modificado_event_serialize_roundtrip() {
        let event = WsServerEvent::MensajeModificado {
            conversation_id: "507f191e810c19729de860ec".to_string(),
            message: sample_message_item(),
            change_type: "edit".to_string(),
        };

        let payload = serde_json::to_string(&event).expect("serialize event");
        let event_json: serde_json::Value =
            serde_json::from_str(&payload).expect("deserialize payload json");

        assert_eq!(event_json["tipo"], "MENSAJE_MODIFICADO");
        assert_eq!(event_json["datos"]["change_type"], "edit");
        assert_eq!(
            event_json["datos"]["conversation_id"],
            "507f191e810c19729de860ec"
        );

        assert_eq!(
            event_json["datos"]["message"]["wa_message_id"],
            "wamid.message"
        );
    }
}
