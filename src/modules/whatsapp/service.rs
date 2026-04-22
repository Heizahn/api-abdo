use anyhow::Result;
use serde_json::json;

const WA_API_VERSION: &str = "v25.0";

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
    pub async fn send_text(&self, to: &str, body: &str) -> Result<String> {
        let payload = json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to,
            "type": "text",
            "text": { "preview_url": false, "body": body }
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
            return Err(anyhow::anyhow!("WhatsApp send_text error [{}]: {}", status, body));
        }

        let json: serde_json::Value = resp.json().await?;
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
