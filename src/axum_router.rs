use axum::http::HeaderValue;
use axum::{middleware, Router};
use axum_client_ip::SecureClientIpSource;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::CorsLayer,
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
    let mut cors = CorsLayer::new()
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::PATCH,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::ACCEPT,
            axum::http::header::AUTHORIZATION,
            axum::http::header::CACHE_CONTROL,
            axum::http::header::CONTENT_TYPE,
            axum::http::header::ORIGIN,
            axum::http::header::PRAGMA,
            axum::http::header::COOKIE,
            axum::http::header::HeaderName::from_static("idempotency-key"),
            axum::http::header::HeaderName::from_static("x-requested-with"),
            axum::http::header::HeaderName::from_static("x-refresh-token"),
            axum::http::header::HeaderName::from_static("x-csrf-token"),
            axum::http::header::HeaderName::from_static("x-client-version"),
        ]);

    if state.config.frontend_origins.is_empty() {
        cors = cors.allow_origin(tower_http::cors::Any);
    } else {
        let origins: Vec<HeaderValue> = state
            .config
            .frontend_origins
            .iter()
            .filter_map(|o| o.parse::<HeaderValue>().ok())
            .collect();
        if origins.is_empty() {
            tracing::warn!(
                "FRONTEND_ORIGINS no tiene valores válidos; usando allow_origin(Any) temporalmente"
            );
            cors = cors.allow_origin(tower_http::cors::Any);
        } else {
            cors = cors.allow_origin(origins);
        }
    }

    if state.config.cors_allow_credentials {
        if state.config.frontend_origins.is_empty() {
            tracing::warn!(
                "CORS_ALLOW_CREDENTIALS=true pero FRONTEND_ORIGINS vacío; se omite allow_credentials para evitar '*' con credenciales"
            );
        } else {
            cors = cors.allow_credentials(true);
        }
    }

    let auth_rate_limit_client =
        rate_limit::create_auth_rate_limiter(state.config.rate_limit_auth_per_minute);
    let auth_rate_limit_admin =
        rate_limit::create_auth_rate_limiter(state.config.rate_limit_auth_per_minute);

    // Rutas públicas de cliente (móvil/webview)
    let client_public = Router::new()
        .merge(auth_client::routes())
        .merge(calculations::routes())
        .merge(api_utils::public_routes())
        .layer(auth_rate_limit_client);

    // Rutas públicas de admin (login/refresh)
    let admin_public = Router::new()
        .merge(auth_user::public_routes())
        .layer(auth_rate_limit_admin);

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

    Router::new()
        .merge(client_public)
        .merge(client_protected)
        .merge(admin_public)
        .merge(user_protected)
        .merge(api_utils::static_routes())
        .merge(webhook)
        .merge(ws)
        .merge(SwaggerUi::new("/docs").url("/docs/openapi.json", ApiDoc::openapi()))
        .layer(
            ServiceBuilder::new()
                .layer(SecureClientIpSource::RightmostXForwardedFor.into_extension())
                .layer(TraceLayer::new_for_http())
                .layer(CompressionLayer::new()),
        )
        .layer(cors)
        .with_state(state)
}
