use axum::{extract::State, Json};
use mongodb::bson::oid::ObjectId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    db::{ProfileRepository, SalesRepository},
    error::ApiError,
    state::AppState,
};

// ============================================
// DTOs
// ============================================

#[derive(Deserialize)]
pub struct CalculationRequest {
    pub amount_usd: f64,
    pub id_tax: Option<String>,
    pub id_debt: Option<String>,
}

#[derive(Serialize)]
pub struct CalculationResponse {
    pub ok: bool,
    pub amount_bs: f64,
}

// ============================================
// HANDLER
// ============================================

/// POST /v1/calculation/bs
pub async fn calculate_bs_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CalculationRequest>,
) -> Result<Json<CalculationResponse>, ApiError> {
    // 1. Validar que venga al menos uno de los dos IDs
    if payload.id_tax.is_none() && payload.id_debt.is_none() {
        return Err(ApiError::BadRequest(
            "Se requiere id_tax o id_debt".to_string(),
        ));
    }

    // 2. Obtener la Tasa (Exchange Rate)
    // Primero intentamos de Redis, si falla o no está, vamos a Mongo
    let exchange_rate = match state.redis.get_exchange_rate().await {
        Ok(Some(rate)) => rate,
        _ => state.db.get_latest_exchange_rate().await.map_err(|e| {
            tracing::error!("Error obteniendo tasa de cambio: {:?}", e);
            ApiError::DatabaseError(e.to_string())
        })?,
    };

    // 3. Determinar el ID del Tax a usar (Opcional)
    let tax_oid = if let Some(tax_id_str) = payload.id_tax {
        Some(
            ObjectId::parse_str(&tax_id_str)
                .map_err(|_| ApiError::BadRequest("Invalid id_tax".to_string()))?,
        )
    } else if let Some(debt_id_str) = payload.id_debt {
        // Buscar la deuda
        let debt = state
            .db
            .find_debt_by_id(&debt_id_str)
            .await
            .map_err(|e| ApiError::DatabaseError(e))?
            .ok_or(ApiError::NotFound)?;

        // Buscar el cliente de la deuda
        let client = state
            .db
            .find_client_by_id(&debt.id_client.to_hex())
            .await
            .map_err(|e| ApiError::DatabaseError(e))?;

        // Obtener el id_tax del cliente (puede ser None)
        client.id_tax
    } else {
        None
    };

    // 4. Buscar el documento Tax (Si tax_oid es None, busca el DEFAULT)
    let tax = state
        .db
        .find_tax_by_id(tax_oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // 5. Calcular
    // Formula: dolarRecibo * tasa * iva

    let result_bs = payload.amount_usd * exchange_rate * tax.iva;

    // Redondear a 2 decimales
    let result_bs_rounded = (result_bs * 100.0).round() / 100.0;

    Ok(Json(CalculationResponse {
        ok: true,
        amount_bs: result_bs_rounded,
    }))
}
