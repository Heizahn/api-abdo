use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct User {
    #[serde(rename = "_id")]
    pub id: String, // LB4 uses UUID strings as _id usually, not ObjectId. Based on the code: `id: string` and `uuidv4()`.

    #[serde(rename = "sName")]
    pub name: String,

    #[serde(rename = "nRole")]
    pub role: f32,

    #[serde(rename = "email")]
    pub email: String,

    #[serde(rename = "visible", default)]
    pub visible: bool,

    #[serde(rename = "nTag", skip_serializing_if = "Option::is_none")]
    pub tag: Option<u32>,

    #[serde(rename = "idCreator", skip_serializing_if = "Option::is_none")]
    pub id_creator: Option<String>,

    #[serde(rename = "dCreation", skip_serializing_if = "Option::is_none")]
    pub d_creation: Option<String>, // LB4 uses ISO string
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserCredentials {
    //si no es UUID es ObjectId el _id de la credencial
    #[serde(rename = "userId")]
    pub user_id: String,
    pub password: String,
}

// DTOs

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: String,
    pub name: String,
    pub email: String,
    pub role: f32,
}

impl From<User> for UserResponse {
    fn from(user: User) -> Self {
        UserResponse {
            id: user.id,
            name: user.name,
            email: user.email,
            role: user.role,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UserLoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct UserLoginResponse {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshTokenRequest {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct RefreshTokenResponse {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub user: UserCreateData,
    pub password: String,
}

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
