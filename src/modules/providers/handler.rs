use axum::{extract::State, Json};
use std::sync::Arc;

use crate::{
    db::UserRepository,
    error::ApiError,
    models::users::{ProviderResponse, UserResponse},
    state::AppState,
};

#[utoipa::path(
    get,
    path = "/v1/users/agents",
    tag = "Providers",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Usuarios con nRole ∈ [0, 3) — admin/staff/etc., excluye providers", body = Vec<UserResponse>),
        (status = 401, description = "No autorizado"),
    )
)]
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

#[utoipa::path(
    get,
    path = "/v1/users/providers",
    tag = "Providers",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Usuarios con nRole == 3 (providers)", body = Vec<ProviderResponse>),
        (status = 401, description = "No autorizado"),
    )
)]
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
