use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

use crate::{
    auth::{
        http_auth::{read_access_token, AuthAudience},
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
    let token = read_access_token(req.headers(), &state.config, AuthAudience::Staff)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let jwt_service = UserJwtService::new();
    let claims = jwt_service
        .verify_token(&token)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

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
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Ambos quedan disponibles en request extensions: `claims` (JWT snapshot)
    // para los handlers que sólo leen `id`, y `User` para los que necesiten
    // el rol/flags actuales (ej. CRUD de users).
    req.extensions_mut().insert(claims);
    req.extensions_mut().insert(user);

    Ok(next.run(req).await)
}
