pub mod handler;

use axum::{routing::post, Router};
use crate::state::AppState;
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/auth/verify_number", post(handler::verify_number_handler))
        .route("/v1/auth/login", post(handler::login_handler))
        .route("/v1/auth/refresh", post(handler::refresh_handler))
}
