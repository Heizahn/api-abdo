use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::env;
use time::OffsetDateTime;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserProfileClaims {
    pub id: String,
    pub name: String,
    /// Tipo de token para distinguir access vs refresh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typ: Option<String>,
    /// nRole stored in the JWT to avoid an extra DB round-trip on every request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserRefreshClaims {
    pub id: String,
    pub typ: String, // "refresh"
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

pub struct UserJwtService {
    access_secret: String,
    refresh_secret: String,
    access_expiration_time: i64,  // seconds
    refresh_expiration_time: i64, // seconds
}

impl UserJwtService {
    pub fn new() -> Self {
        let access_secret = env::var("JWT_USER_SECRET").expect("JWT_USER_SECRET must be set");
        // Fallback temporal para no romper despliegues existentes.
        let refresh_secret =
            env::var("JWT_USER_REFRESH_SECRET").unwrap_or_else(|_| access_secret.clone());
        let access_expiration_time = env::var("JWT_USER_ACCESS_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(900); // 15 min
        let refresh_expiration_time = env::var("JWT_USER_REFRESH_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(30 * 24 * 3600); // 30 días
        Self {
            access_secret,
            refresh_secret,
            access_expiration_time,
            refresh_expiration_time,
        }
    }

    pub fn generate_access_token(
        &self,
        user_id: &str,
        name: &str,
        role: f32,
    ) -> Result<(String, i64), String> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let exp = now + self.access_expiration_time;

        let claims = UserProfileClaims {
            id: user_id.to_string(),
            name: name.to_string(),
            typ: Some("access".to_string()),
            role: Some(role),
            iat: Some(now),
            exp: Some(exp),
        };

        let header = Header::new(Algorithm::HS256);
        let key = EncodingKey::from_secret(self.access_secret.as_bytes());

        let token = encode(&header, &claims, &key).map_err(|e| e.to_string())?;
        Ok((token, exp))
    }

    pub fn generate_refresh_token(&self, user_id: &str) -> Result<(String, i64), String> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let exp = now + self.refresh_expiration_time;
        let claims = UserRefreshClaims {
            id: user_id.to_string(),
            typ: "refresh".to_string(),
            iat: now,
            exp,
            jti: uuid::Uuid::new_v4().to_string(),
        };

        let header = Header::new(Algorithm::HS256);
        let key = EncodingKey::from_secret(self.refresh_secret.as_bytes());
        let token = encode(&header, &claims, &key).map_err(|e| e.to_string())?;
        Ok((token, exp))
    }

    pub fn verify_token(&self, token: &str) -> Result<UserProfileClaims, String> {
        let key = DecodingKey::from_secret(self.access_secret.as_bytes());
        let validation = Validation::new(Algorithm::HS256);

        let decoded =
            decode::<UserProfileClaims>(token, &key, &validation).map_err(|e| e.to_string())?;

        // Si `typ` viene, debe ser access. Si no viene (tokens legacy), se acepta.
        if let Some(t) = decoded.claims.typ.as_deref() {
            if t != "access" {
                return Err("invalid_token_type".to_string());
            }
        }

        Ok(decoded.claims)
    }

    /// Verifica refresh token (firma + exp + typ).
    pub fn verify_refresh_token(&self, token: &str) -> Result<UserRefreshClaims, String> {
        let key = DecodingKey::from_secret(self.refresh_secret.as_bytes());
        let validation = Validation::new(Algorithm::HS256);

        let decoded =
            decode::<UserRefreshClaims>(token, &key, &validation).map_err(|e| e.to_string())?;
        if decoded.claims.typ != "refresh" {
            return Err("invalid_token_type".to_string());
        }

        Ok(decoded.claims)
    }
}
