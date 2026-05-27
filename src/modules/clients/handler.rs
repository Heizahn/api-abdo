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
    models::db::{ClientDetail, ClientListItem, ClientStatusHistoryItem, CustomerInfoItem},
    modules::network::mikrotik::ip_pppoe::get_ip_pppoe_mk,
    state::AppState,
};

#[derive(Deserialize)]
pub struct ClientsQuery {
    pub owner: Option<String>,
}

async fn resolve_owner_scope(
    state: &Arc<AppState>,
    claims: &UserProfileClaims,
    owner_param: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let caller = state
        .db
        .find_user_by_id(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    let caller_is_provider = (caller.role - 3.0_f32).abs() < 0.01;
    if caller_is_provider {
        if let Some(requested_owner) = owner_param {
            if requested_owner != claims.id {
                return Err(ApiError::Forbidden);
            }
        }
        return Ok(Some(claims.id.clone()));
    }

    let Some(requested_owner) = owner_param else {
        return Ok(None);
    };
    let owner_user = state
        .db
        .find_user_by_id(requested_owner)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::Forbidden)?;

    if (owner_user.role - 3.0_f32).abs() >= 0.01 {
        return Err(ApiError::Forbidden);
    }

    Ok(Some(requested_owner.to_string()))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/clients/{id}",
    tag = "Clients — Staff",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId del cliente")),
    responses(
        (status = 200, description = "Detalle completo del cliente. Incluye datos de ONU, sector, plan, proveedor, e IP PPPoE si tiene SN.", body = ClientDetail),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "Cliente no encontrado o no pertenece al provider"),
    )
)]
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

        detail.ip_pppoe = pppoe_ip;
    }

    Ok(Json(detail))
}

#[utoipa::path(
    get,
    path = "/v1/clients/{id}/status-history",
    tag = "Clients — Staff",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId del cliente")),
    responses(
        (status = 200, description = "Historial de cambios de estado del cliente", body = Vec<ClientStatusHistoryItem>),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_status_history_handler(
    State(state): State<Arc<AppState>>,
    Extension(_claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ClientStatusHistoryItem>>, ApiError> {
    state
        .db
        .get_client_status_history(&id)
        .await
        .map(Json)
        .map_err(ApiError::DatabaseError)
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/clients/contact-info",
    tag = "Clients — Staff",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Info de contacto (razón social, DNI, dirección, email, teléfono). Si el caller es provider, sólo sus clientes.", body = Vec<CustomerInfoItem>),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_customers_info_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
) -> Result<Json<Vec<CustomerInfoItem>>, ApiError> {
    // Role is embedded in the JWT claims (field added at login) — no extra DB round-trip needed.
    let role = claims.role.unwrap_or(0.0);
    let owner_id: Option<String> = if (role - 3.0_f32).abs() < 0.01 {
        Some(claims.id.clone())
    } else {
        None
    };

    state
        .db
        .get_customers_info(owner_id.as_deref())
        .await
        .map(Json)
        .map_err(ApiError::DatabaseError)
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/clients/all",
    tag = "Clients — Staff",
    security(("bearerAuth" = [])),
    params(("owner" = Option<String>, Query, description = "Filtrar por owner permitido para el caller. Si no tiene permiso, responde 403")),
    responses(
        (status = 200, description = "Listado de clientes (vista ligera para tablas)", body = Vec<ClientListItem>),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Owner no permitido para este usuario"),
    )
)]
pub async fn get_all_clients_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(params): Query<ClientsQuery>,
) -> Result<Json<Vec<ClientListItem>>, ApiError> {
    let owner_id = resolve_owner_scope(&state, &claims, params.owner.as_deref()).await?;

    state
        .db
        .get_all_clients(owner_id.as_deref())
        .await
        .map(Json)
        .map_err(ApiError::DatabaseError)
}
