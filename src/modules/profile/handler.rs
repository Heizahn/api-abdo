use axum::{extract::State, Extension, Json};
use std::sync::Arc;

use crate::{
    auth::claims::AccessClaims,
    db::{ProfileRepository, SalesRepository},
    error::ApiError,
    models::profile::*,
    models::receivable::RejectedPayment,
    state::AppState,
};

/// GET /v1/profile/me/group
pub async fn me_group_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<MeGroupResponse>, ApiError> {
    tracing::info!("📋 GET /me/group for user: {}", claims.sub);

    if !claims.scope.contains(&"me:read".to_string()) {
        tracing::warn!("⚠️ Insufficient scope for user: {}", claims.sub);
        return Err(ApiError::Forbidden);
    }

    let exchange_rate = match state.redis.get_exchange_rate().await {
        Ok(Some(cached)) => cached,
        _ => state.db.get_latest_exchange_rate().await.map_err(|e| {
            tracing::error!("Error getting exchange rate: {:?}", e);
            ApiError::DatabaseError(e.to_string())
        })?,
    };

    let client_docs = state
        .db
        .get_clients_by_phone_group(claims.sub.clone())
        .await
        .map_err(|e| {
            tracing::error!("❌ Error fetching clients by phone group: {:?}", e);
            ApiError::DatabaseError(e.to_string())
        })?;

    if client_docs.is_empty() {
        tracing::warn!(
            "⚠️ No clients found for phone group of user: {}",
            claims.sub
        );
        return Err(ApiError::NotFound);
    }

    let mut client_summaries: Vec<ClientSummary> = Vec::new();

    for doc in client_docs {
        let client_id_oid = doc.get_object_id("_id").unwrap().clone();
        let client_id = client_id_oid.to_hex();
        let name = doc.get_str("sName").unwrap_or("N/A").to_string();
        let phone = doc.get_str("sPhone").unwrap_or("N/A").to_string();

        let usd_balance = doc
            .get_f64("nBalance")
            .or_else(|_| doc.get_i32("nBalance").map(|v| v as f64))
            .or_else(|_| doc.get_i64("nBalance").map(|v| v as f64))
            .unwrap_or(0.0);

        let linked_tax_id = doc.get_object_id("idTax").ok();

        let tax = state.db.find_tax_by_id(linked_tax_id).await.unwrap_or(None);
        let iva = tax.map(|t| t.iva).unwrap_or(1.0);

        let ves_balance = usd_balance * exchange_rate * iva;
        let ves_balance_rounded = (ves_balance * 100.0).round() / 100.0;

        let last_payments = state
            .db
            .get_last_payments_by_id_client(client_id.clone())
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    "❌ Error getting payments for client {}: {:?}",
                    client_id,
                    e
                );
                Vec::new()
            });

        let rejected_payments = state
            .db
            .find_rejected_reports_by_client_id(&client_id_oid)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    "❌ Error getting rejected payments for client {}: {:?}",
                    client_id,
                    e
                );
                Vec::new()
            })
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

        client_summaries.push(ClientSummary {
            client: ClientData {
                id: client_id,
                name,
                phone,
                id_tax: linked_tax_id.map(|oid| oid.to_hex()),
            },
            balance_ves: ves_balance_rounded,
            last_payments,
            rejected_payments,
        });
    }

    tracing::info!(
        "✅ Respuesta exitosa con {} clientes para user: {}",
        client_summaries.len(),
        claims.sub
    );

    Ok(Json(MeGroupResponse {
        ok: true,
        clients: client_summaries,
    }))
}

pub async fn me_phone_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<MePhoneResponse>, ApiError> {
    let phone = state.db.get_phone(&claims.sub).await.unwrap_or_default();
    Ok(Json(MePhoneResponse { ok: true, phone }))
}
