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
    handlers::{auth, profile},
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
    let general_rate_limit = rate_limit::create_rate_limiter(
        state.config.rate_limit_per_second,
        state.config.rate_limit_burst,
    );

    let auth_rate_limit =
        rate_limit::create_auth_rate_limiter(state.config.rate_limit_auth_per_minute);

    // ✅ RUTAS PÚBLICAS (sin JWT)
    let public_routes = Router::new()
        .route("/v1/auth/verify_number", post(auth::verify_number_handler))
        .route("/v1/auth/login", post(auth::login_handler))
        .route("/v1/auth/refresh", post(auth::refresh_handler))
        .layer(auth_rate_limit);

    // ✅ RUTAS PROTEGIDAS (con JWT)
    let protected_routes = Router::new()
        .route("/v1/profile/me", get(profile::me_handler))
        .route("/v1/profile/me/balance", get(profile::me_balance_handler))
        .route(
            "/v1/profile/me/last_payments",
            get(profile::me_last_payments_handler),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            jwt_auth_middleware,
        ));

    // ✅ ROUTER PRINCIPAL: merge + state al final
    public_routes
        .merge(protected_routes)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CompressionLayer::new())
                .layer(cors)
                .layer(general_rate_limit),
        )
        .with_state(state)
}
