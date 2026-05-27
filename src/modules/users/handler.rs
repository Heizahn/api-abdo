use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use std::sync::Arc;

use crate::{
    db::mongo::users::last_user_cursor,
    db::{UpdateUserPatch, UserListFilter, UserRepository},
    error::ApiError,
    models::users::{
        ChangeMyPasswordRequest, CreateUserBody, OkResponse, SetUserPasswordRequest,
        SetUserVisibleRequest, UpdateUserRequest, User, UserCredentials, UserItem,
        UserListResponse, UserResponseEnvelope,
    },
    state::AppState,
};

const SUPERADMIN_ROLE: f32 = 0.0;
/// Cost de bcrypt para hashear passwords. `10` mantiene compatibilidad con los
/// users creados originalmente en LoopBack 4 (sus hashes son `$2a$10$...`),
/// así todos los logins tienen la misma latencia. Cost 10 ≈ 60-100ms por
/// verify — suficientemente lento para anti-brute-force, suficientemente
/// rápido para UX. NO usar `bcrypt::DEFAULT_COST` (que es 12) — cada
/// incremento duplica el tiempo.
const BCRYPT_COST: u32 = 10;
const PASSWORD_MIN_LEN: usize = 8;

/// Chequea que el caller tenga rol SUPERADMIN. El middleware
/// `user_jwt_auth_middleware` ya inyectó el `User` fresco leído de DB en la
/// request, así que acá sólo validamos el rol — sin query extra. Un user
/// "sin acceso" (role == -1) ya fue rechazado por el middleware con 401.
fn require_superadmin(current_user: &User) -> Result<(), ApiError> {
    if current_user.role != SUPERADMIN_ROLE {
        return Err(ApiError::Forbidden);
    }
    Ok(())
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
    Extension(current_user): Extension<User>,
    Query(q): Query<ListUsersQuery>,
) -> Result<Json<UserListResponse>, ApiError> {
    require_superadmin(&current_user)?;

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
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(payload): Json<SetUserVisibleRequest>,
) -> Result<Json<UserResponseEnvelope>, ApiError> {
    require_superadmin(&current_user)?;

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
    post,
    path = "/v1/auth-user/users",
    tag = "Users — CRUD",
    security(("bearerAuth" = [])),
    request_body = CreateUserBody,
    responses(
        (status = 200, description = "Usuario creado", body = UserResponseEnvelope),
        (status = 400, description = "Payload inválido (email/password/role)"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 409, description = "Email ya registrado"),
    )
)]
pub async fn create_user_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(payload): Json<CreateUserBody>,
) -> Result<Json<UserResponseEnvelope>, ApiError> {
    require_superadmin(&current_user)?;
    let caller = &current_user;

    // Normalización + validación.
    let name = payload.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::BadRequest("name requerido".into()));
    }
    let email = payload.email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::BadRequest("email inválido".into()));
    }
    if payload.password.len() < PASSWORD_MIN_LEN {
        return Err(ApiError::BadRequest(format!(
            "password debe tener al menos {} caracteres",
            PASSWORD_MIN_LEN
        )));
    }
    if !payload.role.is_finite() || payload.role < 0.0 {
        return Err(ApiError::BadRequest("role inválido".into()));
    }

    // Email único.
    if state
        .db
        .find_user_by_email(&email)
        .await
        .map_err(ApiError::DatabaseError)?
        .is_some()
    {
        return Err(ApiError::Conflict("email_ya_registrado".into()));
    }

    // Hash de password (costoso: ~100ms) antes de tocar DB.
    let hash = bcrypt::hash(&payload.password, BCRYPT_COST)
        .map_err(|_| ApiError::Internal("no se pudo hashear password".into()))?;

    let user_id = uuid::Uuid::new_v4().to_string();
    let now = mongodb::bson::DateTime::now();

    let user = User {
        id: user_id.clone(),
        name,
        role: payload.role,
        email: email.clone(),
        visible: payload.visible.unwrap_or(true),
        can_chat: payload.can_chat.unwrap_or(false),
        is_bot: false,
        tag: payload.tag,
        id_creator: Some(caller.id.clone()),
        role_prev: None,
        d_creation: Some(mongodb::bson::Bson::DateTime(now)),
    };

    state
        .db
        .create_user(user.clone())
        .await
        .map_err(ApiError::DatabaseError)?;

    // Credenciales en colección separada (UserCredentials).
    state
        .db
        .create_user_credentials(UserCredentials {
            user_id: user_id.clone(),
            password: hash,
        })
        .await
        .map_err(ApiError::DatabaseError)?;

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
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateUserRequest>,
) -> Result<Json<UserResponseEnvelope>, ApiError> {
    require_superadmin(&current_user)?;

    // Normalización mínima + validación de email si viene.
    let name = payload
        .name
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let email = payload
        .email
        .as_ref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());

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
        if !r.is_finite() {
            return Err(ApiError::BadRequest("role inválido".into()));
        }
        // `-1` es el sentinel de "sin acceso" y se gestiona sólo vía el toggle
        // `/visible` (atómicamente con `nRolePrev`). Bloqueamos el canal
        // directo para evitar estados inconsistentes (role=-1 con visible=true).
        if r == -1.0 {
            return Err(ApiError::BadRequest(
                "para desactivar usar PATCH /:id/visible { visible: false }".into(),
            ));
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

#[utoipa::path(
    patch,
    path = "/v1/auth-user/users/{id}/password",
    tag = "Users — CRUD",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "UUID del usuario")),
    request_body = SetUserPasswordRequest,
    responses(
        (status = 200, description = "Password actualizado", body = OkResponse),
        (status = 400, description = "Password inválido (mínimo 8 caracteres)"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 404, description = "Usuario no encontrado"),
    )
)]
pub async fn set_user_password_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(payload): Json<SetUserPasswordRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    require_superadmin(&current_user)?;

    if payload.password.len() < PASSWORD_MIN_LEN {
        return Err(ApiError::BadRequest(format!(
            "password debe tener al menos {} caracteres",
            PASSWORD_MIN_LEN
        )));
    }

    let hash = bcrypt::hash(&payload.password, BCRYPT_COST)
        .map_err(|_| ApiError::Internal("no se pudo hashear password".into()))?;

    let existed = state
        .db
        .update_user_password(&id, &hash)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !existed {
        return Err(ApiError::NotFound);
    }

    Ok(Json(OkResponse { ok: true }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/me/password",
    tag = "Users — CRUD",
    security(("bearerAuth" = [])),
    request_body = ChangeMyPasswordRequest,
    responses(
        (status = 200, description = "Password actualizado. El JWT actual sigue siendo válido — no se requiere relogin.", body = OkResponse),
        (status = 400, description = "`weak_password` (menor al mínimo de 8) | `same_password` (new == old) | `bad_request` (body mal formado)"),
        (status = 401, description = "`unauthorized` (JWT inválido/expirado)"),
        (status = 403, description = "`wrong_password` (old_password no coincide)"),
    )
)]
pub async fn change_my_password_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(payload): Json<ChangeMyPasswordRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    if payload.new_password.len() < PASSWORD_MIN_LEN {
        return Err(ApiError::WeakPassword);
    }
    if payload.new_password == payload.old_password {
        return Err(ApiError::SamePassword);
    }

    let creds: UserCredentials = state
        .db
        .find_user_credentials_by_user_id(&current_user.id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::Unauthorized(
            "Usuario no tiene credenciales".into(),
        ))?;

    let valid = bcrypt::verify(&payload.old_password, &creds.password)
        .map_err(|_| ApiError::InternalServerError)?;
    if !valid {
        return Err(ApiError::WrongPassword);
    }

    let hash = bcrypt::hash(&payload.new_password, BCRYPT_COST)
        .map_err(|_| ApiError::Internal("no se pudo hashear password".into()))?;

    state
        .db
        .update_user_password(&current_user.id, &hash)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(OkResponse { ok: true }))
}
