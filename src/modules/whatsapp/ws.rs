use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use mongodb::bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::{
    auth::user_jwt::UserJwtService,
    db::WhatsAppRepository,
    models::whatsapp::{MessageItem, WaMessage},
    state::{AppState, WsRegistry},
};

use super::service::WhatsAppService;

// ============================================
// TIPOS DE EVENTOS
// ============================================

#[derive(Debug, Deserialize)]
#[serde(tag = "tipo", content = "datos")]
pub enum WsClientEvent {
    #[serde(rename = "CONECTAR")]
    Conectar { usuario_id: String, nombre: String },

    #[serde(rename = "RESPONDER")]
    Responder {
        conversacion_id: String,
        respuesta: String,
    },
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "tipo", content = "datos")]
pub enum WsServerEvent {
    #[serde(rename = "MENSAJE_ASIGNADO")]
    MensajeAsignado {
        conversation_id: String,
        phone: String,
        name: Option<String>,
        last_message_preview: Option<String>,
        assigned_to: String,
    },

    #[serde(rename = "MENSAJE_RECIBIDO")]
    MensajeRecibido {
        conversation_id: String,
        phone: String,
        name: Option<String>,
        unread_count: i32,
        message: MessageItem,
    },

    #[serde(rename = "MENSAJE_STATUS_ACTUALIZADO")]
    MensajeStatusActualizado {
        conversation_id: String,
        wa_message_id: String,
        status: String,
    },

    #[serde(rename = "CONVERSACION_ACTUALIZADA")]
    ConversacionActualizada {
        conversation_id: String,
        status: String,
        assigned_to: Option<String>,
        unread_count: i32,
    },

    #[serde(rename = "MENSAJE_RESPONDIDO")]
    MensajeRespondido {
        conversation_id: String,
        respondido_por: String,
        respondido_por_nombre: String,
    },

    #[serde(rename = "ERROR")]
    Error { error: String },

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
                handle_client_message(&state, &user_id, &user_name, &text).await;
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

async fn handle_client_message(
    state: &Arc<AppState>,
    user_id: &str,
    user_name: &str,
    text: &str,
) {
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
        }

        WsClientEvent::Responder { conversacion_id, respuesta } => {
            handle_responder(state, user_id, user_name, &conversacion_id, &respuesta).await;
        }
    }
}

async fn handle_responder(
    state: &Arc<AppState>,
    user_id: &str,
    user_name: &str,
    conversacion_id: &str,
    respuesta: &str,
) {
    let oid = match ObjectId::parse_str(conversacion_id) {
        Ok(id) => id,
        Err(_) => {
            send_error(state, user_id, "conversacion_id inválido").await;
            return;
        }
    };

    let conv = match state.db.find_conversation_by_id(&oid).await {
        Ok(Some(c)) => c,
        _ => {
            send_error(state, user_id, "conversación no encontrada").await;
            return;
        }
    };

    // Enviar mensaje por WhatsApp
    let wa = match WhatsAppService::from_env(state.reqwest_client.clone()) {
        Ok(s) => s,
        Err(e) => {
            send_error(state, user_id, &e.to_string()).await;
            return;
        }
    };

    let wa_id = match wa.send_text(&conv.phone, respuesta).await {
        Ok(id) => id,
        Err(e) => {
            send_error(state, user_id, &format!("error enviando a WhatsApp: {}", e)).await;
            return;
        }
    };

    // Guardar mensaje saliente
    let msg = WaMessage {
        id: None,
        conversation_id: oid,
        wa_message_id: wa_id,
        direction: "outbound".to_string(),
        msg_type: "text".to_string(),
        body: Some(respuesta.to_string()),
        media_id: None,
        status: Some("sent".to_string()),
        sent_by: Some(user_id.to_string()),
        timestamp: DateTime::now(),
    };

    if let Err(e) = state.db.save_message(msg).await {
        tracing::error!("[ws] save_message error: {}", e);
    }

    if let Err(e) = state.db.touch_conversation(&oid, respuesta, false).await {
        tracing::warn!("[ws] touch_conversation error: {}", e);
    }

    // Decrementar carga del agente
    state.redis.decr_agent_load(user_id).await;

    tracing::info!(
        "[ws] {} ({}) respondió a conversación {}",
        user_name, user_id, conversacion_id
    );

    // Broadcast a todos: la conversación fue respondida
    let broadcast = WsServerEvent::MensajeRespondido {
        conversation_id: conversacion_id.to_string(),
        respondido_por: user_id.to_string(),
        respondido_por_nombre: user_name.to_string(),
    };
    broadcast_all(&state.ws_registry, &broadcast).await;
}

async fn send_error(state: &Arc<AppState>, user_id: &str, error: &str) {
    tracing::warn!("[ws] error para agente {}: {}", user_id, error);
    send_to_agent(
        &state.ws_registry,
        user_id,
        &WsServerEvent::Error { error: error.to_string() },
    )
    .await;
}
