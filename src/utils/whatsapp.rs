use reqwest::Client;
use serde_json::json;
use anyhow::Result;

use crate::crypto::aes::decrypt_payload;
use crate::db::WhatsAppRepository;
use crate::models::whatsapp::WaSettings;
use crate::state::AppState;

const WA_API_VERSION: &str = "v25.0";

/// Convierte "0414..." → "58414..." y elimina cualquier símbolo no numérico.
fn normalize_phone(phone: &str) -> String {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    match digits.strip_prefix('0') {
        Some(rest) => format!("58{}", rest),
        None       => digits,
    }
}

/// Secreto AES para descifrar `WaSettings.access_token`. Mismo valor que usa
/// el módulo `whatsapp/handler.rs` — reutilizamos `JWT_SECRET` porque tiene
/// alta entropía y es estrictamente privado del backend.
fn settings_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

/// Intenta enviar el OTP por un `WaSettings` puntual. Se extrajo como helper
/// para permitir failover: `send_whatsapp_otp` recorre varios candidatos y
/// se detiene al primero que devuelva `Ok`.
async fn try_send_otp_via(settings: &WaSettings, phone: &str, code: u32) -> Result<()> {
    let otp_cfg = settings
        .purposes
        .otp
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("WaSettings sin otp.template_name — inconsistencia"))?;

    let access_token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| anyhow::anyhow!("No se pudo descifrar access_token de WaSettings"))?;

    let to = normalize_phone(phone);
    let otp = code.to_string();
    let url = format!(
        "https://graph.facebook.com/{}/{}/messages",
        WA_API_VERSION, settings.phone_number_id
    );

    let payload = json!({
        "messaging_product": "whatsapp",
        "to": to,
        "type": "template",
        "template": {
            "name":     otp_cfg.template_name,
            "language": { "code": otp_cfg.language },
            "components": [
                // BODY: reemplaza {{1}} en el texto del mensaje
                {
                    "type": "body",
                    "parameters": [{ "type": "text", "text": otp }]
                },
                // BUTTON url: reemplaza {{1}} al final de la URL del botón "Copiar código"
                {
                    "type":     "button",
                    "sub_type": "url",
                    "index":    "0",
                    "parameters": [{ "type": "text", "text": otp }]
                }
            ]
        }
    });

    let response = Client::new()
        .post(&url)
        .bearer_auth(&access_token)
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body   = response.text().await.unwrap_or_else(|_| "sin cuerpo".to_string());
        tracing::error!(
            "WhatsApp API error enviando a {} via {}. Status: {}. Body: {}",
            phone, settings.phone, status, body
        );
        return Err(anyhow::anyhow!("WhatsApp API error [{}]", status));
    }

    tracing::info!(
        "WhatsApp OTP enviado a {} via WaSettings {} (template: {})",
        phone,
        settings.phone,
        otp_cfg.template_name
    );
    Ok(())
}

/// Envía el código OTP por WhatsApp usando el template configurado en
/// `WaSettings.purposes.otp`. Si hay varios números activos con OTP
/// habilitado, hace **failover secuencial**: prueba el primero, y ante
/// cualquier error (rate-limit, 5xx, token vencido, decrypt fail) pasa al
/// siguiente hasta agotar candidatos.
///
/// El template debe tener:
///  - BODY   con parámetro `{{1}}` → se reemplaza por el código
///  - BUTTON sub_type `url` con parámetro `{{1}}` → sufijo dinámico del botón
///
/// Si no hay ningún `WaSettings` activo con `purposes.otp` configurado, o si
/// todos los candidatos fallan, devuelve `Err` para que el caller use el
/// fallback (SMS).
pub async fn send_whatsapp_otp(state: &AppState, phone: &str, code: u32) -> Result<()> {
    let candidates = state
        .db
        .find_wa_settings_for_purpose("otp")
        .await
        .map_err(|e| anyhow::anyhow!("DB error buscando WaSettings OTP: {}", e))?;

    if candidates.is_empty() {
        return Err(anyhow::anyhow!("No hay WaSettings activos con purposes.otp configurado"));
    }

    let total = candidates.len();
    let mut last_err: Option<anyhow::Error> = None;

    for (i, settings) in candidates.into_iter().enumerate() {
        match try_send_otp_via(&settings, phone, code).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    "OTP via WaSettings {} ({}/{}) falló: {:?} — probando siguiente",
                    settings.phone, i + 1, total, e
                );
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Todos los WaSettings OTP fallaron")))
}
