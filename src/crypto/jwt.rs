use crate::crypto::jwt_verify::{decode_payload_as_string, verify_hs256_and_get_payload_b64};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::crypto::aes::{decrypt_payload, encrypt_payload};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessClaims {
    pub iss: String,
    pub sub: String,
    pub aid: Option<String>,
    pub scope: Vec<String>,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshClaims {
    pub iss: String,
    pub sub: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
    pub fam: String,
}

pub struct JwtCfg {
    pub iss: String,
    pub secret: String, // MISMA cadena para HS256 y AES-GCM (está bien)
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
    pub(crate) cfg: JwtCfg,
    pub(crate) enc: EncodingKey,
}

#[derive(Debug)]
pub enum RefreshVerifyError {
    Signature,
    Decrypt,
    Json,
    IssMismatch,
    Expired,
    PayloadNotString,
}

#[derive(Debug)]
pub enum AccessVerifyError {
    Signature,
    Decrypt,
    Json,
    PayloadNotString,
}

impl JwtService {
    pub fn new(cfg: JwtCfg) -> Self {
        let enc = EncodingKey::from_secret(cfg.secret.as_bytes()); // HS256
        Self { cfg, enc }
    }

    #[inline]
    pub fn now() -> i64 {
        OffsetDateTime::now_utc().unix_timestamp()
    }
}

impl JwtService {
    // ------------ ISSUE ------------
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
            jti: Uuid::new_v4().to_string(),
        };
        let json = serde_json::to_string(&claims).expect("json claims");
        let encrypted = encrypt_payload(&self.cfg.secret, &json);

        let mut header = Header::default();
        header.alg = Algorithm::HS256;
        let token = encode(&header, &encrypted, &self.enc).expect("encode access");
        (token, exp)
    }

    pub fn issue_encrypted_refresh(&self, sub: &str, family: &str) -> (String, i64, String) {
        let now = Self::now();
        let exp = now + self.cfg.refresh_ttl;
        let jti = Uuid::new_v4().to_string();

        let claims = RefreshClaims {
            iss: self.cfg.iss.clone(),
            sub: sub.to_string(),
            iat: now,
            exp,
            jti: jti.clone(),
            fam: family.to_string(),
        };
        let json = serde_json::to_string(&claims).expect("json refresh");
        let encrypted = encrypt_payload(&self.cfg.secret, &json);

        let mut header = Header::default();
        header.alg = Algorithm::HS256;
        let token = encode(&header, &encrypted, &self.enc).expect("encode refresh");
        (token, exp, jti)
    }

    // ------------ DECODE ACCESS ------------
    /// HS256 (firma) -> extrae STRING (blob cifrado) -> AES-GCM -> JSON -> AccessClaims
    pub fn decode_encrypted_verbose(&self, token: &str) -> Result<AccessClaims, AccessVerifyError> {
        // 1) Verificar firma HS256 (manual) y obtener el payload *b64url*
        let payload_b64 = verify_hs256_and_get_payload_b64(token, self.cfg.secret.as_bytes())
            .ok_or(AccessVerifyError::Signature)?;

        // 2) Ese payload (JSON) es un **string** con tu blob cifrado (Base64URL)
        let encrypted_blob =
            decode_payload_as_string(&payload_b64).ok_or(AccessVerifyError::PayloadNotString)?;

        // 3) Descifrar AES-GCM (nonce||cipher, Base64URL sin padding)
        let decrypted =
            decrypt_payload(&self.cfg.secret, &encrypted_blob).ok_or(AccessVerifyError::Decrypt)?;

        // 4) Parsear JSON descifrado -> AccessClaims
        let claims: AccessClaims =
            serde_json::from_str(&decrypted).map_err(|_| AccessVerifyError::Json)?;

        Ok(claims)
    }

    /// Igual que arriba, pero permitiendo que luego valides exp si quieres
    pub fn decode_encrypted_allow_exp(&self, token: &str) -> Option<AccessClaims> {
        let payload_b64 = verify_hs256_and_get_payload_b64(token, self.cfg.secret.as_bytes())?;
        let encrypted_blob = decode_payload_as_string(&payload_b64)?;
        let decrypted = decrypt_payload(&self.cfg.secret, &encrypted_blob)?;
        serde_json::from_str::<AccessClaims>(&decrypted).ok()
    }

    /// Refresh: igual flujo + valida iss/exp tras descifrar
    pub fn verify_encrypted_refresh_verbose(
        &self,
        token: &str,
    ) -> Result<RefreshClaims, RefreshVerifyError> {
        let payload_b64 = verify_hs256_and_get_payload_b64(token, self.cfg.secret.as_bytes())
            .ok_or(RefreshVerifyError::Signature)?;

        let encrypted_blob =
            decode_payload_as_string(&payload_b64).ok_or(RefreshVerifyError::PayloadNotString)?;

        let decrypted = decrypt_payload(&self.cfg.secret, &encrypted_blob)
            .ok_or(RefreshVerifyError::Decrypt)?;

        let claims: RefreshClaims =
            serde_json::from_str(&decrypted).map_err(|_| RefreshVerifyError::Json)?;
        if claims.iss != self.cfg.iss {
            return Err(RefreshVerifyError::IssMismatch);
        }
        if claims.exp <= Self::now() {
            return Err(RefreshVerifyError::Expired);
        }

        Ok(claims)
    }
}
