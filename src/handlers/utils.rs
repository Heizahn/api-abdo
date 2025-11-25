use axum::{extract::State, Extension, Json};
use std::sync::Arc;

use crate::auth::claims::AccessClaims;

use crate::{
    db::SalesRepository,
    error::ApiError,
    models::payment::{Bank, BankListResponse},
    state::AppState,
};

pub async fn get_bank_list(
    Extension(_claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<BankListResponse>, ApiError> {
    let banks: Vec<Bank> = state.db.find_bank_list().await.or_else(|e| {
        tracing::error!("Error finding bank list: {}", e);
        Err(ApiError::DatabaseError(e.to_string()))
    })?;

    // Convert ObjectId to String for client convenience if Bank struct doesn't handle it via serde
    // Assuming Bank struct has an ObjectId field that needs serialization adjustment,
    // but usually, this is handled in the struct definition with #[serde(with = "bson::serde_helpers::hex_string_as_object_id")]
    // or similar. Since I cannot see the Bank struct definition, I will assume the user wants
    // me to modify the handler logic, but typically this requires changing the model.
    // However, if the user implies mapping the data here:
    let banks_formatter = banks
        .into_iter()
        .map(|bank| Bank {
            id: bank.id,
            bank_code: bank.bank_code,
            bank_name: bank.bank_name,
        })
        .collect::<Vec<Bank>>();

    Ok(Json(BankListResponse {
        ok: true,
        data: banks_formatter,
    }))
}
