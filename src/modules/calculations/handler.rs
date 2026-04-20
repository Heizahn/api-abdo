use axum::{extract::State, Json};
use mongodb::bson::oid::ObjectId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    db::{ProfileRepository, SalesRepository},
    error::ApiError,
    state::AppState,
};

#[derive(Deserialize)]
pub struct CalculationRequest {
    pub amount_usd: f64,
    pub id_tax: Option<String>,
    pub id_debt: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Serialize, Deserialize)]
pub enum Currency {
    USD,
    BS,
}

#[derive(Deserialize)]
pub struct CalculationRequestV2 {
    pub amount: f64,
    pub currency: Currency,
    pub id_tax: Option<String>,
    pub id_debt: Option<String>,
}

#[derive(Serialize)]
pub struct CalculationResponseV2 {
    pub ok: bool,
    pub amount: f64,
    pub currency: Currency,
}

#[derive(Serialize)]
pub struct CalculationResponse {
    pub ok: bool,
    pub amount_bs: f64,
}

/// POST /v1/calculation/bs
pub async fn calculate_bs_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CalculationRequest>,
) -> Result<Json<CalculationResponse>, ApiError> {
    if payload.id_tax.is_none() && payload.id_debt.is_none() {
        return Err(ApiError::BadRequest(
            "Se requiere id_tax o id_debt".to_string(),
        ));
    }

    let exchange_rate = match state.redis.get_exchange_rate().await {
        Ok(Some(rate)) => rate,
        _ => state.db.get_latest_exchange_rate().await.map_err(|e| {
            tracing::error!("Error obteniendo tasa de cambio: {:?}", e);
            ApiError::DatabaseError(e.to_string())
        })?,
    };

    let tax_oid = if let Some(tax_id_str) = payload.id_tax {
        Some(
            ObjectId::parse_str(&tax_id_str)
                .map_err(|_| ApiError::BadRequest("Invalid id_tax".to_string()))?,
        )
    } else if let Some(debt_id_str) = payload.id_debt {
        let debt = state
            .db
            .find_debt_by_id(&debt_id_str)
            .await
            .map_err(|e| ApiError::DatabaseError(e))?
            .ok_or(ApiError::NotFound)?;

        let client = state
            .db
            .find_client_by_id(&debt.id_client.to_hex())
            .await
            .map_err(|e| ApiError::DatabaseError(e))?;

        client.id_tax
    } else {
        None
    };

    let tax = state
        .db
        .find_tax_by_id(tax_oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let result_bs = payload.amount_usd * exchange_rate * tax.iva;
    let result_bs_rounded = (result_bs * 100.0).round() / 100.0;

    Ok(Json(CalculationResponse {
        ok: true,
        amount_bs: result_bs_rounded,
    }))
}

pub async fn calculate_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CalculationRequestV2>,
) -> Result<Json<CalculationResponseV2>, ApiError> {
    if payload.id_tax.is_none() && payload.id_debt.is_none() {
        return Err(ApiError::BadRequest(
            "Se requiere id_tax o id_debt".to_string(),
        ));
    }

    let exchange_rate = match state.redis.get_exchange_rate().await {
        Ok(Some(rate)) => rate,
        _ => state.db.get_latest_exchange_rate().await.map_err(|e| {
            tracing::error!("Error obteniendo tasa de cambio: {:?}", e);
            ApiError::DatabaseError(e.to_string())
        })?,
    };

    if exchange_rate == 0.0 {
        return Err(ApiError::Internal("La tasa de cambio es 0, no se puede calcular".to_string()));
    }

    let tax_oid = if let Some(tax_id_str) = payload.id_tax {
        Some(
            ObjectId::parse_str(&tax_id_str)
                .map_err(|_| ApiError::BadRequest("Invalid id_tax".to_string()))?,
        )
    } else if let Some(debt_id_str) = payload.id_debt {
        let debt = state
            .db
            .find_debt_by_id(&debt_id_str)
            .await
            .map_err(|e| ApiError::DatabaseError(e))?
            .ok_or(ApiError::NotFound)?;

        let client = state
            .db
            .find_client_by_id(&debt.id_client.to_hex())
            .await
            .map_err(|e| ApiError::DatabaseError(e))?;

        client.id_tax
    } else {
        None
    };

    let tax = state
        .db
        .find_tax_by_id(tax_oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let (calculated_amount, result_currency) = match payload.currency {
        Currency::USD => {
            let result = payload.amount * exchange_rate * tax.iva;
            (result, Currency::BS)
        },
        Currency::BS => {
            let result = payload.amount / (exchange_rate * tax.iva);
            (result, Currency::USD)
        },
    };

    let amount_rounded = (calculated_amount * 100.0).round() / 100.0;

    Ok(Json(CalculationResponseV2 {
        ok: true,
        amount: amount_rounded,
        currency: result_currency,
    }))
}
