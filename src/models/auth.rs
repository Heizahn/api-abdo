use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================
// VERIFY NUMBER
// ============================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct VerifyNumberRequest {
    /// Número de teléfono venezolano (ej: "04141234567")
    pub phone: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VerifyNumberResponse {
    pub ok: bool,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ============================================
// LOGIN
// ============================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    pub phone: String,
    /// Código de 6 dígitos recibido por WhatsApp/SMS
    pub code: u32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LoginResponse {
    pub ok: bool,
    pub exists: bool,
    pub tokens: TokenPair,
}

// ============================================
// REFRESH
// ============================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct RefreshRequest {
    /// Compat temporal: durante migración puede venir en body.
    /// El flujo recomendado usa cookie HttpOnly.
    pub refresh_token: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RefreshResponse {
    pub ok: bool,
    pub tokens: TokenPair,
}

// ============================================
// SHARED TYPES
// ============================================

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct TokenPair {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "accessExp")]
    pub access_exp: i64,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "refreshExp")]
    pub refresh_exp: i64,
}
