pub mod handler;
pub mod service;

use crate::state::AppState;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;

/// Rutas protegidas con JWT de cliente
pub fn client_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/payments/methods/payment/:debt_id",
            get(handler::get_pago_movil_data_handler),
        )
        .route(
            "/v1/payments/methods/payment/by-client/:client_id",
            get(handler::get_pago_movil_data_by_client_handler),
        )
        .route(
            "/v1/payments/payment/report",
            post(handler::report_payment_handler),
        )
}

/// Rutas protegidas con JWT de usuario (admin/staff)
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/auth-user/payments/report",
            post(handler::report_payment_user_handler),
        )
        .route(
            "/v1/auth-user/payments/methods/by-client/:client_id",
            get(handler::get_pago_movil_data_by_client_user_handler),
        )
        // T20 — list payment reports
        .route(
            "/v1/auth-user/payments-reports",
            get(handler::list_payment_reports_handler),
        )
        // T21 — approve a payment report
        .route(
            "/v1/auth-user/payments-reports/:id/approve",
            post(handler::approve_payment_report_handler),
        )
        // T22 — reject a payment report
        .route(
            "/v1/auth-user/payments-reports/:id/reject",
            post(handler::reject_payment_report_handler),
        )
}
