use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, TokenData, Validation, decode, encode,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::claims::{AccessClaims, RefreshClaims};

use crate::crypto::aes::decrypt_payload;
use crate::crypto::aes::encrypt_payload;
use serde_json;

pub struct JwtCfg {
    pub iss: String,
    pub secret: String,
    pub access_ttl: i64,
    pub refresh_ttl: i64,
}

impl JwtCfg {
    pub fn from_env() -> Self {
        let iss = std::env::var("JWT_ISS").unwrap_or_else(|_| "abdo-api".into());
        let secret = std::env::var("JWT_SECRET").expect("Falta JWT_SECRET en .env");
        let access_ttl = std::env::var("ACCESS_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900);
        let refresh_ttl = std::env::var("REFRESH_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(45 * 24 * 3600);
        Self {
            iss,
            secret,
            access_ttl,
            refresh_ttl,
        }
    }
}

pub struct JwtService {
    cfg: JwtCfg,
    enc: EncodingKey,
    dec: DecodingKey,
}

impl JwtService {
    pub fn new(cfg: JwtCfg) -> Self {
        let enc = EncodingKey::from_secret(cfg.secret.as_bytes()); // HS256
        let dec = DecodingKey::from_secret(cfg.secret.as_bytes());
        Self { cfg, enc, dec }
    }

    pub fn now() -> i64 {
        OffsetDateTime::now_utc().unix_timestamp()
    }

    pub fn issue_access(&self, sub: &str, aid: Option<&str>, scope: &[&str]) -> (String, i64) {
        let now = Self::now();
        let exp = now + self.cfg.access_ttl;
        let claims = AccessClaims {
            iss: self.cfg.iss.clone(),
            sub: sub.to_string(),
            aid: aid.map(|s| s.to_string()),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            iat: now,
            exp,
            jti: Uuid::new_v4().to_string(),
        };
        let mut header = Header::default();
        header.alg = Algorithm::HS256;
        let token = encode(&header, &claims, &self.enc).expect("encode access");
        (token, exp)
    }

    pub fn issue_encrypted_access(
        &self,
        sub: &str,
        aid: Option<&str>,
        scope: &[&str],
    ) -> (String, i64) {
        let now = Self::now();
        let exp = now + self.cfg.access_ttl;

        let claims = AccessClaims {
            iss: self.cfg.iss.clone(),
            sub: sub.to_string(),
            aid: aid.map(|s| s.to_string()),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            iat: now,
            exp,
            jti: uuid::Uuid::new_v4().to_string(),
        };

        // 🔒 Convertir a JSON y encriptar
        let json = serde_json::to_string(&claims).expect("json claims");
        let encrypted = encrypt_payload(&self.cfg.secret, &json);

        // 🔏 Firmar el texto cifrado como si fuera el payload
        let mut header = Header::default();
        header.alg = Algorithm::HS256;
        let token = encode(&header, &encrypted, &self.enc).expect("encode jwt");

        (token, exp)
    }

    pub fn issue_refresh(&self, sub: &str, family: &str) -> (String, i64, String) {
        let now = Self::now();
        let exp = now + self.cfg.refresh_ttl;
        let jti = uuid::Uuid::new_v4().to_string();

        let claims = RefreshClaims {
            iss: self.cfg.iss.clone(),
            sub: sub.to_string(),
            iat: now,
            exp,
            jti: jti.clone(),
            fam: family.to_string(),
        };

        let mut header = Header::default();
        header.alg = Algorithm::HS256;

        let token = encode(&header, &claims, &self.enc).expect("encode refresh");
        (token, exp, jti)
    }

    pub fn verify_refresh(
        &self,
        token: &str,
    ) -> jsonwebtoken::errors::Result<TokenData<RefreshClaims>> {
        let mut val = Validation::new(Algorithm::HS256);
        val.set_issuer(&[&self.cfg.iss]);
        decode::<RefreshClaims>(token, &self.dec, &val)
    }

    pub fn decode_encrypted(&self, token: &str) -> Option<AccessClaims> {
        let data = decode::<String>(token, &self.dec, &Validation::new(Algorithm::HS256)).ok()?;
        let decrypted = decrypt_payload(&self.cfg.secret, &data.claims)?;
        serde_json::from_str::<AccessClaims>(&decrypted).ok()
    }
}
