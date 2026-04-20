pub mod handler;
pub mod service;

use axum::{
    routing::{get, patch, post},
    Router,
};
use crate::state::AppState;
use std::sync::Arc;

/// Rutas públicas: webhook de Meta (sin JWT)
pub fn webhook_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/webhook/whatsapp", get(handler::verify_webhook))
        .route("/v1/webhook/whatsapp", post(handler::receive_webhook))
}

/// Rutas protegidas con JWT de staff/admin
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/auth-user/whatsapp/conversations", get(handler::list_conversations_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/messages", get(handler::get_conversation_messages_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/messages", post(handler::send_message_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/status", patch(handler::update_status_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/assign", patch(handler::assign_conversation_handler))
}
