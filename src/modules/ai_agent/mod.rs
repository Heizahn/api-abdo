//! Módulo `ai_agent` — Asistente Virtual de WhatsApp (modelo agent-centric).
//!
//! Cada `AiAgent` lleva todo lo necesario para correr (api_key, model, prompt,
//! tools, limits) y atiende a 0+ workspaces. Sin recepcionista todavía: cada
//! agente sirve directo. La recepcionista llega en una vuelta posterior.

pub mod business_data;
pub mod config_resolver;
pub mod dispatch;
pub mod escalation;
pub mod guardrails;
pub mod handler;
pub mod openrouter;
pub mod pre_classifier;
pub mod reference_normalize;
pub mod runner;
pub mod sandbox;
pub mod seed;
pub mod state;
pub mod tools;

/// Retorna el secreto usado para cifrar/descifrar con AES-GCM:
/// - La `openrouter_api_key` en `AiConfig.openrouter_api_key`
/// - El `access_token` de Meta WhatsApp en `WaSettings.access_token`
///
/// Ambos usan el mismo `JWT_SECRET` derivado del entorno — el caller decide
/// qué payload descifra.
pub(crate) fn ai_agent_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

use axum::{
    routing::{get, patch, post},
    Router,
};
use std::sync::Arc;

use crate::state::AppState;

/// Rutas REST protegidas con JWT staff/admin. Todas requieren rol SUPERADMIN
/// (validado dentro del handler — el middleware ya filtra `nRole == -1`).
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Configuración global de AI (SUPERADMIN)
        .route(
            "/v1/auth-user/whatsapp/ai-agent/config",
            get(handler::get_ai_config_handler).patch(handler::patch_ai_config_handler),
        )
        // CRUD agentes
        .route(
            "/v1/auth-user/whatsapp/ai-agent/agents",
            get(handler::list_ai_agents_handler).post(handler::create_ai_agent_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/agents/:id",
            get(handler::get_ai_agent_handler)
                .patch(handler::update_ai_agent_handler)
                .delete(handler::delete_ai_agent_handler),
        )
        // FAQs anidadas bajo el agente
        .route(
            "/v1/auth-user/whatsapp/ai-agent/agents/:id/faqs",
            get(handler::list_ai_agent_faqs_handler).post(handler::create_ai_agent_faq_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/agents/faqs/item/:id",
            patch(handler::update_ai_agent_faq_handler)
                .delete(handler::delete_ai_agent_faq_handler),
        )
        // Test connection / models por agente (post-creación)
        .route(
            "/v1/auth-user/whatsapp/ai-agent/agents/:id/test-connection",
            post(handler::test_connection_for_agent_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/agents/:id/models",
            get(handler::list_models_for_agent_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/agents/:id/sandbox",
            post(sandbox::sandbox_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/agents/:id/metrics",
            get(handler::get_ai_agent_metrics_handler),
        )
        // Test connection / models RAW (pre-creación)
        .route(
            "/v1/auth-user/whatsapp/ai-agent/test-connection",
            post(handler::test_connection_raw_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/models",
            get(handler::list_models_raw_handler),
        )
        // Discovery: tools disponibles para que el editor del front no hardcodee
        .route(
            "/v1/auth-user/whatsapp/ai-agent/tools",
            get(business_data::list_tools_handler),
        )
        // CRUD planes (datos de negocio que la tool list_plans expone)
        .route(
            "/v1/auth-user/whatsapp/ai-agent/plans",
            get(business_data::list_plans_handler).post(business_data::create_plan_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/plans/:id",
            patch(business_data::update_plan_handler).delete(business_data::delete_plan_handler),
        )
        // CRUD zonas de cobertura
        .route(
            "/v1/auth-user/whatsapp/ai-agent/coverage-zones",
            get(business_data::list_coverage_zones_handler)
                .post(business_data::create_coverage_zone_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/coverage-zones/:id",
            patch(business_data::update_coverage_zone_handler)
                .delete(business_data::delete_coverage_zone_handler),
        )
        // División política canónica VE (estados/municipios) — path acordado
        // con el front team. Vive bajo /ai/zones/ por contrato, no bajo
        // /ai-agent/coverage-zones/, para mantener el contrato original.
        .route(
            "/v1/auth-user/whatsapp/ai/zones/political-divisions",
            get(business_data::list_political_divisions_handler),
        )
        // Instalaciones (costos por tipo de conexión)
        .route(
            "/v1/auth-user/whatsapp/ai-agent/installations",
            get(business_data::list_installations_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/installations/:type",
            patch(business_data::update_installation_handler),
        )
        // Promociones
        .route(
            "/v1/auth-user/whatsapp/ai-agent/promotions",
            get(business_data::list_promotions_handler)
                .post(business_data::create_promotion_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/ai-agent/promotions/:id",
            patch(business_data::update_promotion_handler)
                .delete(business_data::delete_promotion_handler),
        )
}
