use axum::{extract::State, Extension, Json};
use mongodb::bson;
use std::{collections::HashMap, sync::Arc};

use crate::{
    auth::claims::AccessClaims,
    auth::service::AuthService,
    db::{ProfileRepository, SalesRepository},
    error::ApiError,
    models::receivable::*,
    state::AppState,
};

/// GET /v1/receivable/me
/// Obtiene todas las deudas activas (receivables) del usuario autenticado
pub async fn me_receivables_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ReceivablesResponse>, ApiError> {
    tracing::info!("📋 GET /receivable/me for user: {}", claims.sub);

    // ... (Validaciones de scope) ...

    // 1. Obtener Customer y Clientes
    let customer = AuthService::lookup_by_id(&state.db, &claims.sub)
        .await
        .ok_or(ApiError::NotFound)?;

    let clients = state
        .db
        .find_clients_by_phone(&customer.phone)
        .await
        .map_err(ApiError::DatabaseError)?;

    let client_ids: Vec<_> = clients.iter().map(|c| c._id).collect();

    // 2. Crear Mapa de Impuestos (Client ID -> IVA)
    let mut client_tax_map: HashMap<bson::oid::ObjectId, f64> = HashMap::new();

    for client in &clients {
        // Lógica: Si tiene id_tax busca en BD, sino usa 1.08
        let tax_rate = if let Some(tax_id) = client.id_tax {
            match state.db.find_tax_by_id(Some(tax_id)).await {
                Ok(Some(tax)) => tax.iva, // Asumo que tu modelo Tax tiene campo 'iva'
                _ => 1.08,
            }
        } else {
            1.08
        };
        client_tax_map.insert(client._id, tax_rate);
    }

    // 3. Obtener Deudas Activas
    let debts = state
        .db
        .find_active_debts_by_client_ids(&client_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    if debts.is_empty() {
        return Ok(Json(ReceivablesResponse {
            ok: true,
            receivables: vec![],
        }));
    }

    let debt_ids: Vec<_> = debts.iter().map(|d| d._id).collect();

    // 4. Obtener PartPayments (Usando tu struct simple)
    let part_payments = state
        .db
        .find_part_payments_by_debt_ids(&debt_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    // Extraemos IDs de pagos para buscar sus detalles en la colección Payment
    let payment_ids: Vec<_> = part_payments.iter().map(|pp| pp.id_payment).collect();

    // 5. Obtener Payments (Solo para ver si están Activos)
    let processed_payments = state
        .db
        .find_payments_by_ids(&payment_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    // 6. Obtener Reportes PENDIENTES (Nueva función)
    let pending_reports = state
        .db
        .find_pending_reports_by_debt_ids(&debt_ids)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    // 7. Obtener Tasa de Cambio
    let exchange_rate = match state.redis.get_exchange_rate().await {
        Ok(Some(rate)) => rate,
        _ => {
            let rate = state
                .db
                .get_latest_exchange_rate()
                .await
                .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
            let _ = state
                .redis
                .set_exchange_rate(rate, state.config.redis_exchange_rate_ttl)
                .await;
            rate
        }
    };

    // 8. Procesar Datos
    let mut receivables = Vec::new();

    for debt in debts {
        let mut payment_list = Vec::new();
        let mut total_paid_usd = 0.0;
        let mut has_pending = false;

        // --- A. Procesar Pagos YA Activos (Restan Deuda) ---
        // Filtramos usando el campo id_debt de tu struct PartPayment
        let debt_parts: Vec<_> = part_payments
            .iter()
            .filter(|pp| pp.id_debt == debt._id)
            .collect();

        for pp in debt_parts {
            // Buscamos el Payment padre para ver el estado
            if let Some(payment) = processed_payments.iter().find(|p| p._id == pp.id_payment) {
                if payment.s_state == "Activo" {
                    // Usamos pp.n_amount de tu struct PartPayment
                    total_paid_usd += pp.n_amount;

                    payment_list.push(PaymentData {
                        payment_id: payment._id.to_string(),
                        amount_bs: payment.n_bs,
                        amount_usd: pp.n_amount,
                        status: payment.s_state.clone(),
                        reference: None,
                        is_report: false,
                    });
                }
            }
        }

        // --- B. Procesar Reportes Pendientes (Solo se listan) ---
        // Filtramos reportes donde id_debt coincide (manejando el Option)
        let debt_reports: Vec<_> = pending_reports
            .iter()
            .filter(|r| r.id_debt == Some(debt._id))
            .collect();

        for report in debt_reports {
            has_pending = true;
            // NO sumamos a total_paid_usd (el cliente ve que debe, pero ve el pago pendiente abajo)

            payment_list.push(PaymentData {
                payment_id: report.id.map(|id| id.to_string()).unwrap_or_default(),
                amount_bs: report.amount_bs,
                amount_usd: report.amount_usd,
                status: report.state.clone(), // "Pendiente"
                reference: Some(report.reference.clone()),
                is_report: true, // Es un reporte
            });
        }

        // --- C. Calcular Saldo Pendiente ---
        let pending_usd = debt.n_amount - total_paid_usd;

        // Solo procesamos si hay deuda o si hay pagos pendientes (para que el cliente los vea)
        if pending_usd > 0.001 || has_pending {
            // 1. Obtener Tax del cliente
            let client_tax_rate = client_tax_map.get(&debt.id_client).cloned().unwrap_or(1.08);

            // 2. Calcular BS: (Deuda USD * Tasa * Tax)
            let pending_bs = if pending_usd > 0.0 {
                pending_usd * exchange_rate * client_tax_rate
            } else {
                0.0
            };

            let pending_bs_rounded = (pending_bs * 100.0).round() / 100.0;

            receivables.push(ReceivableData {
                debt_id: debt._id.to_string(),
                id_owner: debt.id_client.to_string(),
                reason: debt.s_reason.clone(),
                state: debt.s_state.clone(),
                total_amount_usd: debt.n_amount,
                pending_amount_usd: pending_usd,
                created_at: debt.d_creation.to_string(),
                pending_amount_bs: pending_bs_rounded,
                has_pending_payments: has_pending,
                payments: payment_list,
            });
        }
    }

    Ok(Json(ReceivablesResponse {
        ok: true,
        receivables,
    }))
}

/// GET /v1/receivable/:id
/// Obtiene una deuda específica por ID, validando que pertenezca al usuario
pub async fn get_receivable_by_id_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    axum::extract::Path(debt_id): axum::extract::Path<String>,
) -> Result<Json<ReceivableByIdResponse>, ApiError> {
    tracing::info!("📋 GET /receivable/{} for user: {}", debt_id, claims.sub);

    // 1. Obtener Customer y Clientes (Validación de propiedad)
    let customer = AuthService::lookup_by_id(&state.db, &claims.sub)
        .await
        .ok_or(ApiError::NotFound)?;

    let clients = state
        .db
        .find_clients_by_phone(&customer.phone)
        .await
        .map_err(ApiError::DatabaseError)?;

    let client_ids: Vec<_> = clients.iter().map(|c| c._id).collect();

    // 2. Buscar la Deuda Específica
    let debt = state
        .db
        .find_debt_by_id(&debt_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 3. Validar que la deuda pertenezca a uno de los clientes del usuario
    if !client_ids.contains(&debt.id_client) {
        return Err(ApiError::Forbidden);
    }

    // 4. Crear Mapa de Impuestos (Solo para este cliente)
    // Aunque sea uno solo, mantenemos la lógica para reusar si se quiere
    let client = clients
        .iter()
        .find(|c| c._id == debt.id_client)
        .ok_or(ApiError::NotFound)?; // Should not happen given check above

    let tax_rate = if let Some(tax_id) = client.id_tax {
        match state.db.find_tax_by_id(Some(tax_id)).await {
            Ok(Some(tax)) => tax.iva,
            _ => 1.08,
        }
    } else {
        1.08
    };

    // 5. Obtener PartPayments de esta deuda
    let part_payments = state
        .db
        .find_part_payments_by_debt_ids(&[debt._id])
        .await
        .map_err(ApiError::DatabaseError)?;

    let payment_ids: Vec<_> = part_payments.iter().map(|pp| pp.id_payment).collect();

    // 6. Obtener Payments
    let processed_payments = state
        .db
        .find_payments_by_ids(&payment_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    // 7. Obtener Reportes PENDIENTES
    let pending_reports = state
        .db
        .find_pending_reports_by_debt_ids(&[debt._id])
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    // 8. Obtener Tasa de Cambio
    let exchange_rate = match state.redis.get_exchange_rate().await {
        Ok(Some(rate)) => rate,
        _ => {
            let rate = state
                .db
                .get_latest_exchange_rate()
                .await
                .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
            let _ = state
                .redis
                .set_exchange_rate(rate, state.config.redis_exchange_rate_ttl)
                .await;
            rate
        }
    };

    // 9. Procesar Datos (Lógica idéntica a me_receivables_handler pero para una sola deuda)
    let mut payment_list = Vec::new();
    let mut total_paid_usd = 0.0;
    let mut has_pending = false;

    // --- A. Procesar Pagos YA Activos ---
    let debt_parts: Vec<_> = part_payments
        .iter()
        .filter(|pp| pp.id_debt == debt._id)
        .collect();

    for pp in debt_parts {
        if let Some(payment) = processed_payments.iter().find(|p| p._id == pp.id_payment) {
            if payment.s_state == "Activo" {
                total_paid_usd += pp.n_amount;
                payment_list.push(PaymentData {
                    payment_id: payment._id.to_string(),
                    amount_bs: payment.n_bs,
                    amount_usd: pp.n_amount,
                    status: payment.s_state.clone(),
                    reference: None,
                    is_report: false,
                });
            }
        }
    }

    // --- B. Procesar Reportes Pendientes ---
    let debt_reports: Vec<_> = pending_reports
        .iter()
        .filter(|r| r.id_debt == Some(debt._id))
        .collect();

    for report in debt_reports {
        has_pending = true;
        payment_list.push(PaymentData {
            payment_id: report.id.map(|id| id.to_string()).unwrap_or_default(),
            amount_bs: report.amount_bs,
            amount_usd: report.amount_usd,
            status: report.state.clone(),
            reference: Some(report.reference.clone()),
            is_report: true,
        });
    }

    // --- C. Calcular Saldo Pendiente ---
    let pending_usd = debt.n_amount - total_paid_usd;

    // Calculamos BS
    let pending_bs = if pending_usd > 0.0 {
        pending_usd * exchange_rate * tax_rate
    } else {
        0.0
    };
    let pending_bs_rounded = (pending_bs * 100.0).round() / 100.0;

    let receivable_data = ReceivableData {
        debt_id: debt._id.to_string(),
        id_owner: debt.id_client.to_string(),
        reason: debt.s_reason.clone(),
        state: debt.s_state.clone(),
        total_amount_usd: debt.n_amount,
        pending_amount_usd: pending_usd,
        created_at: debt.d_creation.to_string(),
        pending_amount_bs: pending_bs_rounded,
        has_pending_payments: has_pending,
        payments: payment_list,
    };

    Ok(Json(ReceivableByIdResponse {
        ok: true,
        receivable: receivable_data,
    }))
}
