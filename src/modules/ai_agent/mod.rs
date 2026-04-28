//! Módulo `ai_agent` — Asistente Virtual de WhatsApp.
//!
//! PR 1 sólo expone el storage (settings + FAQs) y crea el AI user sintético.
//! El loop de Gemini, los tools y el dispatch del inbound se agregan en PRs
//! posteriores (ver plan v1.4 §8).

pub mod gemini;
pub mod handler;
pub mod runner;
pub mod sandbox;
pub mod tools;

use axum::{
    routing::{delete, get, patch, post},
    Router,
};
use std::sync::Arc;

use crate::state::AppState;

/// Rutas REST protegidas con JWT staff/admin. Todas requieren rol SUPERADMIN
/// (validado dentro del handler — el middleware ya filtra `nRole == -1`).
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Settings (uno por workspace = WaSettings._id)
        .route(
            "/v1/auth-user/whatsapp/ai-agent/settings",
            get(handler::list_ai_agent_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/settings/:workspace_id",
            get(handler::get_ai_agent_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/settings/:workspace_id",
            patch(handler::update_ai_agent_settings_handler),
        )
        // FAQs (knowledge base)
        .route(
            "/v1/auth-user/whatsapp/ai-agent/faqs/:workspace_id",
            get(handler::list_ai_agent_faqs_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/faqs/:workspace_id",
            post(handler::create_ai_agent_faq_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/faqs/item/:id",
            patch(handler::update_ai_agent_faq_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/faqs/item/:id",
            delete(handler::delete_ai_agent_faq_handler),
        )
        // Sandbox: ejecuta un turno IA con tools reales pero sin persistir
        // AiInteraction ni crear tickets reales (is_sandbox=true).
        .route(
            "/v1/auth-user/whatsapp/ai-agent/sandbox/:workspace_id",
            post(sandbox::sandbox_handler),
        )
        // Test de conexión a Gemini — GET /v1/models/{model_id} con la
        // api_key. No consume cuota de generación.
        .route(
            "/v1/auth-user/whatsapp/ai-agent/test-connection",
            post(handler::test_connection_handler),
        )
        // Listado de modelos Gemini para una api_key — usado por la UI
        // para que el SUPERADMIN elija qué modelo guardar. Cacheado en
        // Redis 10 min por (workspace, hash de api_key).
        .route(
            "/v1/auth-user/whatsapp/ai-agent/models/:workspace_id",
            get(handler::list_ai_agent_models_handler),
        )
}
