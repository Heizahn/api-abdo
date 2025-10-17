use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct AccessClaims {
    pub iss: String,
    pub sub: String,         // id del cliente
    pub aid: Option<String>, // id de cuenta/servicio activo (si aplica)
    pub scope: Vec<String>,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshClaims {
    pub iss: String,
    pub sub: String, // id del cliente
    pub iat: i64,
    pub exp: i64,
    pub jti: String, // id del refresh token
    pub fam: String, // family id para rotación
}
