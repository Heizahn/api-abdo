use crate::db::OnuRepository;

use crate::models::onu::OnuResponse;
use crate::{error::ApiError, state::AppState};
use axum::{extract::State, Json};
use std::sync::Arc;

pub async fn get_all_onus(
    State(state): State<Arc<AppState>>,
) -> Result<Json<OnuResponse>, ApiError> {
    // El map_err es vital para que el error de DB se convierta en tu ApiError
    let onus = state
        .db
        .get_all_onus()
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    Ok(Json(OnuResponse {
        ok: true,
        data: onus,
    }))
}
