use axum::{extract::State, Extension, Json};
use mongodb::bson;
use std::{collections::HashMap, sync::Arc};

use mongodb::bson::oid::ObjectId;

use crate::{
    auth::claims::AccessClaims,
    auth::service::AuthService,
    db::{ProfileRepository, SalesRepository},
    error::ApiError,
    models::receivable::*,
    state::AppState,
};

#[utoipa::path(
    get,
    path = "/v1/receivable/me",
    tag = "Receivables — Clientes",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Deudas activas del cliente autenticado (con saldo pendiente o pagos en proceso)", body = ReceivablesResponse),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "Usuario no encontrado"),
    )
)]
pub async fn me_receivables_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ReceivablesResponse>, ApiError> {
    tracing::info!("📋 GET /receivable/me for user: {}", claims.sub);

    let customer = AuthService::lookup_by_id(&state.db, &claims.sub)
        .await
        .ok_or(ApiError::NotFound)?;

    let clients = state
        .db
        .find_clients_by_phone(&customer.phone)
        .await
        .map_err(ApiError::DatabaseError)?;

    let client_ids: Vec<_> = clients.iter().map(|c| c._id).collect();

    let mut client_tax_map: HashMap<bson::oid::ObjectId, f64> = HashMap::new();
    for client in &clients {
        let tax_rate = if let Some(tax_id) = client.id_tax {
            match state.db.find_tax_by_id(Some(tax_id)).await {
                Ok(Some(tax)) => tax.iva,
                _ => 1.08,
            }
        } else {
            1.08
        };
        client_tax_map.insert(client._id, tax_rate);
    }

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

    let part_payments = state
        .db
        .find_part_payments_by_debt_ids(&debt_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    let payment_ids: Vec<_> = part_payments.iter().map(|pp| pp.id_payment).collect();

    let processed_payments = state
        .db
        .find_payments_by_ids(&payment_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    let pending_reports = state
        .db
        .find_pending_reports_by_debt_ids(&debt_ids)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

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

    let mut receivables = Vec::new();
    let epsilon = 0.001;

    for debt in debts {
        let mut payment_list = Vec::new();
        let mut total_paid_usd = 0.0;
        let mut has_pending = false;

        let debt_parts: Vec<_> = part_payments
            .iter()
            .filter(|pp| pp.id_debt == debt._id)
            .collect();

        for pp in debt_parts {
            if let Some(payment) = processed_payments.iter().find(|p| p._id == pp.id_payment) {
                if payment.s_state.eq_ignore_ascii_case("Activo") {
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

        let debt_amount_rounded = (debt.n_amount * 100.0).round() / 100.0;
        let total_paid_rounded = (total_paid_usd * 100.0).round() / 100.0;

        let pending_usd_raw = debt_amount_rounded - total_paid_rounded;

        let pending_usd = if pending_usd_raw < epsilon {
            0.0
        } else {
            pending_usd_raw
        };

        if pending_usd > epsilon || has_pending {
            let client_tax_rate = client_tax_map.get(&debt.id_client).cloned().unwrap_or(1.08);

            let pending_bs = if pending_usd > 0.0 {
                pending_usd * exchange_rate * client_tax_rate
            } else {
                0.0
            };
            let pending_bs_rounded = (pending_bs * 100.0).round() / 100.0;

            let created_at_str = debt.d_creation.try_to_rfc3339_string().unwrap();

            receivables.push(ReceivableData {
                debt_id: debt._id.to_string(),
                id_owner: debt.id_client.to_string(),
                reason: debt.s_reason.clone(),
                state: debt.s_state.clone(),
                total_amount_usd: debt.n_amount,
                pending_amount_usd: pending_usd,
                created_at: created_at_str,
                pending_amount_bs: pending_bs_rounded,
                has_pending_payments: has_pending,
                payments: Some(payment_list),
            });
        }
    }

    Ok(Json(ReceivablesResponse {
        ok: true,
        receivables,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/receivable/me/paid",
    tag = "Receivables — Clientes",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Deudas ya pagadas (saldo 0) del cliente autenticado", body = ReceivablesResponse),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "Usuario no encontrado"),
    )
)]
pub async fn me_paid_receivables_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ReceivablesResponse>, ApiError> {
    tracing::info!("📋 GET /receivable/me/paid for user: {}", claims.sub);

    let customer = AuthService::lookup_by_id(&state.db, &claims.sub)
        .await
        .ok_or(ApiError::NotFound)?;

    let clients = state
        .db
        .find_clients_by_phone(&customer.phone)
        .await
        .map_err(ApiError::DatabaseError)?;

    let client_ids: Vec<_> = clients.iter().map(|c| c._id).collect();

    let mut client_tax_map: HashMap<bson::oid::ObjectId, f64> = HashMap::new();

    for client in &clients {
        let tax_rate = if let Some(tax_id) = client.id_tax {
            match state.db.find_tax_by_id(Some(tax_id)).await {
                Ok(Some(tax)) => tax.iva,
                _ => 1.08,
            }
        } else {
            1.08
        };
        client_tax_map.insert(client._id, tax_rate);
    }

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

    let part_payments = state
        .db
        .find_part_payments_by_debt_ids(&debt_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    let payment_ids: Vec<_> = part_payments.iter().map(|pp| pp.id_payment).collect();

    let processed_payments = state
        .db
        .find_payments_by_ids(&payment_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    let pending_reports = state
        .db
        .find_pending_reports_by_debt_ids(&debt_ids)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

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

    let mut receivables = Vec::new();

    for debt in debts {
        let mut total_paid_usd = 0.0;
        let mut has_pending = false;

        let debt_parts: Vec<_> = part_payments
            .iter()
            .filter(|pp| pp.id_debt == debt._id)
            .collect();

        for pp in debt_parts {
            if let Some(payment) = processed_payments.iter().find(|p| p._id == pp.id_payment) {
                if payment.s_state == "Activo" {
                    total_paid_usd += pp.n_amount;
                }
            }
        }

        let debt_reports: Vec<_> = pending_reports
            .iter()
            .filter(|r| r.id_debt == Some(debt._id))
            .collect();

        for _ in debt_reports {
            has_pending = true;
        }

        let pending_usd = debt.n_amount - total_paid_usd;

        if pending_usd <= 0.001 {
            let client_tax_rate = client_tax_map.get(&debt.id_client).cloned().unwrap_or(1.08);

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
                created_at: debt.d_creation.try_to_rfc3339_string().unwrap(),
                pending_amount_bs: pending_bs_rounded,
                has_pending_payments: has_pending,
                payments: None,
            });
        }
    }

    let mut receivables = receivables;
    receivables.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(Json(ReceivablesResponse {
        ok: true,
        receivables,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/receivable/{id}",
    tag = "Receivables — Clientes",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId de la deuda")),
    responses(
        (status = 200, description = "Detalle de la deuda (incluye pagos y reportes)", body = ReceivableByIdResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "La deuda no pertenece al cliente autenticado"),
        (status = 404, description = "Deuda no encontrada"),
    )
)]
pub async fn get_receivable_by_id_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    axum::extract::Path(debt_id): axum::extract::Path<String>,
) -> Result<Json<ReceivableByIdResponse>, ApiError> {
    tracing::info!("📋 GET /receivable/{} for user: {}", debt_id, claims.sub);

    let customer = AuthService::lookup_by_id(&state.db, &claims.sub)
        .await
        .ok_or(ApiError::NotFound)?;

    let clients = state
        .db
        .find_clients_by_phone(&customer.phone)
        .await
        .map_err(ApiError::DatabaseError)?;

    let client_ids: Vec<_> = clients.iter().map(|c| c._id).collect();

    let debt = state
        .db
        .find_debt_by_id(&debt_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    if !client_ids.contains(&debt.id_client) {
        return Err(ApiError::Forbidden);
    }

    let client = clients
        .iter()
        .find(|c| c._id == debt.id_client)
        .ok_or(ApiError::NotFound)?;

    let tax_rate = if let Some(tax_id) = client.id_tax {
        match state.db.find_tax_by_id(Some(tax_id)).await {
            Ok(Some(tax)) => tax.iva,
            _ => 1.08,
        }
    } else {
        1.08
    };

    let part_payments = state
        .db
        .find_part_payments_by_debt_ids(&[debt._id])
        .await
        .map_err(ApiError::DatabaseError)?;

    let payment_ids: Vec<_> = part_payments.iter().map(|pp| pp.id_payment).collect();

    let processed_payments = state
        .db
        .find_payments_by_ids(&payment_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    let pending_reports = state
        .db
        .find_pending_reports_by_debt_ids(&[debt._id])
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

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

    let mut payment_list = Vec::new();
    let mut total_paid_usd = 0.0;
    let mut has_pending = false;

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

    let pending_usd = debt.n_amount - total_paid_usd;

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
        payments: Some(payment_list),
    };

    Ok(Json(ReceivableByIdResponse {
        ok: true,
        receivable: receivable_data,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/receivable/{id}/payments/rejected",
    tag = "Receivables — Clientes",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId de la deuda")),
    responses(
        (status = 200, description = "Pagos rechazados para esta deuda", body = RejectedPaymentsResponse),
        (status = 400, description = "ID inválido"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "La deuda no pertenece al cliente autenticado"),
        (status = 404, description = "Deuda no encontrada"),
    )
)]
pub async fn get_rejected_payments_by_receivable_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    axum::extract::Path(debt_id): axum::extract::Path<String>,
) -> Result<Json<RejectedPaymentsResponse>, ApiError> {
    tracing::info!(
        "📋 GET /receivable/{}/payments/rejected for user: {}",
        debt_id,
        claims.sub
    );

    let customer = AuthService::lookup_by_id(&state.db, &claims.sub)
        .await
        .ok_or(ApiError::NotFound)?;

    let clients = state
        .db
        .find_clients_by_phone(&customer.phone)
        .await
        .map_err(ApiError::DatabaseError)?;

    let client_ids: Vec<_> = clients.iter().map(|c| c._id).collect();

    let debt = state
        .db
        .find_debt_by_id(&debt_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    if !client_ids.contains(&debt.id_client) {
        return Err(ApiError::Forbidden);
    }

    let debt_oid =
        ObjectId::parse_str(&debt_id).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let rejected = state
        .db
        .find_rejected_reports_by_debt_id(&debt_oid)
        .await
        .map_err(ApiError::DatabaseError)?;

    let payments = rejected
        .into_iter()
        .map(|r| RejectedPayment {
            payment_id: r.id.map(|id| id.to_string()).unwrap_or_default(),
            amount_usd: r.amount_usd,
            amount_bs: r.amount_bs,
            reference: r.reference,
            rejected_at: r.created_at.to_rfc3339(),
            rejection_reason: r.rejection_reason.unwrap_or_default(),
        })
        .collect();

    Ok(Json(RejectedPaymentsResponse { ok: true, payments }))
}
