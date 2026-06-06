pub mod assignment;
pub mod audit;
pub mod backfill;
pub mod conversations;
pub mod handler;
pub mod messaging;
pub mod quick_reply_validation;
pub mod service;
pub mod shared;
pub mod tickets;
pub mod url_preview;
pub mod webhook;
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
        .route(
            "/v1/webhook/whatsapp",
            get(webhook::handler::verify_webhook),
        )
        .route(
            "/v1/webhook/whatsapp",
            post(webhook::handler::receive_webhook),
        )
}

/// Ruta WebSocket: autenticación primaria por cookie HttpOnly.
/// Compat temporal: `?token=` sólo durante migración controlada por env.
pub fn ws_routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/ws/chat", get(ws::ws_handler))
}

/// Rutas REST protegidas con JWT de staff/admin
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Conversaciones
        .route(
            "/v1/auth-user/whatsapp/conversations",
            get(conversations::handlers::list_conversations_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/stats",
            get(conversations::handlers::conversations_stats_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id",
            get(conversations::handlers::get_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/client-link",
            get(conversations::handlers::get_conversation_client_link_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/messages",
            get(conversations::handlers::get_conversation_messages_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/messages",
            post(conversations::handlers::send_message_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/mark-read",
            post(conversations::handlers::mark_read_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/take",
            post(conversations::handlers::take_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/transfer",
            post(conversations::handlers::transfer_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/close",
            post(conversations::handlers::close_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/reopen",
            post(conversations::handlers::reopen_conversation_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/agent-state/reset",
            post(conversations::handlers::reset_ai_conv_state_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/conversations/:id/intervene",
            post(conversations::handlers::intervene_conversation_handler),
        )
        // Iniciar una conversación (agente outbound first) — siempre template
        .route(
            "/v1/auth-user/whatsapp/conversations/initiate",
            post(conversations::handlers::initiate_conversation_handler),
        )
        // Agentes con permiso de chat (para dropdown de transferencia)
        .route(
            "/v1/auth-user/whatsapp/transferable-agents",
            get(conversations::handlers::list_transferable_agents_handler),
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
        // Test connection: pre-creación (raw) y re-test sobre setting guardado.
        // La ruta sin `:id` debe declararse ANTES de las que matchean `:id`
        // para que axum no interprete `test-connection` como un id literal.
        .route(
            "/v1/auth-user/whatsapp/settings/test-connection",
            post(handler::test_settings_connection_raw_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/settings/:id/test-connection",
            post(handler::test_settings_connection_stored_handler),
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
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers/test-connection",
            post(handler::test_settings_connection_raw_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers/:id/test-connection",
            post(handler::test_settings_connection_stored_handler),
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
            get(webhook::handler::debug_last_webhook_handler),
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
        // Reacciones sobre mensajes individuales
        .route(
            "/v1/auth-user/whatsapp/messages/:id/react",
            post(handler::react_message_handler),
        )
}
