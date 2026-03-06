use axum::{
    extract::{Extension, Query, State},
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, UserRepository},
    error::ApiError,
    models::db::ClientListItem,
    state::AppState,
};

#[derive(Deserialize)]
pub struct ClientsQuery {
    pub owner: Option<String>,
}

/// GET /v1/auth-user/clients/all?owner=<id>
///
/// - Rol 3 (provider): siempre filtra por su propio ID, ignora ?owner.
/// - Otros roles: usa ?owner si se provee, o devuelve todos.
pub async fn get_all_clients_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(params): Query<ClientsQuery>,
) -> Result<Json<Vec<ClientListItem>>, ApiError> {
    let user = state
        .db
        .find_user_by_id(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    let owner_id: Option<String> = if (user.role - 3.0_f32).abs() < 0.01 {
        Some(claims.id.clone())
    } else {
        params.owner
    };

    state
        .db
        .get_all_clients(owner_id.as_deref())
        .await
        .map(Json)
        .map_err(ApiError::DatabaseError)
}