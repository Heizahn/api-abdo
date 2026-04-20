pub mod handler;

use axum::{routing::get, Router};
use crate::state::AppState;
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/users/providers", get(handler::get_providers_handler))
}
