use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use std::sync::Arc;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{UpdateUserPatch, UserListFilter, UserRepository},
    db::mongo::users::last_user_cursor,
    error::ApiError,
    models::users::{
        SetUserVisibleRequest, UpdateUserRequest, User, UserItem, UserListResponse,
        UserResponseEnvelope,
    },
    state::AppState,
};

const SUPERADMIN_ROLE: f32 = 0.0;

/// Valida que el `claims.id` (UUID del JWT) corresponda a un user existente y
/// con `nRole == 0.0` LEÍDO DE DB (no del snapshot del JWT). Si el rol fue
/// revocado, `claims.role` puede estar desactualizado hasta 6h. Este gate
/// evita que un ex-admin siga gestionando usuarios tras la revocación.
async fn require_superadmin(
    state: &Arc<AppState>,
    claims: &UserProfileClaims,
) -> Result<User, ApiError> {
    let user = state
        .db
        .find_user_by_id(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::Unauthorized("usuario no encontrado".into()))?;

    if user.role != SUPERADMIN_ROLE {
        return Err(ApiError::Forbidden);
    }

    Ok(user)
}

#[derive(serde::Deserialize)]
pub struct ListUsersQuery {
    pub search: Option<String>,
    pub role: Option<f32>,
    pub visible: Option<bool>,
    pub can_chat: Option<bool>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/users",
    tag = "Users — CRUD",
    security(("bearerAuth" = [])),
    params(
        ("search" = Option<String>, Query, description = "Substring case-insensitive en `sName` o `email`"),
        ("role" = Option<f32>, Query, description = "Filtro exacto por nRole"),
        ("visible" = Option<bool>, Query, description = "Filtro por visible (si se omite, devuelve ambos)"),
        ("can_chat" = Option<bool>, Query, description = "Filtro por bCanChat (si se omite, devuelve ambos)"),
        ("limit" = Option<i64>, Query, description = "Resultados por página (default 50, max 200)"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco (copiar de next_cursor)"),
    ),
    responses(
        (status = 200, description = "Listado paginado de usuarios", body = UserListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
    )
)]
pub async fn list_users_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<ListUsersQuery>,
) -> Result<Json<UserListResponse>, ApiError> {
    let _caller = require_superadmin(&state, &claims).await?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let search_ref = q.search.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let users = state
        .db
        .list_users(UserListFilter {
            search: search_ref,
            role: q.role,
            visible: q.visible,
            can_chat: q.can_chat,
            limit,
            cursor: q.cursor.as_deref(),
        })
        .await
        .map_err(ApiError::DatabaseError)?;

    let next_cursor = if (users.len() as i64) < limit {
        None
    } else {
        last_user_cursor(&users)
    };

    Ok(Json(UserListResponse {
        ok: true,
        data: users.into_iter().map(UserItem::from).collect(),
        next_cursor,
    }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/users/{id}/visible",
    tag = "Users — CRUD",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "UUID del usuario")),
    request_body = SetUserVisibleRequest,
    responses(
        (status = 200, description = "Estado actualizado", body = UserResponseEnvelope),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 404, description = "Usuario no encontrado"),
    )
)]
pub async fn set_user_visible_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<SetUserVisibleRequest>,
) -> Result<Json<UserResponseEnvelope>, ApiError> {
    let _caller = require_superadmin(&state, &claims).await?;

    let existed = state
        .db
        .set_user_visible(&id, payload.visible)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !existed {
        return Err(ApiError::NotFound);
    }

    // Releer para devolver el user actualizado (shape coherente con los demás
    // endpoints del CRUD que también devuelven el doc post-update).
    let user = state
        .db
        .find_user_by_id(&id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    Ok(Json(UserResponseEnvelope {
        ok: true,
        data: UserItem::from(user),
    }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/users/{id}",
    tag = "Users — CRUD",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "UUID del usuario")),
    request_body = UpdateUserRequest,
    responses(
        (status = 200, description = "Usuario actualizado", body = UserResponseEnvelope),
        (status = 400, description = "Payload inválido"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 404, description = "Usuario no encontrado"),
        (status = 409, description = "Email ya registrado por otro usuario"),
    )
)]
pub async fn update_user_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateUserRequest>,
) -> Result<Json<UserResponseEnvelope>, ApiError> {
    let _caller = require_superadmin(&state, &claims).await?;

    // Normalización mínima + validación de email si viene.
    let name = payload.name.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let email = payload.email.as_ref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());

    // Validar unicidad de email si cambió.
    if let Some(new_email) = email.as_ref() {
        if let Some(existing) = state
            .db
            .find_user_by_email(new_email)
            .await
            .map_err(ApiError::DatabaseError)?
        {
            if existing.id != id {
                return Err(ApiError::Conflict("email_ya_registrado".into()));
            }
        }
    }

    if let Some(r) = payload.role {
        if !r.is_finite() || r < 0.0 {
            return Err(ApiError::BadRequest("role inválido".into()));
        }
    }

    let existed = state
        .db
        .update_user(
            &id,
            UpdateUserPatch {
                name,
                email,
                role: payload.role,
                can_chat: payload.can_chat,
                tag: payload.tag,
            },
        )
        .await
        .map_err(ApiError::DatabaseError)?;
    if !existed {
        return Err(ApiError::NotFound);
    }

    let user = state
        .db
        .find_user_by_id(&id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    Ok(Json(UserResponseEnvelope {
        ok: true,
        data: UserItem::from(user),
    }))
}
