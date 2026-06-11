pub mod assignment;
pub mod audit;
pub mod backfill;
pub mod campaigns;
pub mod conversations;
pub mod handler;
pub mod messaging;
pub mod quick_replies;
pub mod quick_reply_validation;
pub mod service;
pub mod settings;
pub mod shared;
pub mod templates;
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
            "/v1/admin/whatsapp-campaigns",
            get(campaigns::handler::list_campaigns_handler),
        )
        .route(
            "/v1/admin/whatsapp-campaigns",
            post(campaigns::handler::create_campaign_handler),
        )
        .route(
            "/v1/admin/whatsapp-campaigns/preview",
            post(campaigns::handler::preview_campaign_recipients_handler),
        )
        .route(
            "/v1/admin/whatsapp-campaigns/:id/recipients",
            get(campaigns::handler::get_campaign_recipients_handler),
        )
        .route(
            "/v1/admin/whatsapp-campaigns/:id/recipients/exclusions",
            patch(campaigns::handler::update_campaign_recipient_exclusions_handler),
        )
        .route(
            "/v1/admin/whatsapp-campaigns/:id/confirm",
            post(campaigns::handler::confirm_campaign_handler),
        )
        .route(
            "/v1/admin/whatsapp-campaigns/:id",
            get(campaigns::handler::get_campaign_handler),
        )
        .route(
            "/v1/admin/whatsapp-campaigns/:id",
            patch(campaigns::handler::update_campaign_handler),
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
            get(messaging::download::get_media_handler),
        )
        // Media: límites por tipo — el front los lee para validar client-side.
        .route(
            "/v1/auth-user/whatsapp/media/limits",
            get(messaging::media::get_media_limits_handler),
        )
        // Media: upload multipart hacia Meta (paso 1 del envío outbound).
        // El body limit por defecto de axum es 2 MiB — lo desactivamos por
        // ruta porque el tope real lo aplica el handler según `type` y la
        // config (`wa_media_max_*_bytes`, hasta 100 MiB para documentos).
        .route(
            "/v1/auth-user/whatsapp/media",
            post(messaging::media::upload_media_handler).layer(DefaultBodyLimit::disable()),
        )
        // Configuración de números y agentes
        .route(
            "/v1/auth-user/whatsapp/settings",
            get(settings::handlers::list_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/settings",
            post(settings::handlers::create_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/settings/:id",
            put(settings::handlers::update_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/settings/:id",
            delete(settings::handlers::delete_settings_handler),
        )
        // Test connection: pre-creación (raw) y re-test sobre setting guardado.
        // La ruta sin `:id` debe declararse ANTES de las que matchean `:id`
        // para que axum no interprete `test-connection` como un id literal.
        .route(
            "/v1/auth-user/whatsapp/settings/test-connection",
            post(settings::handlers::test_settings_connection_raw_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/settings/:id/test-connection",
            post(settings::handlers::test_settings_connection_stored_handler),
        )
        // Alias: el frontend se refiere a estas configs como "WhatsApp Numbers"
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers",
            get(settings::handlers::list_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers",
            post(settings::handlers::create_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers/:id",
            put(settings::handlers::update_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers/:id",
            delete(settings::handlers::delete_settings_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers/test-connection",
            post(settings::handlers::test_settings_connection_raw_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/whatsapp-numbers/:id/test-connection",
            post(settings::handlers::test_settings_connection_stored_handler),
        )
        // Quick replies (snippets de texto)
        .route(
            "/v1/auth-user/whatsapp/quick-replies",
            get(quick_replies::handlers::list_quick_replies_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies",
            post(quick_replies::handlers::create_quick_reply_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies/:id",
            put(quick_replies::handlers::update_quick_reply_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies/:id",
            delete(quick_replies::handlers::delete_quick_reply_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies/:id/active",
            patch(quick_replies::handlers::set_quick_reply_active_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/quick-replies/:id/duplicate",
            post(quick_replies::handlers::duplicate_quick_reply_handler),
        )
        // Templates CRUD (WaTemplates — DB local)
        .route(
            "/v1/auth-user/whatsapp/templates",
            post(templates::handlers::create_template_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates",
            get(templates::handlers::list_templates_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/header-media",
            post(templates::header_media::upload_template_header_media_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/:id",
            get(templates::handlers::get_template_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/:id",
            patch(templates::handlers::update_template_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/:id",
            delete(templates::handlers::delete_template_handler),
        )
        .route(
            "/v1/auth-user/whatsapp/templates/:id/resync",
            post(templates::handlers::resync_template_handler),
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
            post(messaging::reactions::react_message_handler),
        )
}

#[cfg(test)]
mod tests {
    use regex::Regex;
    use serde_json::Value;
    use std::collections::{BTreeMap, BTreeSet};
    use utoipa::OpenApi;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    struct RouteInventoryEntry {
        method: &'static str,
        path: &'static str,
        documented: bool,
        tag: Option<&'static str>,
    }

    const fn documented(
        method: &'static str,
        path: &'static str,
        tag: &'static str,
    ) -> RouteInventoryEntry {
        RouteInventoryEntry {
            method,
            path,
            documented: true,
            tag: Some(tag),
        }
    }

    const fn undocumented(method: &'static str, path: &'static str) -> RouteInventoryEntry {
        RouteInventoryEntry {
            method,
            path,
            documented: false,
            tag: None,
        }
    }

    const EXPECTED_ROUTE_INVENTORY: &[RouteInventoryEntry] = &[
        undocumented("GET", "/v1/webhook/whatsapp"),
        undocumented("POST", "/v1/webhook/whatsapp"),
        undocumented("GET", "/v1/ws/chat"),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/conversations",
            "WhatsApp — Soporte",
        ),
        documented(
            "GET",
            "/v1/admin/whatsapp-campaigns",
            "WhatsApp — Campaigns",
        ),
        documented(
            "POST",
            "/v1/admin/whatsapp-campaigns",
            "WhatsApp — Campaigns",
        ),
        documented(
            "POST",
            "/v1/admin/whatsapp-campaigns/preview",
            "WhatsApp — Campaigns",
        ),
        documented(
            "GET",
            "/v1/admin/whatsapp-campaigns/:id/recipients",
            "WhatsApp — Campaigns",
        ),
        documented(
            "PATCH",
            "/v1/admin/whatsapp-campaigns/:id/recipients/exclusions",
            "WhatsApp — Campaigns",
        ),
        documented(
            "POST",
            "/v1/admin/whatsapp-campaigns/:id/confirm",
            "WhatsApp — Campaigns",
        ),
        documented(
            "GET",
            "/v1/admin/whatsapp-campaigns/:id",
            "WhatsApp — Campaigns",
        ),
        documented(
            "PATCH",
            "/v1/admin/whatsapp-campaigns/:id",
            "WhatsApp — Campaigns",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/conversations/stats",
            "WhatsApp — Soporte",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/conversations/:id",
            "WhatsApp — Soporte",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/conversations/:id/client-link",
            "WhatsApp — Soporte",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/conversations/:id/messages",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/:id/messages",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/:id/mark-read",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/:id/take",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/:id/transfer",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/:id/close",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/:id/reopen",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/:id/agent-state/reset",
            "WhatsApp — Conversaciones",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/:id/intervene",
            "WhatsApp — Conversaciones",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/initiate",
            "WhatsApp — Soporte",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/transferable-agents",
            "WhatsApp — Soporte",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/media/:media_id",
            "WhatsApp — Soporte",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/media/limits",
            "WhatsApp — Soporte",
        ),
        documented("POST", "/v1/auth-user/whatsapp/media", "WhatsApp — Soporte"),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/settings",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/settings",
            "WhatsApp — Soporte",
        ),
        documented(
            "PUT",
            "/v1/auth-user/whatsapp/settings/:id",
            "WhatsApp — Soporte",
        ),
        documented(
            "DELETE",
            "/v1/auth-user/whatsapp/settings/:id",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/settings/test-connection",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/settings/:id/test-connection",
            "WhatsApp — Soporte",
        ),
        undocumented("GET", "/v1/auth-user/whatsapp/whatsapp-numbers"),
        undocumented("POST", "/v1/auth-user/whatsapp/whatsapp-numbers"),
        undocumented("PUT", "/v1/auth-user/whatsapp/whatsapp-numbers/:id"),
        undocumented("DELETE", "/v1/auth-user/whatsapp/whatsapp-numbers/:id"),
        undocumented(
            "POST",
            "/v1/auth-user/whatsapp/whatsapp-numbers/test-connection",
        ),
        undocumented(
            "POST",
            "/v1/auth-user/whatsapp/whatsapp-numbers/:id/test-connection",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/quick-replies",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/quick-replies",
            "WhatsApp — Soporte",
        ),
        documented(
            "PUT",
            "/v1/auth-user/whatsapp/quick-replies/:id",
            "WhatsApp — Soporte",
        ),
        documented(
            "DELETE",
            "/v1/auth-user/whatsapp/quick-replies/:id",
            "WhatsApp — Soporte",
        ),
        documented(
            "PATCH",
            "/v1/auth-user/whatsapp/quick-replies/:id/active",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/quick-replies/:id/duplicate",
            "WhatsApp — Soporte",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/templates",
            "WhatsApp — Templates",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/templates",
            "WhatsApp — Templates",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/templates/header-media",
            "WhatsApp — Templates",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/templates/:id",
            "WhatsApp — Templates",
        ),
        documented(
            "PATCH",
            "/v1/auth-user/whatsapp/templates/:id",
            "WhatsApp — Templates",
        ),
        documented(
            "DELETE",
            "/v1/auth-user/whatsapp/templates/:id",
            "WhatsApp — Templates",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/templates/:id/resync",
            "WhatsApp — Templates",
        ),
        undocumented("GET", "/v1/auth-user/whatsapp/debug/last-webhook"),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/audit/messages",
            "WhatsApp — Auditoría",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/audit/metrics",
            "WhatsApp — Auditoría",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/audit/export",
            "WhatsApp — Auditoría",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/audit/conversations/:id/messages",
            "WhatsApp — Auditoría",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/audit/conversations/:id/timeline",
            "WhatsApp — Auditoría",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/tickets/categories",
            "WhatsApp — Tickets",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/tickets",
            "WhatsApp — Tickets",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/tickets",
            "WhatsApp — Tickets",
        ),
        documented(
            "GET",
            "/v1/auth-user/whatsapp/tickets/:id",
            "WhatsApp — Tickets",
        ),
        documented(
            "PATCH",
            "/v1/auth-user/whatsapp/tickets/:id",
            "WhatsApp — Tickets",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/conversations/:id/transfer-and-ticket",
            "WhatsApp — Tickets",
        ),
        documented(
            "POST",
            "/v1/auth-user/whatsapp/messages/:id/react",
            "WhatsApp — Messages",
        ),
    ];

    #[test]
    fn mod_rs_route_inventory_matches_expected() {
        let actual = route_inventory_from_source();
        let expected: Vec<(String, String)> = EXPECTED_ROUTE_INVENTORY
            .iter()
            .map(|entry| (entry.method.to_string(), entry.path.to_string()))
            .collect();

        assert_eq!(
            actual, expected,
            "WhatsApp route inventory drifted; update the expected inventory only after confirming contract parity.",
        );
    }

    #[test]
    fn openapi_whatsapp_inventory_matches_expected() {
        let openapi = serde_json::to_value(crate::openapi::ApiDoc::openapi())
            .expect("serialize OpenAPI document");
        let paths = openapi
            .get("paths")
            .and_then(Value::as_object)
            .expect("OpenAPI paths object");

        assert_eq!(
            openapi_whatsapp_inventory(paths),
            expected_openapi_inventory(),
            "WhatsApp OpenAPI path/method parity drifted; inspect /docs/openapi.json before accepting changes.",
        );

        for entry in EXPECTED_ROUTE_INVENTORY
            .iter()
            .filter(|entry| entry.documented)
        {
            let doc_path = route_path_to_openapi(entry.path);
            let operation = paths
                .get(&doc_path)
                .and_then(Value::as_object)
                .and_then(|path_item| path_item.get(&entry.method.to_ascii_lowercase()))
                .and_then(Value::as_object)
                .unwrap_or_else(|| {
                    panic!(
                        "missing OpenAPI operation for {} {}",
                        entry.method, doc_path
                    )
                });

            let expected_tag = entry.tag.expect("documented routes must declare tags");
            let actual_tags: Vec<&str> = operation
                .get("tags")
                .and_then(Value::as_array)
                .expect("documented routes must expose tags")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            assert_eq!(
                actual_tags,
                vec![expected_tag],
                "tag drift for {} {}",
                entry.method,
                doc_path,
            );

            let has_bearer_auth = operation
                .get("security")
                .and_then(Value::as_array)
                .expect("documented routes must expose security")
                .iter()
                .any(|item| {
                    item.as_object()
                        .is_some_and(|object| object.contains_key("bearerAuth"))
                });
            assert!(
                has_bearer_auth,
                "missing bearerAuth security for {} {}",
                entry.method, doc_path,
            );
        }
    }

    fn route_inventory_from_source() -> Vec<(String, String)> {
        let route_re =
            Regex::new(r#"(?s)\.route\(\s*"([^"]+)"\s*,\s*(get|post|put|patch|delete)\("#)
                .expect("compile route inventory regex");

        route_re
            .captures_iter(include_str!("mod.rs"))
            .map(|capture| (capture[2].to_ascii_uppercase(), capture[1].to_string()))
            .collect()
    }

    fn expected_openapi_inventory() -> BTreeMap<String, BTreeSet<String>> {
        let mut inventory = BTreeMap::<String, BTreeSet<String>>::new();

        for entry in EXPECTED_ROUTE_INVENTORY
            .iter()
            .filter(|entry| entry.documented)
        {
            inventory
                .entry(route_path_to_openapi(entry.path))
                .or_default()
                .insert(entry.method.to_ascii_lowercase());
        }

        inventory
    }

    fn openapi_whatsapp_inventory(
        paths: &serde_json::Map<String, Value>,
    ) -> BTreeMap<String, BTreeSet<String>> {
        const HTTP_METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

        paths
            .iter()
            .filter(|(path, _)| {
                (path.starts_with("/v1/auth-user/whatsapp")
                    || path.starts_with("/v1/admin/whatsapp-campaigns"))
                    && !path.starts_with("/v1/auth-user/whatsapp/ai-agent")
                    && !path.starts_with("/v1/auth-user/whatsapp/ai/")
            })
            .map(|(path, item)| {
                let methods = item
                    .as_object()
                    .expect("OpenAPI path item object")
                    .keys()
                    .filter(|key| HTTP_METHODS.contains(&key.as_str()))
                    .cloned()
                    .collect::<BTreeSet<_>>();

                (path.clone(), methods)
            })
            .collect()
    }

    fn route_path_to_openapi(path: &str) -> String {
        path.split('/')
            .map(|segment| match segment.strip_prefix(':') {
                Some(param) => format!("{{{}}}", param),
                None => segment.to_string(),
            })
            .collect::<Vec<_>>()
            .join("/")
    }
}
