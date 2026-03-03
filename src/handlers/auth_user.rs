use axum::{extract::State, Extension, Json};
use bcrypt::verify;
use std::sync::Arc;

use crate::{
    auth::user_jwt::{UserJwtService, UserProfileClaims},
    db::UserRepository,
    error::ApiError,
    models::users::{
        RefreshTokenRequest, RefreshTokenResponse, User, UserLoginRequest, UserLoginResponse,
    },
    state::AppState,
};

/// POST /v1/auth-user/login
pub async fn login_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<UserLoginRequest>,
) -> Result<Json<UserLoginResponse>, ApiError> {
    // 1. Find user by email
    let user = state
        .db
        .find_user_by_email(&payload.email)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    // 2. Find credentials
    let creds = state
        .db
        .find_user_credentials_by_user_id(&user.id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::Unauthorized(
            "Usuario no tiene credenciales".to_string(),
        ))?;

    // 3. Verify password
    let valid =
        verify(&payload.password, &creds.password).map_err(|_| ApiError::InternalServerError)?;

    if !valid {
        return Err(ApiError::Unauthorized(
            "Credenciales incorrectas".to_string(),
        ));
    }

    // 4. Generate Token
    let jwt_service = UserJwtService::new();
    let token = jwt_service
        .generate_token(&user.id, &user.name)
        .map_err(|e| ApiError::Internal(e))?;

    Ok(Json(UserLoginResponse { token }))
}

async fn get_user_by_id(state: &Arc<AppState>, id: &str) -> Result<Option<User>, ApiError> {
    state
        .db
        .find_user_by_id(id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))
}

/// POST /v1/auth-user/refresh-token
pub async fn refresh_token_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<RefreshTokenRequest>,
) -> Result<Json<RefreshTokenResponse>, ApiError> {
    let jwt_service = UserJwtService::new();

    // 1. Verify old token (even if expired? LB4 verifyToken throws if expired usually)
    let claims = jwt_service
        .verify_token(&payload.token)
        .map_err(|_| ApiError::Unauthorized("Token inválido".to_string()))?;

    // 2. Check if user still exists
    let user = get_user_by_id(&state, &claims.id)
        .await?
        .ok_or(ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    // 3. Generate new token
    let new_token = jwt_service
        .generate_token(&user.id, &user.name)
        .map_err(|e| ApiError::Internal(e))?;

    Ok(Json(RefreshTokenResponse { token: new_token }))
}

/// GET /v1/auth-user/me
pub async fn me_handler(
    Extension(claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<User>, ApiError> {
    let user = get_user_by_id(&state, &claims.id)
        .await?
        .ok_or(ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    Ok(Json(user))
}
