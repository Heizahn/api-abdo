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
    pub amount: f64,        // Ya no se llama "amount_bs" porque puede ser USD
    pub currency: Currency, // Nos dice en qué moneda está el resultado
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

pub async fn calculate_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CalculationRequestV2>,
) -> Result<Json<CalculationResponseV2>, ApiError> { // Nota el cambio en el tipo de retorno
    
    // 1. Validar que venga al menos uno de los dos IDs
    if payload.id_tax.is_none() && payload.id_debt.is_none() {
        return Err(ApiError::BadRequest(
            "Se requiere id_tax o id_debt".to_string(),
        ));
    }

    // 2. Obtener la Tasa (Exchange Rate)
    let exchange_rate = match state.redis.get_exchange_rate().await {
        Ok(Some(rate)) => rate,
        _ => state.db.get_latest_exchange_rate().await.map_err(|e| {
            tracing::error!("Error obteniendo tasa de cambio: {:?}", e);
            ApiError::DatabaseError(e.to_string())
        })?,
    };

    // Validación de seguridad: evitar división por cero
    if exchange_rate == 0.0 {
        return Err(ApiError::Internal("La tasa de cambio es 0, no se puede calcular".to_string()));
    }

    // 3. Determinar el ID del Tax a usar (Lógica idéntica a la anterior)
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

    // 4. Buscar el documento Tax
    let tax = state
        .db
        .find_tax_by_id(tax_oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // 5. CALCULAR SEGÚN LA MONEDA DE ENTRADA
    // payload.currency es la moneda que RECIBIMOS. 
    // Calculamos hacia la moneda CONTRARIA.
    
    let (calculated_amount, result_currency) = match payload.currency {
        Currency::USD => {
            // Entrada: USD -> Salida: BS
            // Fórmula: Monto * Tasa * Impuesto
            let result = payload.amount * exchange_rate * tax.iva;
            (result, Currency::BS)
        },
        Currency::BS => {
            // Entrada: BS -> Salida: USD
            // Fórmula: (Monto / Tasa) * Impuesto
            // Nota: Aquí asumo que quieres aplicar el impuesto también al convertir a USD.
            let result = payload.amount / (exchange_rate * tax.iva);
            (result, Currency::USD)
        },
    };

    // Redondear a 2 decimales
    let amount_rounded = (calculated_amount * 100.0).round() / 100.0;

    Ok(Json(CalculationResponseV2 {
        ok: true,
        amount: amount_rounded,
        currency: result_currency,
    }))
}
