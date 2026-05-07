pub mod handler;

use crate::state::AppState;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;

pub fn public_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/auth-user/login", post(handler::login_handler))
        .route(
            "/v1/auth-user/refresh-token",
            post(handler::refresh_token_handler),
        )
}

pub fn protected_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/auth-user/me", get(handler::me_handler))
        .route(
            "/v1/auth-user/payments/check-reference",
            post(handler::check_reference_handler),
        )
}
