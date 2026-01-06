use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

use crate::{
    handlers::{auth, calculation, payment, profile, receivable, utils},
    middleware::{auth::jwt_auth_middleware, rate_limit},
    state::AppState,
};

/// Construye el router completo de la aplicación
pub fn build_router(state: Arc<AppState>) -> Router {
    // CORS layer
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Rate limiters
    // let general_rate_limit = rate_limit::create_rate_limiter(
    //     state.config.rate_limit_per_second,
    //     state.config.rate_limit_burst,
    // );

    let auth_rate_limit =
        rate_limit::create_auth_rate_limiter(state.config.rate_limit_auth_per_minute);

    // ✅ RUTAS PÚBLICAS (sin JWT)
    let public_routes = Router::new()
        .route("/v1/auth/verify_number", post(auth::verify_number_handler))
        .route("/v1/auth/login", post(auth::login_handler))
        .route("/v1/auth/refresh", post(auth::refresh_handler))
        .route(
            "/v1/utils/calculate/bs",
            post(calculation::calculate_bs_handler),
        )
        .route("/v1/utils/ping", get(utils::get_ping_response))
        .route(
            "/v1/utils/latest-version",
            get(utils::get_latest_version_response),
        )
        .route("/v1/utils/image/:filename", get(utils::get_image))
        .layer(auth_rate_limit);

    // ✅ RUTAS PROTEGIDAS (con JWT)
    let protected_routes = Router::new()
        .route("/v1/profile/me/group", get(profile::me_group_handler))
        .route("/v1/profile/me/phone", get(profile::me_phone_handler))
        .route("/v1/receivable/me", get(receivable::me_receivables_handler))
        .route(
            "/v1/receivable/me/paid",
            get(receivable::me_paid_receivables_handler),
        )
        .route(
            "/v1/receivable/:id",
            get(receivable::get_receivable_by_id_handler),
        )
        .route(
            "/v1/payments/methods/payment/:debt_id",
            get(payment::get_pago_movil_data_handler),
        )
        .route(
            "/v1/payments/payment/report",
            post(payment::report_payment_handler),
        )
        .route("/v1/utils/list/banks", get(utils::get_bank_list))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            jwt_auth_middleware,
        ));

    let static_routes = Router::new().route("/v1/privacy-policy", get(utils::get_privacy_policy));

    // ✅ ROUTER PRINCIPAL: merge + state al final
    public_routes
        .merge(protected_routes)
        .merge(static_routes)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CompressionLayer::new())
                .layer(cors),
        )
        .with_state(state)
}
