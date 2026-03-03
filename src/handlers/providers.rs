use axum::{extract::State, Json};
use std::sync::Arc;

use crate::{
    db::UserRepository,
    error::ApiError,
    models::users::ProviderResponse,
    state::AppState,
};

/// GET /v1/users/providers
pub async fn get_providers_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ProviderResponse>>, ApiError> {
    let users = state
        .db
        .find_providers()
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let providers: Vec<ProviderResponse> = users
        .into_iter()
        .map(ProviderResponse::from)
        .collect();

    Ok(Json(providers))
}
