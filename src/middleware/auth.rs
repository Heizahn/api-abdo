use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use crate::{
    auth::claims::Claims,
    crypto::jwt::{JwtCfg, JwtService},
    state::AppState,
};

/// Middleware de autenticación JWT
/// Valida el token JWT y lo inyecta en las extensiones del request
pub async fn jwt_auth_middleware(
    State(_state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Extraer header Authorization
    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Extraer token del formato "Bearer <token>"
    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Verificar JWT
    let jwt = JwtService::new(JwtCfg::from_env());
    let claims = jwt
        .decode_encrypted_verbose(token)
        .map_err(|e| {
            tracing::error!("JWT verification failed: {:?}", e);
            StatusCode::UNAUTHORIZED
        })?;

    // Verificar expiración
    if claims.exp < JwtService::now() {
        tracing::warn!("JWT expired for user: {}", claims.sub);
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Inyectar claims en request extensions para que handlers los usen
    req.extensions_mut().insert(claims);

    // Continuar con el siguiente middleware/handler
    Ok(next.run(req).await)
}

/// Extractor de claims desde request extensions
/// Se usa en handlers protegidos
pub struct AuthUser(pub Claims);

#[axum::async_trait]
impl<S> axum::extract::FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Claims>()
            .cloned()
            .map(AuthUser)
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}
