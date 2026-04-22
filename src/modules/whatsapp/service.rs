use anyhow::Result;
use serde_json::json;
use std::error::Error as StdError;

const WA_API_VERSION: &str = "v25.0";

/// Formatea un `reqwest::Error` con toda la cadena de causas (is_timeout,
/// is_connect, source()...) para que el log muestre la razón real en vez
/// del texto genérico "error sending request for url".
fn describe_reqwest_error(ctx: &str, e: reqwest::Error) -> anyhow::Error {
    let mut flags = Vec::new();
    if e.is_timeout() { flags.push("timeout"); }
    if e.is_connect() { flags.push("connect"); }
    if e.is_request() { flags.push("request"); }
    if e.is_body() { flags.push("body"); }
    if e.is_decode() { flags.push("decode"); }

    let mut chain = format!("{}", e);
    let mut src: Option<&dyn StdError> = e.source();
    while let Some(s) = src {
        chain.push_str(" | caused by: ");
        chain.push_str(&s.to_string());
        src = s.source();
    }

    let flag_str = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(",")) };
    anyhow::anyhow!("{}{}: {}", ctx, flag_str, chain)
}

pub struct WhatsAppService {
    access_token: String,
    phone_number_id: String,
    client: reqwest::Client,
}

impl WhatsAppService {
    /// Construye el service con credenciales explícitas (provienen de `WaSettings`,
    /// cifrado descifrado in-memory).
    pub fn new(client: reqwest::Client, phone_number_id: String, access_token: String) -> Self {
        Self { access_token, phone_number_id, client }
    }

    fn messages_url(&self) -> String {
        format!(
            "https://graph.facebook.com/{}/{}/messages",
            WA_API_VERSION, self.phone_number_id
        )
    }

    /// Envía un mensaje de texto libre a un número (formato E.164 sin "+").
    ///
    /// Si `reply_to` trae un `wa_message_id` (wamid…), Meta lo recibe como
    /// `context.message_id` y la burbuja sale citada en el chat del cliente.
    pub async fn send_text(&self, to: &str, body: &str, reply_to: Option<&str>) -> Result<String> {
        let mut payload = json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to,
            "type": "text",
            "text": { "preview_url": false, "body": body }
        });

        if let Some(wamid) = reply_to {
            payload["context"] = json!({ "message_id": wamid });
        }

        let resp = self.client
            .post(self.messages_url())
            .bearer_auth(&self.access_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| describe_reqwest_error("send_text request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("WhatsApp send_text error [{}]: {}", status, body));
        }

        let json: serde_json::Value = resp.json().await
            .map_err(|e| describe_reqwest_error("send_text response decode", e))?;
        let wa_id = json["messages"][0]["id"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(wa_id)
    }

    /// Marca un mensaje entrante como leído (actualiza los ticks en el cliente).
    pub async fn mark_as_read(&self, wa_message_id: &str) -> Result<()> {
        let payload = json!({
            "messaging_product": "whatsapp",
            "status": "read",
            "message_id": wa_message_id
        });

        let resp = self.client
            .post(self.messages_url())
            .bearer_auth(&self.access_token)
            .json(&payload)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("mark_as_read error [{}]: {}", status, body);
        }

        Ok(())
    }
}
