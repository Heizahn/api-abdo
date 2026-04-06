use axum::{extract::State, Extension, Json};
use bcrypt::verify;
use mongodb::bson::oid::ObjectId;
use std::sync::Arc;

use crate::{
    auth::user_jwt::{UserJwtService, UserProfileClaims},
    db::{SalesRepository, UserRepository},
    error::ApiError,
    models::{
        payment::{
            CheckReferenceData, CheckReferenceRequest, CheckReferenceResponse, ReferenceDetails,
        },
        users::{
            RefreshTokenRequest, RefreshTokenResponse, User, UserLoginRequest, UserLoginResponse,
            UserResponse,
        },
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
        .generate_token(&user.id, &user.name, user.role)
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

    // Decodificar ignorando expiración — solo valida la firma.
    // Si la firma es inválida (token manipulado) sí rechaza.
    let claims = jwt_service
        .decode_ignoring_exp(&payload.token)
        .map_err(|_| ApiError::Unauthorized("Token inválido".to_string()))?;

    // 2. Check if user still exists
    let user = get_user_by_id(&state, &claims.id)
        .await?
        .ok_or(ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    // 3. Generate new token
    let new_token = jwt_service
        .generate_token(&user.id, &user.name, user.role)
        .map_err(|e| ApiError::Internal(e))?;

    Ok(Json(RefreshTokenResponse { token: new_token }))
}

/// POST /v1/auth-user/payments/check-reference
pub async fn check_reference_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CheckReferenceRequest>,
) -> Result<Json<CheckReferenceResponse>, ApiError> {
    let id_client = ObjectId::parse_str(&payload.id_client)
        .map_err(|_| ApiError::BadRequest("idClient inválido".to_string()))?;

    let result = state
        .db
        .check_reference(&id_client, &payload.s_reference)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let response = match result {
        None => CheckReferenceResponse {
            ok: true,
            message: "Referencia disponible".to_string(),
            data: CheckReferenceData {
                status: "available".to_string(),
                source: None,
                details: None,
            },
        },
        Some(info) if info.is_same_client => CheckReferenceResponse {
            ok: true,
            message: "El cliente ya tiene esta referencia registrada".to_string(),
            data: CheckReferenceData {
                status: "duplicate_own_client".to_string(),
                source: Some(info.source),
                details: Some(ReferenceDetails {
                    s_name: None,
                    s_reference: info.s_reference,
                    n_amount: info.n_amount,
                    n_bs: info.n_bs,
                    s_state: info.s_state,
                }),
            },
        },
        Some(info) => CheckReferenceResponse {
            ok: true,
            message: "La referencia ya existe registrada para otro cliente".to_string(),
            data: CheckReferenceData {
                status: "duplicate_other_client".to_string(),
                source: Some(info.source),
                details: Some(ReferenceDetails {
                    s_name: info.s_name,
                    s_reference: info.s_reference,
                    n_amount: info.n_amount,
                    n_bs: info.n_bs,
                    s_state: info.s_state,
                }),
            },
        },
    };

    Ok(Json(response))
}

/// GET /v1/auth-user/me
pub async fn me_handler(
    Extension(claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<UserResponse>, ApiError> {
    let user = get_user_by_id(&state, &claims.id)
        .await?
        .ok_or(ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    Ok(Json(UserResponse::from(user)))
}
