use crate::db::OnuRepository;

use crate::models::onu::{OnuCreate, OnuResponse};
use crate::{error::ApiError, state::AppState};
use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
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

#[derive(Serialize)]
pub struct OkResponse {
    ok: bool,
}

pub async fn create_onu(
    State(state): State<Arc<AppState>>,
    Json(onu): Json<OnuCreate>,
) -> Result<(StatusCode, Json<OkResponse>), ApiError> {
    match state.db.create_onu(onu).await {
        // TODO: Implementar respuesta correcta status code 201
        Ok(_) => {
            let response = Json(OkResponse { ok: true });
            Ok((StatusCode::CREATED, response))
        }
        Err(e) => Err(ApiError::DatabaseError(e)),
    }
}
