use axum::{extract::State, Json};
use std::sync::Arc;

use crate::{
    db::UserRepository,
    error::ApiError,
    models::users::{ProviderResponse, UserResponse},
    state::AppState,
};

/// GET /v1/users/agents
/// Retorna usuarios con nRole >= 0 y < 3 (admin, staff, etc. — excluye providers)
pub async fn get_agents_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<UserResponse>>, ApiError> {
    let users = state
        .db
        .find_agents()
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    Ok(Json(users.into_iter().map(UserResponse::from).collect()))
}

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
