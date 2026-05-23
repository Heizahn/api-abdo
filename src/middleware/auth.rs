use crate::{
    auth::http_auth::{auth_input_debug, read_access_token, AuthAudience},
    crypto::jwt::{JwtCfg, JwtService},
    state::AppState,
};
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

/// Middleware de autenticación JWT
pub async fn jwt_auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let route = req.uri().path().to_string();
    let auth_debug = auth_input_debug(req.headers(), AuthAudience::Client);
    tracing::debug!(
        target: "auth",
        route = %route,
        audience = "client",
        has_authorization_header = auth_debug.has_authorization_header,
        has_cookie_header = auth_debug.has_cookie_header,
        has_access_cookie = auth_debug.has_access_cookie,
        has_bearer_token = auth_debug.has_bearer_token,
        "Procesando autenticación JWT (cliente)"
    );

    let token =
        read_access_token(req.headers(), &state.config, AuthAudience::Client).ok_or_else(|| {
            tracing::warn!(
                target: "auth",
                route = %route,
                audience = "client",
                has_authorization_header = auth_debug.has_authorization_header,
                has_cookie_header = auth_debug.has_cookie_header,
                has_access_cookie = auth_debug.has_access_cookie,
                has_bearer_token = auth_debug.has_bearer_token,
                "Missing auth token (cookie/header)"
            );
            StatusCode::UNAUTHORIZED
        })?;

    // Verificar JWT
    let jwt = JwtService::new(JwtCfg::from_env());
    let claims = jwt.decode_encrypted_verbose(&token).map_err(|e| {
        tracing::error!(
            target: "auth",
            route = %route,
            audience = "client",
            error = ?e,
            "JWT verification failed"
        );
        StatusCode::UNAUTHORIZED
    })?;

    tracing::debug!(
        target: "auth",
        route = %route,
        audience = "client",
        user_id = %claims.sub,
        "JWT válido"
    );

    // Verificar expiración
    if claims.exp < JwtService::now() {
        tracing::warn!(
            target: "auth",
            route = %route,
            audience = "client",
            user_id = %claims.sub,
            "JWT expired"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    tracing::debug!(
        "✅ JWT no expirado (exp: {}, now: {})",
        claims.exp,
        JwtService::now()
    );

    // ✅ CRÍTICO: Insertar claims ANTES de llamar next
    let user_id = claims.sub.clone();
    req.extensions_mut().insert(claims);

    tracing::debug!(
        target: "auth",
        route = %route,
        audience = "client",
        user_id = %user_id,
        "Claims insertados en extensions"
    );
    tracing::info!(
        target: "auth",
        route = %route,
        audience = "client",
        user_id = %user_id,
        "Autenticación exitosa"
    );

    // Continuar con el siguiente middleware/handler
    Ok(next.run(req).await)
}
