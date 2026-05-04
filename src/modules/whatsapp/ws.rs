use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::{
    auth::user_jwt::UserJwtService,
    db::WhatsAppRepository,
    models::whatsapp::{ConversationItem, MessageItem, TicketItem},
    state::{AppState, WsRegistry},
};

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
    /// `reason` es uno de: `"request_human"`, `"transfer_to_agent_failed"`,
    /// `"manual"`. `by` es `"ai_agent"` cuando el origen fue una tool del
    /// loop, o el UUID del usuario que pausó manualmente.
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

    #[serde(rename = "ERROR")]
    Error { error: String },

    /// Confirmación de conexión tras el upgrade WebSocket.
    #[serde(rename = "CONECTADO")]
    Conectado { usuario_id: String },
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
    let registry = registry.read().await;
    if let Some(sender) = registry.get(agent_id) {
        let _ = sender.send(json);
    }
}

/// Envía un payload JSON (string) a un agente específico.
/// Drop silencioso si el agente no está conectado.
pub async fn send_to_user(registry: &WsRegistry, user_id: &str, payload: String) {
    let registry = registry.read().await;
    if let Some(sender) = registry.get(user_id) {
        let _ = sender.send(payload);
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
    let settings = match state.db.find_wa_settings_by_phone_number_id(phone_number_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!("[ws] WaSettings not found for phone_number_id: {}", phone_number_id);
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
        let _ = sender.send(json.clone());
    }
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
        let _ = sender.send(json.clone());
    }
}

// ============================================
// HANDLER DE CONEXIÓN WEBSOCKET
// ============================================

#[derive(Deserialize)]
pub struct WsConnectParams {
    token: String,
}

/// GET /v1/ws/chat?token=<user_jwt>
/// Upgrade a WebSocket. Valida JWT de staff/admin via query param.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(params): Query<WsConnectParams>,
) -> impl IntoResponse {
    // Validar JWT antes del upgrade
    let jwt = UserJwtService::new();
    match jwt.verify_token(&params.token) {
        Ok(claims) => ws.on_upgrade(move |socket| handle_socket(socket, state, claims.id, claims.name)),
        Err(_) => {
            // No podemos retornar error HTTP después del upgrade — rechazamos antes
            ws.on_upgrade(|mut socket| async move {
                let err = serde_json::to_string(&WsServerEvent::Error {
                    error: "token_invalido".to_string(),
                })
                .unwrap_or_default();
                let _ = socket.send(Message::Text(err.into())).await;
                let _ = socket.close().await;
            })
        }
    }
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, user_id: String, user_name: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Registrar en WsRegistry
    {
        let mut registry = state.ws_registry.write().await;
        registry.insert(user_id.clone(), tx);
    }
    tracing::info!("[ws] agente conectado: {} ({})", user_name, user_id);

    // Confirmar conexión
    let connected_msg = serde_json::to_string(&WsServerEvent::Conectado {
        usuario_id: user_id.clone(),
    })
    .unwrap_or_default();
    let _ = sink.send(Message::Text(connected_msg.into())).await;

    // Task: reenviar mensajes del canal al socket
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(Message::Text(msg.into())).await.is_err() {
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
    tracing::info!("[ws] agente desconectado: {} ({})", user_name, user_id);
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
            tracing::info!("[ws] CONECTAR recibido de {} ({})", nombre, usuario_id);
            let _ = user_name;
        }

        WsClientEvent::SuscribirConversacion { conversation_id } => {
            tracing::debug!(
                "[ws] SUSCRIBIR_CONVERSACION de {}: {}",
                user_id, conversation_id
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
        &WsServerEvent::Error { error: error.to_string() },
    )
    .await;
}
