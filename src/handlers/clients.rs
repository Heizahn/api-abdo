use axum::{extract::State, Json};
use std::sync::Arc;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, UserRepository},
    error::ApiError,
    models::db::ClientListItem,
    state::AppState,
};
use axum::extract::Extension;

/// GET /v1/auth-user/clients/all
///
/// Retorna la lista de clientes con id, nombre y balance.
/// Si el usuario tiene rol provider (nRole == 3) solo ve sus clientes.
/// Otros roles ven todos los clientes (o pueden filtrar via query en el futuro).
pub async fn get_all_clients_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
) -> Result<Json<Vec<ClientListItem>>, ApiError> {
    let user = state
        .db
        .find_user_by_id(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    let owner_id = if (user.role - 3.0_f32).abs() < 0.01 {
        Some(claims.id.as_str())
    } else {
        None
    };

    state
        .db
        .get_all_clients(owner_id)
        .await
        .map(Json)
        .map_err(ApiError::DatabaseError)
}
