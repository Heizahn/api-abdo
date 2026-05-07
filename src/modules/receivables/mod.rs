pub mod handler;

use crate::state::AppState;
use axum::{routing::get, Router};
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/receivable/me", get(handler::me_receivables_handler))
        .route(
            "/v1/receivable/me/paid",
            get(handler::me_paid_receivables_handler),
        )
        .route(
            "/v1/receivable/:id",
            get(handler::get_receivable_by_id_handler),
        )
        .route(
            "/v1/receivable/:id/payments/rejected",
            get(handler::get_rejected_payments_by_receivable_handler),
        )
}
