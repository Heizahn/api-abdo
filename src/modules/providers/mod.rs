pub mod handler;

use crate::state::AppState;
use axum::{routing::get, Router};
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/users/providers", get(handler::get_providers_handler))
        .route("/v1/users/agents", get(handler::get_agents_handler))
}
