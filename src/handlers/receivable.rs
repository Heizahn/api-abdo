use axum::{extract::State, Extension, Json};
use std::sync::Arc;

use crate::{
    auth::claims::AccessClaims, auth::service::AuthService, db::Db, error::ApiError,
    models::receivable::*, state::AppState,
};

/// GET /v1/receivable/me
/// Obtiene todas las deudas activas (receivables) del usuario autenticado
pub async fn me_receivables_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ReceivablesResponse>, ApiError> {
    tracing::info!("📋 GET /receivable/me for user: {}", claims.sub);

    // Verificar scope
    if !claims.scope.contains(&"me:read".to_string()) {
        tracing::warn!("⚠️ Insufficient scope for user: {}", claims.sub);
        return Err(ApiError::Forbidden);
    }

    tracing::debug!("✅ Scope válido: me:read");

    // 1. Obtener el cliente por ID para conseguir el sPhone
    tracing::debug!("🔍 Buscando customer por ID: {}", claims.sub);
    let customer = AuthService::lookup_by_id(&state.db, &claims.sub)
        .await
        .ok_or_else(|| {
            tracing::error!("❌ Customer not found: {}", claims.sub);
            ApiError::NotFound
        })?;

    tracing::debug!("✅ Customer encontrado: phone={}", customer.phone);

    // 2. Obtener todos los clientes con ese sPhone
    tracing::debug!(
        "🔍 Buscando todos los clientes con phone: {}",
        customer.phone
    );
    let clients = state
        .db
        .find_clients_by_phone(&customer.phone)
        .await
        .map_err(|e| {
            tracing::error!("❌ Error finding clients by phone: {:?}", e);
            ApiError::DatabaseError(e)
        })?;

    let client_ids: Vec<_> = clients.iter().map(|c| c._id).collect();
    tracing::debug!("✅ Encontrados {} clientes", client_ids.len());

    // 3. Obtener todas las deudas activas de esos clientes
    tracing::debug!(
        "🔍 Buscando deudas activas para {} clientes",
        client_ids.len()
    );
    let debts = state
        .db
        .find_active_debts_by_client_ids(&client_ids)
        .await
        .map_err(|e| {
            tracing::error!("❌ Error finding debts: {:?}", e);
            ApiError::DatabaseError(e)
        })?;

    tracing::debug!("✅ Encontradas {} deudas activas", debts.len());

    if debts.is_empty() {
        tracing::info!("✅ No receivables found for user: {}", claims.sub);
        return Ok(Json(ReceivablesResponse {
            ok: true,
            receivables: vec![],
        }));
    }

    let debt_ids: Vec<_> = debts.iter().map(|d| d._id).collect();

    // 4. Obtener todos los PartPayments de esas deudas
    tracing::debug!("🔍 Buscando PartPayments para {} deudas", debt_ids.len());
    let part_payments = state
        .db
        .find_part_payments_by_debt_ids(&debt_ids)
        .await
        .map_err(|e| {
            tracing::error!("❌ Error finding part payments: {:?}", e);
            ApiError::DatabaseError(e)
        })?;

    tracing::debug!("✅ Encontrados {} PartPayments", part_payments.len());

    // 5. Obtener los Payments asociados (solo Activos y Pendientes)
    let payment_ids: Vec<_> = part_payments.iter().map(|pp| pp.id_payment).collect();

    tracing::debug!("🔍 Buscando {} Payments", payment_ids.len());
    let payments = state
        .db
        .find_payments_by_ids(&payment_ids)
        .await
        .map_err(|e| {
            tracing::error!("❌ Error finding payments: {:?}", e);
            ApiError::DatabaseError(e)
        })?;

    tracing::debug!("✅ Encontrados {} Payments", payments.len());

    // 6. Obtener tasa de cambio BCV (con cache)
    tracing::debug!("🔍 Obteniendo tasa de cambio BCV");
    let exchange_rate = match state.redis.get_exchange_rate().await {
        Ok(Some(cached)) => {
            tracing::debug!("✅ Cache hit for exchange rate");
            cached
        }
        _ => {
            tracing::debug!("⚠️ Cache miss for exchange rate");
            let rate = state.db.get_latest_exchange_rate().await.map_err(|e| {
                tracing::error!("❌ Error getting exchange rate: {:?}", e);
                ApiError::DatabaseError(e.to_string())
            })?;

            let ttl = state.config.redis_exchange_rate_ttl;
            let _ = state.redis.set_exchange_rate(rate, ttl).await;

            rate
        }
    };

    tracing::debug!("✅ Exchange rate: {}", exchange_rate);

    // 7. Procesar cada deuda y calcular montos pendientes
    let mut receivables = Vec::new();

    for debt in debts {
        tracing::debug!("📊 Procesando deuda: {:?}", debt._id);

        // Filtrar PartPayments de esta deuda
        let debt_part_payments: Vec<_> = part_payments
            .iter()
            .filter(|pp| pp.id_debt == debt._id)
            .collect();

        // Construir lista de pagos con su estado

        let mut payment_list = Vec::new();
        let mut total_paid_usd = 0.0;
        let mut has_pending = false;

        for pp in debt_part_payments {
            // Buscar el Payment asociado
            if let Some(payment) = payments.iter().find(|p| p._id == pp.id_payment) {
                let status = payment.s_state.clone();

                // Solo considerar pagos Activos y Pendientes (ignorar Anulados)
                if status == "Activo" || status == "Pendiente" {
                    // Acumular el monto pagado en USD
                    total_paid_usd += pp.n_amount;

                    // Marcar si hay pagos pendientes
                    if status == "Pendiente" {
                        has_pending = true;
                    }

                    // Agregar a la lista de pagos
                    payment_list.push(PaymentData {
                        payment_id: payment._id.to_string(),
                        amount_bs: payment.n_bs, // Ya tiene IVA incluido
                        status: status.clone(),
                    });
                }
            }
        }

        // Calcular monto pendiente
        if debt._id.to_string() == "6863e184f73a401990403f93" {
            tracing::info!("🔍 DEBUG debt._id: {}", debt._id);
            tracing::info!("🔍 DEBUG debt.n_amount: {}", debt.n_amount);
            tracing::info!(
                "🔍 DEBUG debt.n_amount type: {}",
                std::any::type_name_of_val(&debt.n_amount)
            );
            tracing::info!("🔍 DEBUG total_paid_usd: {}", total_paid_usd);
            tracing::info!("🔍 DEBUG exchange_rate: {}", exchange_rate);
        }
        let pending_usd = debt.n_amount - total_paid_usd;
        let pending_bs = if pending_usd > 0.0 {
            pending_usd * exchange_rate * 1.08 // Aplicar IVA
        } else {
            0.0
        };

        // Redondear a 2 decimales
        let pending_bs_rounded = (pending_bs * 100.0).round() / 100.0;

        tracing::debug!(
            "✅ Deuda procesada - Original: {} USD, Pagado: {} USD, Pendiente: {} USD ({} BS)",
            debt.n_amount,
            total_paid_usd,
            pending_usd,
            pending_bs_rounded
        );

        if pending_bs_rounded > 0.0 {
            receivables.push(ReceivableData {
                debt_id: debt._id.to_string(),
                id_owner: debt.id_client.to_string(),
                reason: debt.s_reason.clone(),
                state: debt.s_state.clone(),
                created_at: debt.d_creation.timestamp_millis().to_string(),
                pending_amount_bs: pending_bs_rounded,
                has_pending_payments: has_pending,
                payments: payment_list,
            });
        } else {
            tracing::debug!("⏭️ Deuda completamente pagada, se omite de la respuesta");
        }
    }

    tracing::info!(
        "✅ Respuesta exitosa: {} receivables para user: {}",
        receivables.len(),
        claims.sub
    );

    Ok(Json(ReceivablesResponse {
        ok: true,
        receivables,
    }))
}
