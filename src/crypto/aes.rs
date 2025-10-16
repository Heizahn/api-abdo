use aes_gcm::aead::{Aead, AeadCore, OsRng};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce}; // AES-GCM 256 bits
use base64::{Engine as _, engine::general_purpose};
use sha2::{Digest, Sha256};

fn key32_from(secret: &str) -> [u8; 32] {
    // Deriva SIEMPRE 32 bytes desde el secreto (cualquier longitud)
    let hash = Sha256::digest(secret.as_bytes());
    let mut k = [0u8; 32];
    k.copy_from_slice(&hash);
    k
}

pub fn encrypt_payload(secret: &str, plaintext: &str) -> String {
    // ← ¡clave de 32 bytes garantizada!
    let k = key32_from(secret);
    let cipher = Aes256Gcm::new_from_slice(&k).expect("key");

    let nonce = Aes256Gcm::generate_nonce(&mut OsRng); // 12 bytes aleatorios
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .expect("encryption failed");

    // nonce || ciphertext  → Base64URL (sin padding)
    let mut data = nonce.to_vec();
    data.extend_from_slice(&ciphertext);
    general_purpose::URL_SAFE_NO_PAD.encode(data)
}

pub fn decrypt_payload(secret: &str, b64_cipher: &str) -> Option<String> {
    let bytes = general_purpose::URL_SAFE_NO_PAD.decode(b64_cipher).ok()?;
    if bytes.len() < 13 {
        return None;
    } // 12 nonce + al menos 1 de cipher

    let (nonce_bytes, ciphertext) = bytes.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    let k = key32_from(secret);
    let cipher = Aes256Gcm::new_from_slice(&k).ok()?;

    let plaintext = cipher.decrypt(nonce, ciphertext).ok()?;
    String::from_utf8(plaintext).ok()
}
