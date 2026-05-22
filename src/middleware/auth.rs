use crate::{
    auth::http_auth::{read_access_token, AuthAudience},
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
    tracing::debug!("🔐 JWT Middleware: Procesando autenticación");

    let token =
        read_access_token(req.headers(), &state.config, AuthAudience::Client).ok_or_else(|| {
            tracing::warn!("❌ Missing auth token (cookie/header)");
            StatusCode::UNAUTHORIZED
        })?;

    tracing::debug!(
        "🔐 Token extraído (primeros 20 chars): {}...",
        &token[..20.min(token.len())]
    );

    // Verificar JWT
    let jwt = JwtService::new(JwtCfg::from_env());
    let claims = jwt.decode_encrypted_verbose(&token).map_err(|e| {
        tracing::error!("❌ JWT verification failed: {:?}", e);
        StatusCode::UNAUTHORIZED
    })?;

    tracing::debug!("✅ JWT válido para user: {}", claims.sub);

    // Verificar expiración
    if claims.exp < JwtService::now() {
        tracing::warn!("❌ JWT expired for user: {}", claims.sub);
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

    tracing::debug!("✅ Claims insertados en extensions para user: {}", user_id);
    tracing::info!("✅ Autenticación exitosa para user: {}", user_id);

    // Continuar con el siguiente middleware/handler
    Ok(next.run(req).await)
}
