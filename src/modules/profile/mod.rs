pub mod handler;

use axum::{routing::get, Router};
use crate::state::AppState;
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/profile/me/group", get(handler::me_group_handler))
        .route("/v1/profile/me/phone", get(handler::me_phone_handler))
}
