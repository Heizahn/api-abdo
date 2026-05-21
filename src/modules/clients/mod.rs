pub mod handler;

use crate::state::AppState;
use axum::{routing::get, Router};
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/auth-user/clients/contact-info",
            get(handler::get_customers_info_handler),
        )
        .route(
            "/v1/auth-user/clients/all",
            get(handler::get_all_clients_handler),
        )
        .route(
            "/v1/auth-user/clients/:id",
            get(handler::get_client_by_id_handler),
        )
        .route(
            "/v1/clients/:id/status-history",
            get(handler::get_status_history_handler),
        )
}
