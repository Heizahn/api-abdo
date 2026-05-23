use axum::{
    extract::State,
    http::{header::SET_COOKIE, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use bcrypt::verify;
use mongodb::bson::oid::ObjectId;
use std::sync::Arc;
use time::OffsetDateTime;

use crate::{
    auth::{
        http_auth::{
            build_auth_cookie, build_clear_cookie, read_staff_refresh_token, AuthAudience,
            STAFF_ACCESS_COOKIE, STAFF_REFRESH_COOKIE,
        },
        user_jwt::{UserJwtService, UserProfileClaims},
    },
    cache::RefreshSessionRotateOutcome,
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

#[utoipa::path(
    post,
    path = "/v1/auth-user/login",
    tag = "Auth — Staff",
    request_body = UserLoginRequest,
    responses(
        (status = 200, description = "Login exitoso", body = UserLoginResponse),
        (status = 401, description = "Credenciales incorrectas o usuario sin acceso"),
    )
)]
pub async fn login_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<UserLoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let user = state
        .db
        .find_user_by_email(&payload.email)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    // Usuarios "sin acceso" (nRole == -1) no pueden loggear ni refrescar.
    // Diferente de `visible == false` (ese sólo los oculta de listados).
    if user.role == -1.0 {
        return Err(ApiError::Unauthorized("Usuario sin acceso".to_string()));
    }

    let creds = state
        .db
        .find_user_credentials_by_user_id(&user.id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::Unauthorized(
            "Usuario no tiene credenciales".to_string(),
        ))?;

    let valid =
        verify(&payload.password, &creds.password).map_err(|_| ApiError::InternalServerError)?;

    if !valid {
        return Err(ApiError::Unauthorized(
            "Credenciales incorrectas".to_string(),
        ));
    }

    let jwt_service = UserJwtService::new();
    let family = uuid::Uuid::new_v4().to_string();
    let (access_token, access_exp) = jwt_service
        .generate_access_token(&user.id, &user.name, user.role)
        .map_err(|e| ApiError::Internal(e))?;
    let (refresh_token, refresh_exp, jti) = jwt_service
        .generate_refresh_token(&user.id, &family)
        .map_err(|e| ApiError::Internal(e))?;

    state
        .redis
        .set_refresh_session(
            AuthAudience::Staff.redis_realm(),
            &family,
            &user.id,
            &jti,
            ttl_from_exp(refresh_exp),
        )
        .await
        .map_err(ApiError::from)?;

    let response_body = UserLoginResponse {
        token: access_token.clone(),
        access_token: access_token.clone(),
        refresh_token: refresh_token.clone(),
        access_exp,
        refresh_exp,
    };

    Ok(response_with_staff_auth_cookies(
        &state,
        response_body,
        &access_token,
        access_exp,
        &refresh_token,
        refresh_exp,
    ))
}

async fn get_user_by_id(state: &Arc<AppState>, id: &str) -> Result<Option<User>, ApiError> {
    state
        .db
        .find_user_by_id(id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/refresh-token",
    tag = "Auth — Staff",
    request_body = RefreshTokenRequest,
    responses(
        (status = 200, description = "Token renovado", body = RefreshTokenResponse),
        (status = 401, description = "Token inválido o usuario sin acceso"),
    )
)]
pub async fn refresh_token_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    payload: Option<Json<RefreshTokenRequest>>,
) -> Result<Response, ApiError> {
    let jwt_service = UserJwtService::new();

    let body_token = payload
        .as_ref()
        .and_then(|p| p.refresh_token.as_deref().or(p.token.as_deref()));

    let raw_refresh =
        match read_staff_refresh_token(&headers, &state.config, body_token) {
            Some(v) => v,
            None => {
                return Ok(refresh_error_response(
                    &state,
                    "invalid_refresh_token",
                    "No se encontró refresh token",
                ));
            }
        };

    let claims = match jwt_service.verify_refresh_token(&raw_refresh) {
        Ok(v) => v,
        Err(_) => {
            return Ok(refresh_error_response(
                &state,
                "invalid_refresh_token",
                "Refresh token inválido",
            ));
        }
    };

    let user = get_user_by_id(&state, &claims.id)
        .await?
        .ok_or(ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    // Misma regla que el login: usuarios sin acceso no pueden refrescar el token.
    if user.role == -1.0 {
        return Err(ApiError::Unauthorized("Usuario sin acceso".to_string()));
    }

    let family = claims
        .fam
        .clone()
        .unwrap_or_else(|| format!("legacy-{}", claims.id));

    let (access_token, access_exp) = jwt_service
        .generate_access_token(&user.id, &user.name, user.role)
        .map_err(|e| ApiError::Internal(e))?;
    let (refresh_token, refresh_exp, new_jti) = jwt_service
        .generate_refresh_token(&user.id, &family)
        .map_err(|e| ApiError::Internal(e))?;

    let rotate_result = state
        .redis
        .rotate_refresh_session(
            AuthAudience::Staff.redis_realm(),
            &family,
            &user.id,
            &claims.jti,
            &new_jti,
            ttl_from_exp(refresh_exp),
        )
        .await
        .map_err(ApiError::from)?;

    match rotate_result {
        RefreshSessionRotateOutcome::Rotated => {
            let response_body = RefreshTokenResponse {
                token: access_token.clone(),
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
                access_exp,
                refresh_exp,
            };
            Ok(response_with_staff_auth_cookies(
                &state,
                response_body,
                &access_token,
                access_exp,
                &refresh_token,
                refresh_exp,
            ))
        }
        RefreshSessionRotateOutcome::Missing => Ok(refresh_error_response(
            &state,
            "session_expired",
            "La sesión expiró, inicia sesión nuevamente",
        )),
        RefreshSessionRotateOutcome::Stale => Ok(refresh_soft_error_response(
            "refresh_in_progress",
            "Se detectó una renovación concurrente; reintenta la solicitud",
        )),
        RefreshSessionRotateOutcome::ReuseDetected => Ok(refresh_error_response(
            &state,
            "refresh_token_reused",
            "Se detectó reutilización de refresh token; inicia sesión nuevamente",
        )),
    }
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/payments/check-reference",
    tag = "Payments",
    security(("bearerAuth" = [])),
    request_body = CheckReferenceRequest,
    responses(
        (status = 200, description = "Resultado de la búsqueda de referencia (available | duplicate_own_client | duplicate_other_client)", body = CheckReferenceResponse),
        (status = 400, description = "idClient inválido"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn check_reference_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CheckReferenceRequest>,
) -> Result<Json<CheckReferenceResponse>, ApiError> {
    let id_client = ObjectId::parse_str(&payload.id_client)
        .map_err(|_| ApiError::BadRequest("idClient inválido".to_string()))?;

    let result = state
        .db
        .check_reference(&id_client, &payload.s_reference, None)
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

#[utoipa::path(
    get,
    path = "/v1/auth-user/me",
    tag = "Auth — Staff",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Perfil del usuario autenticado", body = UserResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn me_handler(
    Extension(claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<UserResponse>, ApiError> {
    let user = get_user_by_id(&state, &claims.id)
        .await?
        .ok_or(ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    Ok(Json(UserResponse::from(user)))
}

fn response_with_staff_auth_cookies<T: serde::Serialize>(
    state: &Arc<AppState>,
    body: T,
    access_token: &str,
    access_exp: i64,
    refresh_token: &str,
    refresh_exp: i64,
) -> Response {
    let mut response = Json(body).into_response();
    push_set_cookie(
        response.headers_mut(),
        build_auth_cookie(
            &state.config,
            STAFF_ACCESS_COOKIE,
            access_token,
            ttl_from_exp(access_exp) as i64,
            "/",
        ),
    );
    push_set_cookie(
        response.headers_mut(),
        build_auth_cookie(
            &state.config,
            STAFF_REFRESH_COOKIE,
            refresh_token,
            ttl_from_exp(refresh_exp) as i64,
            "/v1/auth-user",
        ),
    );
    response
}

fn refresh_error_response(state: &Arc<AppState>, code: &str, message: &str) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "ok": false,
            "error": code,
            "code": code,
            "message": message
        })),
    )
        .into_response();

    push_set_cookie(
        response.headers_mut(),
        build_clear_cookie(&state.config, STAFF_ACCESS_COOKIE, "/"),
    );
    push_set_cookie(
        response.headers_mut(),
        build_clear_cookie(&state.config, STAFF_REFRESH_COOKIE, "/v1/auth-user"),
    );
    response
}

/// Error de refresh sin limpieza de cookies (p.ej. race benigno).
/// Evita desloguear al usuario cuando hubo dos refresh concurrentes y uno de
/// ellos ya rotó correctamente la sesión.
fn refresh_soft_error_response(code: &str, message: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "ok": false,
            "error": code,
            "code": code,
            "message": message
        })),
    )
        .into_response()
}

fn push_set_cookie(headers: &mut HeaderMap, cookie: String) {
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        headers.append(SET_COOKIE, value);
    }
}

fn ttl_from_exp(exp: i64) -> u64 {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    exp.saturating_sub(now).max(1) as u64
}
