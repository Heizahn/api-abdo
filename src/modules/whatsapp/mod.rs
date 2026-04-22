pub mod assignment;
pub mod handler;
pub mod service;
pub mod ws;

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use crate::state::AppState;
use std::sync::Arc;

/// Rutas públicas: webhook de Meta (sin JWT ni rate limit)
pub fn webhook_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/webhook/whatsapp", get(handler::verify_webhook))
        .route("/v1/webhook/whatsapp", post(handler::receive_webhook))
}

/// Ruta WebSocket: autenticación via query param ?token=<user_jwt>
pub fn ws_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/ws/chat", get(ws::ws_handler))
}

/// Rutas REST protegidas con JWT de staff/admin
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Conversaciones
        .route("/v1/auth-user/whatsapp/conversations", get(handler::list_conversations_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id", get(handler::get_conversation_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/messages", get(handler::get_conversation_messages_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/messages", post(handler::send_message_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/mark-read", post(handler::mark_read_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/take", post(handler::take_conversation_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/transfer", post(handler::transfer_conversation_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/close", post(handler::close_conversation_handler))
        // Agentes con permiso de chat (para dropdown de transferencia)
        .route("/v1/auth-user/whatsapp/transferable-agents", get(handler::list_transferable_agents_handler))
        // Configuración de números y agentes
        .route("/v1/auth-user/whatsapp/settings", get(handler::list_settings_handler))
        .route("/v1/auth-user/whatsapp/settings", post(handler::create_settings_handler))
        .route("/v1/auth-user/whatsapp/settings/:id", put(handler::update_settings_handler))
        .route("/v1/auth-user/whatsapp/settings/:id", delete(handler::delete_settings_handler))
        // Alias: el frontend se refiere a estas configs como "WhatsApp Numbers"
        .route("/v1/auth-user/whatsapp/whatsapp-numbers", get(handler::list_settings_handler))
        .route("/v1/auth-user/whatsapp/whatsapp-numbers", post(handler::create_settings_handler))
        .route("/v1/auth-user/whatsapp/whatsapp-numbers/:id", put(handler::update_settings_handler))
        .route("/v1/auth-user/whatsapp/whatsapp-numbers/:id", delete(handler::delete_settings_handler))
        // Debug
        .route("/v1/auth-user/whatsapp/debug/last-webhook", get(handler::debug_last_webhook_handler))
}
