pub mod assignment;
pub mod audit;
pub mod backfill;
pub mod handler;
pub mod quick_reply_validation;
pub mod service;
pub mod tickets;
pub mod url_preview;
pub mod ws;

use crate::state::AppState;
use axum::{
    extract::DefaultBodyLimit,
    routing::{delete, get, patch, post, put},
    Router,
};
use std::sync::Arc;

/// Rutas públicas: webhook de Meta (sin JWT ni rate limit)
pub fn webhook_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/webhook/whatsapp", get(handler::verify_webhook))
        .route("/v1/webhook/whatsapp", post(handler::receive_webhook))
}

/// Ruta WebSocket: autenticación via query param ?token=<user_jwt>
pub fn ws_routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/ws/chat", get(ws::ws_handler))
}

/// Rutas REST protegidas con JWT de staff/admin
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Conversaciones
        .route(
            "/v1/auth-user/whatsapp/conversations",
            get(handler::list_conversations_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/stats",
            get(handler::conversations_stats_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id",
            get(handler::get_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/messages",
            get(handler::get_conversation_messages_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/messages",
            post(handler::send_message_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/mark-read",
            post(handler::mark_read_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/take",
            post(handler::take_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/transfer",
            post(handler::transfer_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/close",
            post(handler::close_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/reopen",
            post(handler::reopen_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/agent-state/reset",
            post(handler::reset_ai_conv_state_handler),
        )
        // Iniciar una conversación (agente outbound first) — siempre template
        .route(
            "/v1/auth-user/whatsapp/conversations/initiate",
            post(handler::initiate_conversation_handler),
        )
        // Agentes con permiso de chat (para dropdown de transferencia)
        .route(
            "/v1/auth-user/whatsapp/transferable-agents",
            get(handler::list_transferable_agents_handler),
        )
        // Media: proxy de descarga (el binario vive en la CDN de Meta)
        .route(
            "/v1/auth-user/whatsapp/media/:media_id",
            get(handler::get_media_handler),
        )
        // Media: límites por tipo — el front los lee para validar client-side.
        .route(
            "/v1/auth-user/whatsapp/media/limits",
            get(handler::get_media_limits_handler),
        )
        // Media: upload multipart hacia Meta (paso 1 del envío outbound).
        // El body limit por defecto de axum es 2 MiB — lo desactivamos por
        // ruta porque el tope real lo aplica el handler según `type` y la
        // config (`wa_media_max_*_bytes`, hasta 100 MiB para documentos).
        .route(
            "/v1/auth-user/whatsapp/media",
            post(handler::upload_media_handler).layer(DefaultBodyLimit::disable()),
        )
        // Configuración de números y agentes
        .route(
            "/v1/auth-user/whatsapp/settings",
            get(handler::list_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/settings",
            post(handler::create_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/settings/:id",
            put(handler::update_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/settings/:id",
            delete(handler::delete_settings_handler),
        )
        // Alias: el frontend se refiere a estas configs como "WhatsApp Numbers"
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers",
            get(handler::list_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers",
            post(handler::create_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers/:id",
            put(handler::update_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers/:id",
            delete(handler::delete_settings_handler),
        )
        // Quick replies (snippets de texto)
        .route(
            "/v1/auth-user/whatsapp/quick-replies",
            get(handler::list_quick_replies_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies",
            post(handler::create_quick_reply_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies/:id",
            put(handler::update_quick_reply_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies/:id",
            delete(handler::delete_quick_reply_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies/:id/active",
            patch(handler::set_quick_reply_active_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies/:id/duplicate",
            post(handler::duplicate_quick_reply_handler),
        )
        // Templates CRUD (WaTemplates — DB local)
        .route(
            "/v1/auth-user/whatsapp/templates",
            post(handler::create_template_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates",
            get(handler::list_templates_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/header-media",
            post(handler::upload_template_header_media_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/:id",
            get(handler::get_template_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/:id",
            patch(handler::update_template_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/:id",
            delete(handler::delete_template_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/:id/resync",
            post(handler::resync_template_handler),
        )
        // Debug
        .route(
            "/v1/auth-user/whatsapp/debug/last-webhook",
            get(handler::debug_last_webhook_handler),
        )
        // Auditoría / trazabilidad (SUPERADMIN only)
        .route(
            "/v1/auth-user/whatsapp/audit/messages",
            get(audit::audit_messages_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/audit/metrics",
            get(audit::audit_metrics_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/audit/export",
            get(audit::audit_export_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/audit/conversations/:id/messages",
            get(audit::audit_conversation_messages_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/audit/conversations/:id/timeline",
            get(audit::audit_conversation_timeline_handler),
        )
        // Tickets — soporte derivado de chats (bCanChat). El catálogo de
        // categorías se sirve estático en el back; el resto opera sobre
        // `WaTickets`. La ruta `categories` debe ir ANTES de `:id` para que
        // axum no la interprete como un id literal.
        .route(
            "/v1/auth-user/whatsapp/tickets/categories",
            get(tickets::list_ticket_categories_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/tickets",
            get(tickets::list_tickets_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/tickets",
            post(tickets::create_ticket_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/tickets/:id",
            get(tickets::get_ticket_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/tickets/:id",
            patch(tickets::update_ticket_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/transfer-and-ticket",
            post(tickets::transfer_and_ticket_handler),
        )
}
