use reqwest::Client;
use serde_json::json;
use std::env;
use anyhow::Result;

const WA_API_VERSION:  &str = "v25.0";
const WA_TEMPLATE:     &str = "code_verification";
const WA_TEMPLATE_LANG:&str = "es";

/// Convierte "0414..." → "58414..." y elimina cualquier símbolo no numérico.
fn normalize_phone(phone: &str) -> String {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    match digits.strip_prefix('0') {
        Some(rest) => format!("58{}", rest),
        None       => digits,
    }
}

/// Envía el código OTP por WhatsApp usando el template "code_verification" (aprobado).
///
/// El template tiene dos componentes con parámetro `{{1}}`:
///  - BODY   → texto del mensaje
///  - BUTTON → sub_type `url`, sufijo dinámico de la URL del botón nativo
///
/// Variables de entorno requeridas:
///  - `WHATSAPP_ACCESS_TOKEN`    — Bearer token de la Meta Cloud API
///  - `WHATSAPP_PHONE_NUMBER_ID` — ID del número de teléfono registrado en WhatsApp Business
pub async fn send_whatsapp_otp(phone: &str, code: u32) -> Result<()> {
    let access_token    = env::var("WHATSAPP_ACCESS_TOKEN")
        .map_err(|_| anyhow::anyhow!("Falta variable de entorno: WHATSAPP_ACCESS_TOKEN"))?;
    let phone_number_id = env::var("WHATSAPP_PHONE_NUMBER_ID")
        .map_err(|_| anyhow::anyhow!("Falta variable de entorno: WHATSAPP_PHONE_NUMBER_ID"))?;

    let to  = normalize_phone(phone);
    let otp = code.to_string();
    let url = format!(
        "https://graph.facebook.com/{}/{}/messages",
        WA_API_VERSION, phone_number_id
    );

    let payload = json!({
        "messaging_product": "whatsapp",
        "to": to,
        "type": "template",
        "template": {
            "name":     WA_TEMPLATE,
            "language": { "code": WA_TEMPLATE_LANG },
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
            "WhatsApp API error enviando a {}. Status: {}. Body: {}",
            phone, status, body
        );
        return Err(anyhow::anyhow!("WhatsApp API error [{}]", status));
    }

    tracing::info!("WhatsApp OTP enviado exitosamente a {}", phone);
    Ok(())
}
