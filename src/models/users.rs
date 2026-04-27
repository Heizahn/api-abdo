use mongodb::bson::Bson;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct User {
    #[serde(rename = "_id")]
    pub id: String, // Tu UUID puro intocable

    #[serde(rename = "sName")]
    pub name: String,

    #[serde(rename = "nRole")]
    pub role: f32,

    #[serde(rename = "email")]
    pub email: String,

    #[serde(rename = "visible", default)]
    pub visible: bool,

    /// Permiso para atender chats de WhatsApp. Cuando es `true`, el usuario aparece
    /// en el dropdown de transferencia de conversaciones.
    #[serde(rename = "bCanChat", default)]
    pub can_chat: bool,

    /// `true` cuando el usuario es un bot (Asistente Virtual del módulo
    /// AI Agent). Los pickers de agentes y filtros que devuelven humanos
    /// deben excluir estos registros — existen sólo para atribución
    /// (timeline de tickets/conversaciones, métricas).
    #[serde(rename = "bIsBot", default)]
    pub is_bot: bool,

    #[serde(rename = "nTag", skip_serializing_if = "Option::is_none")]
    pub tag: Option<u32>,

    #[serde(rename = "idCreator", skip_serializing_if = "Option::is_none")]
    pub id_creator: Option<String>,

    /// Rol previo al desactivar. Se llena cuando `visible` pasa de `true` a
    /// `false` (guardamos el `nRole` vigente antes de setearlo a `-1`).
    /// Se borra al reactivar. Interno — no se expone en `UserItem`.
    #[serde(rename = "nRolePrev", default, skip_serializing_if = "Option::is_none")]
    pub role_prev: Option<f32>,

    // Las fechas en Mongo suelen guardarse como mapas (ISODate), así que también usamos Bson aquí por si acaso
    #[serde(rename = "dCreation", skip_serializing_if = "Option::is_none")]
    pub d_creation: Option<Bson>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct UserCredentials {
    //si no es UUID es ObjectId el _id de la credencial
    #[serde(rename = "userId")]
    pub user_id: String,
    pub password: String,
}

// DTOs

#[derive(Debug, Serialize, ToSchema)]
pub struct UserResponse {
    pub id: String,
    pub name: String,
    pub email: String,
    pub role: f32,
    /// Permiso para atender chats de WhatsApp. El front lo usa para mostrar
    /// la sección de soporte sin depender del rol.
    pub can_chat: bool,
    /// `true` para usuarios bot (AI Agent). El front debería excluirlos de
    /// pickers humanos.
    pub is_bot: bool,
    /// Fecha de creación en ISO-8601 (RFC 3339). `null` si el documento no la tiene.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<String>,
}

fn bson_to_iso_string(b: &Bson) -> Option<String> {
    match b {
        Bson::DateTime(dt) => dt.try_to_rfc3339_string().ok(),
        Bson::String(s) => Some(s.clone()),
        _ => None,
    }
}

impl From<User> for UserResponse {
    fn from(user: User) -> Self {
        let creation_date = user.d_creation.as_ref().and_then(bson_to_iso_string);
        UserResponse {
            id: user.id,
            name: user.name,
            email: user.email,
            role: user.role,
            can_chat: user.can_chat,
            is_bot: user.is_bot,
            creation_date,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderResponse {
    pub id: String,
    pub tag: String,
    pub name: String,
}

impl From<User> for ProviderResponse {
    fn from(user: User) -> Self {
        let tag_value = user.tag.unwrap_or(0);


        ProviderResponse {
            id: user.id,
            tag: format!("ABDO77-{}", tag_value),
            name: user.name,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UserLoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserLoginResponse {
    pub token: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RefreshTokenRequest {
    pub token: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RefreshTokenResponse {
    pub token: String,
}

/// Shape devuelto por el CRUD de usuarios (`/v1/auth-user/users*`).
/// Nombres en inglés snake_case, como el resto del módulo nuevo.
#[derive(Debug, Serialize, ToSchema)]
pub struct UserItem {
    pub id: String,
    pub name: String,
    pub email: String,
    pub role: f32,
    pub visible: bool,
    pub can_chat: bool,
    /// `true` para usuarios bot (AI Agent). FE debe filtrar pickers humanos
    /// con `!is_bot`.
    pub is_bot: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<String>,
}

impl From<User> for UserItem {
    fn from(u: User) -> Self {
        let creation_date = u.d_creation.as_ref().and_then(bson_to_iso_string);
        UserItem {
            id: u.id,
            name: u.name,
            email: u.email,
            role: u.role,
            visible: u.visible,
            can_chat: u.can_chat,
            is_bot: u.is_bot,
            tag: u.tag,
            creator_id: u.id_creator,
            creation_date,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserListResponse {
    pub ok: bool,
    pub data: Vec<UserItem>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserResponseEnvelope {
    pub ok: bool,
    pub data: UserItem,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetUserVisibleRequest {
    pub visible: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateUserRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_chat: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<u32>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetUserPasswordRequest {
    pub password: String,
}

/// Payload para que el usuario autenticado cambie su propia contraseña.
/// Requiere la contraseña actual (`old_password`) como prueba de posesión,
/// independiente del JWT.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ChangeMyPasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateUserBody {
    pub name: String,
    pub email: String,
    pub role: f32,
    pub password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_chat: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<u32>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub user: UserCreateData,
    pub password: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct UserCreateData {
    #[serde(rename = "sName")]
    pub s_name: String,
    #[serde(rename = "sEmail")]
    pub s_email: String,
    #[serde(rename = "nRole")]
    pub n_role: f32,
    #[serde(rename = "nTag", skip_serializing_if = "Option::is_none")]
    pub n_tag: Option<u32>,
    #[serde(rename = "idCreator", skip_serializing_if = "Option::is_none")]
    pub id_creator: Option<String>,
    #[serde(rename = "dCreation", skip_serializing_if = "Option::is_none")]
    pub d_creation: Option<String>,
}
