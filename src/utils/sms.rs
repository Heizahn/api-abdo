use anyhow::Result;
use reqwest;
use std::env;

/// Envía un SMS con el código de verificación
pub async fn send_sms(phone: &str, code: u32) -> Result<()> {
    // Obtener configuración de environment
    let api_host = env::var("API_HOST_SMS")?;
    let api_key = env::var("API_KEY_SMS")?;
    let short_number = env::var("API_SHORT_NUMBER")?;

    // Determinar prefijo según operadora
    let complete_short = if phone.starts_with("0416") || phone.starts_with("0426") {
        format!("121{}", short_number)
    } else {
        short_number
    };

    // Formatear número: "0414..." → "58414..."
    let to_phone = if let Some(stripped) = phone.strip_prefix('0') {
        format!("58{}", stripped)
    } else {
        phone.to_string()
    };

    // Construir mensaje
    let sms_content = format!(
        "Inversiones ABDO77 te envia el codigo {}. Valido solo por 1 hora para confirmar tu cuenta. No responder.",
        code
    );

    // Cuerpo de la petición
    let sms_body = serde_json::json!({
        "to": to_phone,
        "from": complete_short,
        "content": sms_content,
        "dlr": "no",
        "coding": "3"
    });

    // Enviar SMS
    let client = reqwest::Client::new();
    let response = client
        .post(&api_host)
        .header("Authorization", format!("Basic {}", api_key))
        .json(&sms_body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_body = response
            .text()
            .await
            .unwrap_or_else(|_| "sin cuerpo".to_string());
        tracing::error!(
            "Error enviando SMS. Status: {}. Body: {}",
            status,
            error_body
        );
        return Err(anyhow::anyhow!("SMS provider returned error: {}", status));
    }

    tracing::info!("SMS enviado exitosamente a {}", phone);
    Ok(())
}
