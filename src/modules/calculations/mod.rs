pub mod handler;

use axum::{routing::post, Router};
use crate::state::AppState;
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/utils/calculate/bs", post(handler::calculate_bs_handler))
        .route("/v2/utils/calculate", post(handler::calculate_handler))
}
