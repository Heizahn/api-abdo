pub mod assignment;
pub mod backfill;
pub mod handler;
pub mod quick_reply_validation;
pub mod service;
pub mod url_preview;
pub mod ws;

use axum::{
    routing::{delete, get, patch, post, put},
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
        .route("/v1/auth-user/whatsapp/conversations/stats", get(handler::conversations_stats_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id", get(handler::get_conversation_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/messages", get(handler::get_conversation_messages_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/messages", post(handler::send_message_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/mark-read", post(handler::mark_read_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/take", post(handler::take_conversation_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/transfer", post(handler::transfer_conversation_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/close", post(handler::close_conversation_handler))
        .route("/v1/auth-user/whatsapp/conversations/:id/reopen", post(handler::reopen_conversation_handler))
        // Iniciar una conversación (agente outbound first) — siempre template
        .route("/v1/auth-user/whatsapp/conversations/initiate", post(handler::initiate_conversation_handler))
        // Agentes con permiso de chat (para dropdown de transferencia)
        .route("/v1/auth-user/whatsapp/transferable-agents", get(handler::list_transferable_agents_handler))
        // Media: proxy de descarga (el binario vive en la CDN de Meta)
        .route("/v1/auth-user/whatsapp/media/:media_id", get(handler::get_media_handler))
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
        // Quick replies (snippets de texto)
        .route("/v1/auth-user/whatsapp/quick-replies", get(handler::list_quick_replies_handler))
        .route("/v1/auth-user/whatsapp/quick-replies", post(handler::create_quick_reply_handler))
        .route("/v1/auth-user/whatsapp/quick-replies/:id", put(handler::update_quick_reply_handler))
        .route("/v1/auth-user/whatsapp/quick-replies/:id", delete(handler::delete_quick_reply_handler))
        .route("/v1/auth-user/whatsapp/quick-replies/:id/active", patch(handler::set_quick_reply_active_handler))
        .route("/v1/auth-user/whatsapp/quick-replies/:id/duplicate", post(handler::duplicate_quick_reply_handler))
        // Templates (Meta Cloud API, cached 5min en Redis)
        .route("/v1/auth-user/whatsapp/templates", get(handler::list_templates_handler))
        // Debug
        .route("/v1/auth-user/whatsapp/debug/last-webhook", get(handler::debug_last_webhook_handler))
}
