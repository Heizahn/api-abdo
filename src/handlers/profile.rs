use axum::{extract::State, Extension, Json};
use std::sync::Arc;

use crate::{
    auth::{
        claims::{AccessClaims, Claims},
        service::AuthService,
    },
    db::Db,
    error::ApiError,
    models::profile::*,
    state::AppState,
};

/// GET /v1/profile/me
/// Obtiene información básica del usuario autenticado
pub async fn me_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<MeResponse>, ApiError> {
    tracing::info!("📋 GET /me for user: {}", claims.sub);
    tracing::debug!("📋 Claims scope: {:?}", claims.scope);

    // Verificar scope
    if !claims.scope.contains(&"me:read".to_string()) {
        tracing::warn!("⚠️ Insufficient scope for user: {}", claims.sub);
        return Err(ApiError::Forbidden);
    }

    tracing::debug!("✅ Scope válido: me:read");

    // Buscar customer por ID
    tracing::debug!("🔍 Buscando customer por ID: {}", claims.sub);
    let customer = AuthService::lookup_by_id(&state.db, &claims.sub)
        .await
        .ok_or_else(|| {
            tracing::error!("❌ Customer not found: {}", claims.sub);
            ApiError::NotFound
        })?;

    tracing::debug!("✅ Customer encontrado: phone={}", customer.phone);

    // Intentar obtener summary desde cache
    tracing::debug!("🔍 Buscando summary en cache...");
    let summary = match state.redis.get_user_summary(&claims.sub).await {
        Ok(Some(cached)) => {
            tracing::debug!("✅ Cache HIT para user summary");
            cached
        }
        Ok(None) => {
            tracing::debug!("⚠️ Cache MISS - Consultando MongoDB");
            // Obtener desde MongoDB
            let s = state
                .db
                .summary_by_phone(&customer.phone)
                .await
                .ok_or_else(|| {
                    tracing::error!("❌ Summary not found for phone: {}", customer.phone);
                    ApiError::NotFound
                })?;

            tracing::debug!("✅ Summary obtenido de MongoDB");

            // Guardar en cache
            let ttl = state.config.redis_user_data_ttl;
            match state.redis.set_user_summary(&claims.sub, &s, ttl).await {
                Ok(_) => tracing::debug!("✅ Summary guardado en cache"),
                Err(e) => tracing::warn!("⚠️ Error guardando en cache: {:?}", e),
            }

            s
        }
        Err(e) => {
            tracing::error!("❌ Error consultando cache: {:?}", e);
            // Continuar sin cache
            tracing::debug!("⚠️ Fallback a MongoDB sin cache");
            state
                .db
                .summary_by_phone(&customer.phone)
                .await
                .ok_or_else(|| {
                    tracing::error!("❌ Summary not found for phone: {}", customer.phone);
                    ApiError::NotFound
                })?
        }
    };

    tracing::info!("✅ Respuesta exitosa para user: {}", claims.sub);

    Ok(Json(MeResponse {
        ok: true,
        customer: CustomerData {
            name: summary.primary_name,
            phone: summary.phone,
        },
    }))
}

/// GET /v1/profile/me/balance
/// Obtiene el balance en VES del usuario autenticado
pub async fn me_balance_handler(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<BalanceResponse>, ApiError> {
    tracing::info!("GET /me/balance for user: {}", claims.sub);

    // Verificar scope
    if !claims.scope.contains(&"me:read".to_string()) {
        tracing::warn!("Insufficient scope for user: {}", claims.sub);
        return Err(ApiError::Forbidden);
    }

    // Intentar obtener balance desde cache
    let usd_balance = match state.redis.get_user_balance(&claims.sub).await {
        Ok(Some(cached)) => {
            tracing::debug!("Cache hit for user balance: {}", claims.sub);
            cached
        }
        _ => {
            tracing::debug!("Cache miss for user balance: {}", claims.sub);
            // Obtener desde MongoDB
            let balance = state
                .db
                .get_user_balance_usd(claims.sub.clone())
                .await
                .map_err(|e| {
                    tracing::error!("Error getting user balance: {:?}", e);
                    ApiError::DatabaseError(e.to_string())
                })?;

            // Guardar en cache
            let ttl = state.config.redis_balance_ttl;
            let _ = state
                .redis
                .set_user_balance(&claims.sub, balance, ttl)
                .await;

            balance
        }
    };

    // Obtener tasa de cambio (con cache)
    let exchange_rate = match state.redis.get_exchange_rate().await {
        Ok(Some(cached)) => {
            tracing::debug!("Cache hit for exchange rate");
            cached
        }
        _ => {
            tracing::debug!("Cache miss for exchange rate");
            // Obtener desde MongoDB
            let rate = state.db.get_latest_exchange_rate().await.map_err(|e| {
                tracing::error!("Error getting exchange rate: {:?}", e);
                ApiError::DatabaseError(e.to_string())
            })?;

            // Guardar en cache
            let ttl = state.config.redis_exchange_rate_ttl;
            let _ = state.redis.set_exchange_rate(rate, ttl).await;

            rate
        }
    };

    // Calcular balance en VES
    let ves_balance = usd_balance * exchange_rate * 1.08;
    let ves_balance_rounded = (ves_balance * 100.0).round() / 100.0;
    tracing::info!(
        "Balance calculated for user {}: {} VES",
        claims.sub,
        ves_balance
    );

    Ok(Json(BalanceResponse {
        ok: true,
        balance_ves: ves_balance_rounded,
    }))
}

/// GET /v1/profile/me/last_payments
/// Obtiene los últimos pagos del usuario agrupados por fecha
pub async fn me_last_payments_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<LastPaymentsResponse>, ApiError> {
    tracing::info!("GET /me/last_payments for user: {}", claims.sub);

    // Verificar scope
    if !claims.scope.contains(&"me:read".to_string()) {
        tracing::warn!("Insufficient scope for user: {}", claims.sub);
        return Err(ApiError::Forbidden);
    }

    // Obtener últimos pagos desde MongoDB
    let payments = state
        .db
        .get_last_payments_by_id(claims.sub.clone())
        .await
        .map_err(|e| {
            tracing::error!("Error getting last payments: {:?}", e);
            ApiError::DatabaseError(e.to_string())
        })?;

    tracing::info!(
        "Found {} payment groups for user {}",
        payments.len(),
        claims.sub
    );

    Ok(Json(LastPaymentsResponse {
        ok: true,
        data: payments,
    }))
}
