pub mod handler;

use crate::state::AppState;
use axum::{routing::get, Router};
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/auth-user/dashboard/monthly-closing",
            get(handler::monthly_closing_handler),
        )
        .route(
            "/v1/auth-user/dashboard/solvency",
            get(handler::solvency_handler),
        )
        .route(
            "/v1/auth-user/dashboard/latest-payments",
            get(handler::latest_payments_handler),
        )
}
