pub mod handler;

use axum::{routing::{get, post}, Router};
use crate::state::AppState;
use std::sync::Arc;

/// Rutas protegidas con JWT de cliente
pub fn client_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/payments/methods/payment/:debt_id", get(handler::get_pago_movil_data_handler))
        .route("/v1/payments/methods/payment/by-client/:client_id", get(handler::get_pago_movil_data_by_client_handler))
        .route("/v1/payments/payment/report", post(handler::report_payment_handler))
}

/// Rutas protegidas con JWT de usuario (admin/staff)
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/auth-user/payments/report", post(handler::report_payment_user_handler))
        .route("/v1/auth-user/payments/methods/by-client/:client_id", get(handler::get_pago_movil_data_by_client_user_handler))
}
