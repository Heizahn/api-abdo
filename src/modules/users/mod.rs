pub mod handler;

use axum::{
    routing::{get, patch, post},
    Router,
};
use std::sync::Arc;

use crate::state::AppState;

/// Rutas CRUD de usuarios. Todas requieren JWT staff válido + rol SUPERADMIN
/// (`nRole == 0.0` leído de DB en cada request, no del JWT snapshot).
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/auth-user/users", get(handler::list_users_handler))
        .route("/v1/auth-user/users", post(handler::create_user_handler))
        .route(
            "/v1/auth-user/users/:id/visible",
            patch(handler::set_user_visible_handler),
        )
        .route(
            "/v1/auth-user/users/:id",
            patch(handler::update_user_handler),
        )
        .route(
            "/v1/auth-user/users/:id/password",
            patch(handler::set_user_password_handler),
        )
}
