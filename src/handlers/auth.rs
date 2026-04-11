use axum::{
    extract::State,
    Json,
};
use std::sync::Arc;

use crate::{
    auth::service::AuthService,
    crypto::jwt::{JwtCfg, JwtService},
    db::AuthRepository,
    error::ApiError,
    models::auth::*,
    state::AppState,
    utils::{
        generate_verification_code,
        sms::send_sms,
        whatsapp::send_whatsapp_otp,
        timezone::VenezuelaDateTime,
    },
};

/// POST /v1/auth/verify_number
/// Verifica si un número existe y envía código de verificación por SMS
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

    // 1. Verificar si el usuario existe
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

    // 2. Generar código de verificación
    let code = generate_verification_code();


    // 3. Guardar código en MongoDB
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
    // 4. Enviar OTP de forma asíncrona: WhatsApp primero, SMS como fallback
    let phone_clone = payload.phone.clone();
    tokio::spawn(async move {
        match send_whatsapp_otp(&phone_clone, code).await {
            Ok(()) => {
                tracing::info!("OTP enviado por WhatsApp a {}", phone_clone);
            }
            Err(wa_err) => {
                tracing::warn!(
                    "WhatsApp OTP falló para {} ({:?}). Usando fallback SMS...",
                    phone_clone, wa_err
                );
                if let Err(sms_err) = send_sms(&phone_clone, code).await {
                    tracing::error!(
                        "Fallback SMS también falló para {}: {:?}",
                        phone_clone, sms_err
                    );
                }
            }
        }
    });

    // 5. Respuesta inmediata
    tracing::info!("Verification code sent successfully to {}", payload.phone);
    Ok(Json(VerifyNumberResponse {
        ok: true,
        exists: true,
        phone: None,
        message: Some("verification_code_sent".to_string()),
    }))
}

/// POST /v1/auth/login
/// con teléfono y código de verificación
pub async fn login_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    let now_vz = VenezuelaDateTime::now();
    tracing::info!(
        "[{}] login request for phone: {}",
        now_vz.datetime_string_venezuela(),
        payload.phone
    );

    // 1. Verificar que el usuario existe
    let customer = AuthService::lookup_by_phone(&state.db, &payload.phone)
        .await
        .ok_or_else(|| {
            tracing::warn!("Login attempt for non-existent phone: {}", payload.phone);
            ApiError::Unauthorized("invalid_phone_number".to_string())
        })?;

    // 2. Buscar código de verificación
    let verification = AuthService::lookup_verification_code(
        &state.db,
        &payload.phone,
        &payload.code,
    )
    .await
    .ok_or_else(|| {
        tracing::warn!("Invalid verification code for phone: {}", payload.phone);
        ApiError::Unauthorized("invalid_verification_code".to_string())
    })?;

    // 3. Verificar expiración del código
    if AuthService::is_code_expired(&verification){
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

    // 4. Borrar código usado (opcional pero recomendado)
    if let Some(id) = &verification._id {
        let _ = AuthService::delete_verification_code(&state.db, id).await;
        tracing::debug!("Código de verificación borrado después de uso exitoso");
    }

    // 5. Generar tokens JWT
    let jwt = JwtService::new(JwtCfg::from_env());

    let (access_token, access_exp) = jwt.issue_encrypted_access(
        &customer.id,
        None,
        &["me:read", "payments:create"],
    );

    let family = uuid::Uuid::new_v4().to_string();
    let (refresh_token, refresh_exp, _jti) = jwt.issue_encrypted_refresh(&customer.id, &family);

    tracing::info!(
        "Login successful for user: {} at {} (Venezuela)",
        customer.id,
        now_vz.datetime_string_venezuela()
    );

    Ok(Json(LoginResponse {
        ok: true,
        exists: true,
        tokens: TokenPair {
            access_token,
            access_exp,
            refresh_token,
            refresh_exp,
        },
    }))
}

/// POST /v1/auth/refresh
/// Renueva los tokens usando un refresh token válido
pub async fn refresh_handler(
    State(_state): State<Arc<AppState>>,
    Json(payload): Json<RefreshRequest>,
) -> Result<Json<RefreshResponse>, ApiError> {
    let now_vz = VenezuelaDateTime::now();
    tracing::info!(
        "[{}] refresh token request",
        now_vz.datetime_string_venezuela()
    );

    let jwt = JwtService::new(JwtCfg::from_env());

    // Verificar y descifrar refresh token
    let refresh_claims = jwt
        .verify_encrypted_refresh_verbose(&payload.refresh_token)
        .map_err(|e| {
            tracing::error!("Refresh token verification failed: {:?}", e);
            ApiError::Unauthorized("invalid_refresh_token".to_string())
        })?;

    // Emitir nuevos tokens
    let (access_token, access_exp) = jwt.issue_encrypted_access(
        &refresh_claims.sub,
        None,
        &["me:read", "payments:create"],
    );

    let (new_refresh_token, refresh_exp, _new_jti) =
        jwt.issue_encrypted_refresh(&refresh_claims.sub, &refresh_claims.fam);

    tracing::info!(
        "Tokens refreshed successfully for user: {} at {} (Venezuela)",
        refresh_claims.sub,
        now_vz.datetime_string_venezuela()
    );

    Ok(Json(RefreshResponse {
        ok: true,
        tokens: TokenPair {
            access_token,
            access_exp,
            refresh_token: new_refresh_token,
            refresh_exp,
        },
    }))
}
