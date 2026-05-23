use axum::{middleware, Router};
use axum_client_ip::SecureClientIpSource;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::{
    middleware::{auth::jwt_auth_middleware, auth_user::user_jwt_auth_middleware, rate_limit},
    modules::{
        ai_agent, api_utils, auth_client, auth_user, calculations, clients, dashboard, payments,
        profile, providers, receivables, users, whatsapp,
    },
    openapi::ApiDoc,
    state::AppState,
};

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let auth_rate_limit =
        rate_limit::create_auth_rate_limiter(state.config.rate_limit_auth_per_minute);

    // Rutas públicas con rate limit
    let public = Router::new()
        .merge(auth_client::routes())
        .merge(auth_user::public_routes())
        .merge(calculations::routes())
        .merge(api_utils::public_routes())
        .layer(auth_rate_limit);

    // Webhook de WhatsApp (público, sin rate limit — Meta reenvía si recibe != 200)
    let webhook = whatsapp::webhook_routes();

    // WebSocket (público en router, JWT validado internamente via query param)
    let ws = whatsapp::ws_routes();

    // Rutas protegidas con JWT de staff/admin
    let user_protected = Router::new()
        .merge(auth_user::protected_routes())
        .merge(clients::routes())
        .merge(dashboard::routes())
        .merge(providers::routes())
        .merge(api_utils::user_routes())
        .merge(payments::user_routes())
        .merge(whatsapp::user_routes())
        .merge(users::user_routes())
        .merge(ai_agent::user_routes())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            user_jwt_auth_middleware,
        ));

    // Rutas protegidas con JWT de cliente
    let client_protected = Router::new()
        .merge(profile::routes())
        .merge(receivables::routes())
        .merge(payments::client_routes())
        .merge(api_utils::client_routes())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            jwt_auth_middleware,
        ));

    public
        .merge(webhook)
        .merge(ws)
        .merge(user_protected)
        .merge(client_protected)
        .merge(api_utils::static_routes())
        .merge(SwaggerUi::new("/docs").url("/docs/openapi.json", ApiDoc::openapi()))
        .layer(
            ServiceBuilder::new()
                .layer(SecureClientIpSource::RightmostXForwardedFor.into_extension())
                .layer(TraceLayer::new_for_http())
                .layer(CompressionLayer::new())
                .layer(cors),
        )
        .with_state(state)
}
