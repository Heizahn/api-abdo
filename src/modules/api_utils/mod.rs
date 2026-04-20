pub mod handler;

use axum::{routing::get, Router};
use crate::state::AppState;
use std::sync::Arc;

/// Rutas públicas (sin JWT, con rate limit)
pub fn public_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/utils/ping", get(handler::get_ping_response))
        .route("/v1/utils/latest-version", get(handler::get_latest_version_response))
}

/// Rutas protegidas con JWT de usuario (admin/staff)
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/utils/bcv", get(handler::get_bcv))
        .route("/v1/utils/ip-pppoe/:sn", get(handler::get_ip_pppoe))
        .route("/v1/utils/image/:filename", get(handler::get_image))
        .route("/v1/utils/zabbix/:id_client", get(handler::get_zabbix))
        .route("/v1/auth-user/utils/list/banks", get(handler::get_bank_list_user))
}

/// Rutas protegidas con JWT de cliente
pub fn client_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/utils/list/banks", get(handler::get_bank_list))
}

/// Rutas estáticas (sin auth)
pub fn static_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/privacy-policy", get(handler::get_privacy_policy))
}
