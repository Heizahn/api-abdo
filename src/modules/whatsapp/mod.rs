pub mod assignment;
pub mod handler;
pub mod service;
pub mod ws;

use axum::{
    routing::{get, patch, post},
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
        .route("/v1/auth-user/whatsapp/conversations", get(handler::list_conversations_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/messages", get(handler::get_conversation_messages_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/messages", post(handler::send_message_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/status", patch(handler::update_status_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/assign", patch(handler::assign_conversation_handler))
}
