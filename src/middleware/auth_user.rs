use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

use crate::{
    auth::{
        http_auth::{auth_input_debug, read_staff_access_token, STAFF_ACCESS_COOKIE},
        user_jwt::UserJwtService,
    },
    db::UserRepository,
    state::AppState,
};

/// Rol sentinel para usuarios revocados: si `nRole == -1.0`, el user no puede
/// autenticarse ni hacer requests, aunque tenga un JWT vivo. Diferente de
/// `visible == false` (ese es sólo oculto en listados; conserva el acceso).
const NO_ACCESS_ROLE: f32 = -1.0;

pub async fn user_jwt_auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let route = req.uri().path().to_string();
    let auth_debug = auth_input_debug(req.headers(), STAFF_ACCESS_COOKIE);
    tracing::debug!(
        target: "auth",
        route = %route,
        audience = "staff",
        has_authorization_header = auth_debug.has_authorization_header,
        has_cookie_header = auth_debug.has_cookie_header,
        has_access_cookie = auth_debug.has_access_cookie,
        has_bearer_token = auth_debug.has_bearer_token,
        "Procesando autenticación JWT (staff)"
    );

    let token = read_staff_access_token(req.headers()).ok_or_else(|| {
            tracing::warn!(
                target: "auth",
                route = %route,
                audience = "staff",
                has_authorization_header = auth_debug.has_authorization_header,
                has_cookie_header = auth_debug.has_cookie_header,
                has_access_cookie = auth_debug.has_access_cookie,
                has_bearer_token = auth_debug.has_bearer_token,
                "Missing auth token (cookie/header)"
            );
            StatusCode::UNAUTHORIZED
        })?;

    let jwt_service = UserJwtService::new();
    let claims = jwt_service
        .verify_token(&token)
        .map_err(|err| {
            tracing::warn!(
                target: "auth",
                route = %route,
                audience = "staff",
                error = ?err,
                "JWT verification failed"
            );
            StatusCode::UNAUTHORIZED
        })?;

    // Gate "sin acceso": lee el rol vivo de DB. Un JWT emitido cuando el
    // user era válido deja de funcionar apenas le seteen `nRole = -1` en DB,
    // sin esperar al exp. El query va sobre el índice `_id` — ~5ms.
    let user = state
        .db
        .find_user_by_id(&claims.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if user.role == NO_ACCESS_ROLE {
        tracing::warn!(
            target: "auth",
            route = %route,
            audience = "staff",
            user_id = %claims.id,
            "Access denied by sentinel role"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Ambos quedan disponibles en request extensions: `claims` (JWT snapshot)
    // para los handlers que sólo leen `id`, y `User` para los que necesiten
    // el rol/flags actuales (ej. CRUD de users).
    req.extensions_mut().insert(claims);
    req.extensions_mut().insert(user);

    tracing::info!(
        target: "auth",
        route = %route,
        audience = "staff",
        "Autenticación exitosa"
    );

    Ok(next.run(req).await)
}
