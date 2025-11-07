use serde::{Deserialize, Serialize};

// ============================================
// VERIFY NUMBER
// ============================================

#[derive(Debug, Deserialize)]
pub struct VerifyNumberRequest {
    pub phone: String,
}

#[derive(Debug, Serialize)]
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

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub phone: String,
    pub code: u32,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub ok: bool,
    pub exists: bool,
    pub tokens: TokenPair,
}

// ============================================
// REFRESH
// ============================================

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Serialize)]
pub struct RefreshResponse {
    pub ok: bool,
    pub tokens: TokenPair,
}

// ============================================
// SHARED TYPES
// ============================================

#[derive(Debug, Serialize, Clone)]
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
