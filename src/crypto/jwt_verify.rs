use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Verifica HS256 del JWT y devuelve el payload **en Base64URL** (segmento [1]).
/// NO toca ni intenta parsear el JSON interno.
pub fn verify_hs256_and_get_payload_b64(token: &str, secret: &[u8]) -> Option<String> {
    // header.payload.signature
    let mut parts = token.split('.');
    let (h, p, s) = (parts.next()?, parts.next()?, parts.next()?);

    // 1) Recalcular firma sobre "header.payload"
    let signing_input = format!("{}.{}", h, p);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
    mac.update(signing_input.as_bytes());

    // 2) Decodificar firma del token (Base64URL sin padding) y verificar
    let sig = general_purpose::URL_SAFE_NO_PAD.decode(s).ok()?;
    mac.verify_slice(&sig).ok()?; // falla -> None

    // 3) Si la firma es válida, devolvemos el **payload en b64url** (tal cual)
    Some(p.to_string())
}

/// Decodifica el payload (b64url) como **String JSON**.
/// Tu JWT lleva como payload un string: "...." (el blob cifrado).
pub fn decode_payload_as_string(b64_payload: &str) -> Option<String> {
    let bytes = general_purpose::URL_SAFE_NO_PAD.decode(b64_payload).ok()?;
    // Es un JSON que representa un string: ej:  "\"H8WW9J...\""
    serde_json::from_slice::<String>(&bytes).ok()
}
