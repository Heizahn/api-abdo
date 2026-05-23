use axum::{
    extract::State,
    http::{header::SET_COOKIE, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

use crate::{
    auth::{
        http_auth::{
            build_auth_cookie, build_clear_cookie, read_refresh_token, AuthAudience,
            CLIENT_ACCESS_COOKIE, CLIENT_REFRESH_COOKIE,
        },
        service::AuthService,
    },
    cache::RefreshSessionRotateOutcome,
    crypto::jwt::{JwtCfg, JwtService},
    db::AuthRepository,
    error::ApiError,
    models::auth::*,
    state::AppState,
    utils::{
        generate_verification_code, sms::send_sms, timezone::VenezuelaDateTime,
        whatsapp::send_whatsapp_otp,
    },
};

#[utoipa::path(
    post,
    path = "/v1/auth/verify_number",
    tag = "Auth — Clientes",
    request_body = VerifyNumberRequest,
    responses(
        (status = 200, description = "Código enviado o número no encontrado", body = VerifyNumberResponse),
        (status = 500, description = "Error interno del servidor"),
    )
)]
pub async fn verify_number_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VerifyNumberRequest>,
) -> Result<Json<VerifyNumberResponse>, ApiError> {
    let now_vz = VenezuelaDateTime::now();
    tracing::info!(
        "[{}] verify_number request for phone: {}",
        now_vz.datetime_string_venezuela(),
        payload.phone
    );

    let found = AuthService::lookup_by_phone(&state.db, &payload.phone).await;

    if found.is_none() {
        tracing::info!("Phone {} not found in database", payload.phone);
        return Ok(Json(VerifyNumberResponse {
            ok: true,
            exists: false,
            phone: Some(payload.phone),
            message: None,
        }));
    }

    let code = generate_verification_code();

    state
        .db
        .store_verification_code(&payload.phone, &code)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    tracing::info!(
        "Código guardado para {} - Hora Venezuela: {}",
        payload.phone,
        now_vz.datetime_string_venezuela()
    );

    let phone_clone = payload.phone.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        match send_whatsapp_otp(&state_clone, &phone_clone, code).await {
            Ok(()) => {
                tracing::info!("OTP enviado por WhatsApp a {}", phone_clone);
            }
            Err(wa_err) => {
                tracing::warn!(
                    "WhatsApp OTP falló para {} ({:?}). Usando fallback SMS...",
                    phone_clone,
                    wa_err
                );
                if let Err(sms_err) = send_sms(&phone_clone, code).await {
                    tracing::error!(
                        "Fallback SMS también falló para {}: {:?}",
                        phone_clone,
                        sms_err
                    );
                }
            }
        }
    });

    tracing::info!("Verification code sent successfully to {}", payload.phone);
    Ok(Json(VerifyNumberResponse {
        ok: true,
        exists: true,
        phone: None,
        message: Some("verification_code_sent".to_string()),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth/login",
    tag = "Auth — Clientes",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Login exitoso, retorna tokens JWT", body = LoginResponse),
        (status = 401, description = "Teléfono inválido o código incorrecto/expirado"),
    )
)]
pub async fn login_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let now_vz = VenezuelaDateTime::now();
    tracing::info!(
        "[{}] login request for phone: {}",
        now_vz.datetime_string_venezuela(),
        payload.phone
    );

    let customer = AuthService::lookup_by_phone(&state.db, &payload.phone)
        .await
        .ok_or_else(|| {
            tracing::warn!("Login attempt for non-existent phone: {}", payload.phone);
            ApiError::Unauthorized("invalid_phone_number".to_string())
        })?;

    let verification =
        AuthService::lookup_verification_code(&state.db, &payload.phone, &payload.code)
            .await
            .ok_or_else(|| {
                tracing::warn!("Invalid verification code for phone: {}", payload.phone);
                ApiError::Unauthorized("invalid_verification_code".to_string())
            })?;

    if AuthService::is_code_expired(&verification) {
        let expires_vz = VenezuelaDateTime::from_utc(verification.expires_at);
        tracing::warn!(
            "Código expirado para {}: expiró el {} (hora Venezuela)",
            payload.phone,
            expires_vz.datetime_string_venezuela()
        );
        return Err(ApiError::Unauthorized("code_expired".to_string()));
    }

    let created_vz = VenezuelaDateTime::from_utc(verification.created_at);
    let expires_vz = VenezuelaDateTime::from_utc(verification.expires_at);
    tracing::debug!(
        "Código válido - Creado: {}, Expira: {}, Ahora: {}",
        created_vz.datetime_string_venezuela(),
        expires_vz.datetime_string_venezuela(),
        now_vz.datetime_string_venezuela()
    );

    if let Some(id) = &verification._id {
        let _ = AuthService::delete_verification_code(&state.db, id).await;
        tracing::debug!("Código de verificación borrado después de uso exitoso");
    }

    let jwt = JwtService::new(JwtCfg::from_env());
    let family = uuid::Uuid::new_v4().to_string();

    let (access_token, access_exp) =
        jwt.issue_encrypted_access(&customer.id, None, &["me:read", "payments:create"]);
    let (refresh_token, refresh_exp, jti) = jwt.issue_encrypted_refresh(&customer.id, &family);

    let ttl = ttl_from_exp(refresh_exp);
    state
        .redis
        .set_refresh_session(
            AuthAudience::Client.redis_realm(),
            &family,
            &customer.id,
            &jti,
            ttl,
        )
        .await
        .map_err(ApiError::from)?;

    tracing::info!(
        "Login successful for user: {} at {} (Venezuela)",
        customer.id,
        now_vz.datetime_string_venezuela()
    );

    let payload = LoginResponse {
        ok: true,
        exists: true,
        tokens: TokenPair {
            access_token,
            access_exp,
            refresh_token,
            refresh_exp,
        },
    };
    let tokens = payload.tokens.clone();

    Ok(response_with_client_auth_cookies(&state, payload, &tokens))
}

#[utoipa::path(
    post,
    path = "/v1/auth/refresh",
    tag = "Auth — Clientes",
    request_body = RefreshRequest,
    responses(
        (status = 200, description = "Tokens renovados", body = RefreshResponse),
        (status = 401, description = "Refresh inválido/expirado/reutilizado"),
    )
)]
pub async fn refresh_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    payload: Option<Json<RefreshRequest>>,
) -> Result<Response, ApiError> {
    let now_vz = VenezuelaDateTime::now();
    tracing::info!(
        "[{}] refresh token request",
        now_vz.datetime_string_venezuela()
    );

    let body_token = payload.as_ref().and_then(|v| v.refresh_token.as_deref());
    let raw_refresh =
        match read_refresh_token(&headers, &state.config, AuthAudience::Client, body_token) {
            Some(v) => v,
            None => {
                return Ok(refresh_error_response(
                    &state,
                    "invalid_refresh_token",
                    "No se encontró refresh token",
                ));
            }
        };

    let jwt = JwtService::new(JwtCfg::from_env());

    let refresh_claims = match jwt.verify_encrypted_refresh_verbose(&raw_refresh) {
        Ok(v) => v,
        Err(_) => {
            return Ok(refresh_error_response(
                &state,
                "invalid_refresh_token",
                "Refresh token inválido",
            ));
        }
    };

    let (access_token, access_exp) =
        jwt.issue_encrypted_access(&refresh_claims.sub, None, &["me:read", "payments:create"]);

    let (new_refresh_token, refresh_exp, new_jti) =
        jwt.issue_encrypted_refresh(&refresh_claims.sub, &refresh_claims.fam);

    let rotate_result = state
        .redis
        .rotate_refresh_session(
            AuthAudience::Client.redis_realm(),
            &refresh_claims.fam,
            &refresh_claims.sub,
            &refresh_claims.jti,
            &new_jti,
            ttl_from_exp(refresh_exp),
        )
        .await;

    let rotate_result = match rotate_result {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("Refresh rotation redis error: {}", err);
            return Err(ApiError::from(err));
        }
    };

    match rotate_result {
        RefreshSessionRotateOutcome::Rotated => {
            tracing::info!(
                "Tokens refreshed successfully for user: {} at {} (Venezuela)",
                refresh_claims.sub,
                now_vz.datetime_string_venezuela()
            );

            let response_body = RefreshResponse {
                ok: true,
                tokens: TokenPair {
                    access_token,
                    access_exp,
                    refresh_token: new_refresh_token,
                    refresh_exp,
                },
            };
            let tokens = response_body.tokens.clone();
            Ok(response_with_client_auth_cookies(
                &state,
                response_body,
                &tokens,
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

fn response_with_client_auth_cookies<T: serde::Serialize>(
    state: &Arc<AppState>,
    body: T,
    tokens: &TokenPair,
) -> Response {
    let json = Json(body);
    let mut response = json.into_response();
    append_client_auth_cookies(&state.config, response.headers_mut(), tokens);
    response
}

fn append_client_auth_cookies(
    cfg: &crate::config::Config,
    headers: &mut HeaderMap,
    pair: &TokenPair,
) {
    push_set_cookie(
        headers,
        build_auth_cookie(
            cfg,
            CLIENT_ACCESS_COOKIE,
            &pair.access_token,
            ttl_from_exp(pair.access_exp) as i64,
            "/",
        ),
    );
    push_set_cookie(
        headers,
        build_auth_cookie(
            cfg,
            CLIENT_REFRESH_COOKIE,
            &pair.refresh_token,
            ttl_from_exp(pair.refresh_exp) as i64,
            "/v1/auth",
        ),
    );
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
        build_clear_cookie(&state.config, CLIENT_ACCESS_COOKIE, "/"),
    );
    push_set_cookie(
        response.headers_mut(),
        build_clear_cookie(&state.config, CLIENT_REFRESH_COOKIE, "/v1/auth"),
    );
    response
}

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
    let now = JwtService::now();
    exp.saturating_sub(now).max(1) as u64
}
