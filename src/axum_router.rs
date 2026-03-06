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
    handlers::{auth, auth_user, calculation, clients, dashboard, payment, profile, providers, receivable, utils},
    middleware::{auth::jwt_auth_middleware, auth_user::user_jwt_auth_middleware, rate_limit},
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
        .route("/v2/utils/calculate", post(calculation::calculate_handler))
        .route("/v1/utils/ping", get(utils::get_ping_response))
        .route(
            "/v1/utils/latest-version",
            get(utils::get_latest_version_response),
        )
        .layer(auth_rate_limit.clone()); // Cloned to reuse

    // ✅ AUTH USER RUTAS (Admin/Staff)
    let auth_user_public = Router::new()
        .route("/v1/auth-user/login", post(auth_user::login_handler))
        .route(
            "/v1/auth-user/refresh-token",
            post(auth_user::refresh_token_handler),
        )
        .layer(auth_rate_limit); // Reused rate limiter

    let auth_user_protected = Router::new()
        .route("/v1/auth-user/me", get(auth_user::me_handler))
        .route(
            "/v1/auth-user/clients/all",
            get(clients::get_all_clients_handler),
        )
        .route(
            "/v1/auth-user/clients/:id",
            get(clients::get_client_by_id_handler),
        )
        .route(
            "/v1/auth-user/dashboard/monthly-closing",
            get(dashboard::monthly_closing_handler),
        )
        .route(
            "/v1/auth-user/dashboard/solvency",
            get(dashboard::solvency_handler),
        )
        .route(
            "/v1/auth-user/dashboard/latest-payments",
            get(dashboard::latest_payments_handler),
        )
        .route("/v1/users/providers", get(providers::get_providers_handler))
        .route("/v1/utils/bcv", get(utils::get_bcv))
        .route("/v1/utils/ip-pppoe/:sn", get(utils::get_ip_pppoe))
        .route("/v1/utils/image/:filename", get(utils::get_image))
        .route("/v1/utils/zabbix/:id_client", get(utils::get_zabbix))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            user_jwt_auth_middleware,
        ));

    // ✅ RUTAS PROTEGIDAS (con JWT Clientes)
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
            "/v1/payments/methods/payment/by-client/:client_id",
            get(payment::get_pago_movil_data_by_client_handler),
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
        .merge(auth_user_public)
        .merge(auth_user_protected)
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
