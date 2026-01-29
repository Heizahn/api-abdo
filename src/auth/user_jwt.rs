use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::env;
use time::OffsetDateTime;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserProfileClaims {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
}

pub struct UserJwtService {
    secret: String,
    expiration_time: i64, // seconds
}

impl UserJwtService {
    pub fn new() -> Self {
        let secret = env::var("JWT_USER_SECRET").expect("JWT_USER_SECRET must be set");
        let expiration_time = 21600; // 6 hours (LoopBack 4 default usually)
        Self {
            secret,
            expiration_time,
        }
    }

    pub fn generate_token(&self, user_id: &str, name: &str) -> Result<String, String> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let exp = now + self.expiration_time;

        let claims = UserProfileClaims {
            id: user_id.to_string(),
            name: name.to_string(),
            iat: Some(now),
            exp: Some(exp),
        };

        let header = Header::new(Algorithm::HS256);
        let key = EncodingKey::from_secret(self.secret.as_bytes());

        encode(&header, &claims, &key).map_err(|e| e.to_string())
    }

    pub fn verify_token(&self, token: &str) -> Result<UserProfileClaims, String> {
        let key = DecodingKey::from_secret(self.secret.as_bytes());
        let validation = Validation::new(Algorithm::HS256);

        let decoded =
            decode::<UserProfileClaims>(token, &key, &validation).map_err(|e| e.to_string())?;

        Ok(decoded.claims)
    }
}
