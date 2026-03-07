use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, UserRepository},
    error::ApiError,
    models::db::{ClientDetail, ClientListItem},
    services::get_ip_pppoe_mk::get_ip_pppoe_mk,
    state::AppState,
};

#[derive(Deserialize)]
pub struct ClientsQuery {
    pub owner: Option<String>,
}

/// GET /v1/auth-user/clients/:id
///
/// Devuelve el detalle completo de un cliente.
/// - Rol 3 (provider): solo puede ver sus propios clientes.
/// - Otros roles: acceso libre, pueden filtrar por ?owner.
pub async fn get_client_by_id_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ClientDetail>, ApiError> {
    let user = state
        .db
        .find_user_by_id(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    let owner_id: Option<String> = if (user.role - 3.0_f32).abs() < 0.01 {
        Some(claims.id.clone())
    } else {
        None
    };

    let mut detail = state
        .db
        .get_client_by_id(&id, owner_id.as_deref())
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // Buscar IP PPPoE en MikroTik si el cliente tiene SN
    if let Some(sn) = detail.sn.clone() {
        let port_mk = state.config.port_mk.clone();
        let pass_mk = state.config.pass_mk.clone();
        let routers = vec!["10.255.255.5", "10.255.255.8"];

        let pppoe_ip = tokio::task::spawn_blocking(move || {
            for router_ip in routers {
                match get_ip_pppoe_mk(&sn, router_ip, &port_mk, "rust_api", &pass_mk) {
                    Ok(ip) => return Some(ip),
                    Err(_) => continue,
                }
            }
            None
        })
        .await
        .unwrap_or(None);

        detail.ip = pppoe_ip;
    } else {
        detail.ip = None;
    }

    Ok(Json(detail))
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