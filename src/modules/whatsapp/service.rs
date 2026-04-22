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

/// `true` si el error es un fallo transitorio en la capa de transporte que vale
/// la pena reintentar: connect timeout, reset de conexión, body corrupto, etc.
/// Deja pasar errores "de aplicación" (4xx/5xx) que no se resuelven con retry.
fn is_transient_reqwest_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request() || e.is_body()
}

/// Ejecuta un request con un reintento simple ante fallos transitorios (ver
/// `is_transient_reqwest_error`). Backoff fijo de 500 ms para no martillear al
/// CDN de Meta. Loggea en `warn` el primer fallo para diagnóstico.
async fn send_with_retry(
    ctx: &str,
    mut build: impl FnMut() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, reqwest::Error> {
    match build().send().await {
        Ok(r) => Ok(r),
        Err(e) if is_transient_reqwest_error(&e) => {
            tracing::warn!("{} fallo transitorio, reintentando en 500ms: {}", ctx, e);
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            build().send().await
        }
        Err(e) => Err(e),
    }
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

        let url = self.messages_url();
        let resp = send_with_retry("send_text", || {
            self.client.post(&url).bearer_auth(&self.access_token).json(&payload)
        }).await
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

    /// Envía una plantilla aprobada a un número (fuera de la ventana de 24h).
    /// `components` se pasa tal cual a Meta (el front ya interpola los
    /// parámetros). Si es `None`, se envía un template sin placeholders.
    pub async fn send_template(
        &self,
        to: &str,
        template_name: &str,
        language: &str,
        components: Option<&serde_json::Value>,
    ) -> Result<String> {
        let mut template = json!({
            "name": template_name,
            "language": { "code": language }
        });

        if let Some(c) = components {
            template["components"] = c.clone();
        }

        let payload = json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to,
            "type": "template",
            "template": template
        });

        let url = self.messages_url();
        let resp = send_with_retry("send_template", || {
            self.client.post(&url).bearer_auth(&self.access_token).json(&payload)
        }).await
            .map_err(|e| describe_reqwest_error("send_template request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("WhatsApp send_template error [{}]: {}", status, body));
        }

        let json: serde_json::Value = resp.json().await
            .map_err(|e| describe_reqwest_error("send_template response decode", e))?;
        let wa_id = json["messages"][0]["id"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(wa_id)
    }

    /// Descarga el binario de un media subido por un contacto vía webhook.
    ///
    /// Son dos llamadas contra Meta:
    /// 1. `GET /v25.0/{media_id}` → devuelve `{ url, mime_type, file_size, ... }`.
    /// 2. `GET <url>` (con el mismo bearer) → binario.
    ///
    /// Se fuerza `Accept: */*` porque la CDN de Meta a veces responde 406 con
    /// los headers por defecto de reqwest.
    pub async fn download_media(&self, media_id: &str) -> Result<(Vec<u8>, String, Option<String>)> {
        let info_url = format!("https://graph.facebook.com/{}/{}", WA_API_VERSION, media_id);
        let resp = send_with_retry("download_media info", || {
            self.client.get(&info_url).bearer_auth(&self.access_token)
        }).await
            .map_err(|e| describe_reqwest_error("download_media info", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("WhatsApp media info error [{}]: {}", status, body));
        }

        let info: serde_json::Value = resp.json().await
            .map_err(|e| describe_reqwest_error("download_media info decode", e))?;
        let url = info["url"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Meta media response sin campo `url`"))?
            .to_string();
        let mime = info["mime_type"].as_str()
            .unwrap_or("application/octet-stream")
            .to_string();
        let file_name = info["file_name"].as_str().map(|s| s.to_string());

        let bin = send_with_retry("download_media bytes", || {
            self.client
                .get(&url)
                .bearer_auth(&self.access_token)
                .header(reqwest::header::ACCEPT, "*/*")
        }).await
            .map_err(|e| describe_reqwest_error("download_media bytes", e))?;

        if !bin.status().is_success() {
            let status = bin.status();
            let body = bin.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("WhatsApp media download error [{}]: {}", status, body));
        }

        // `bytes()` también puede fallar transitoriamente (reset a mitad de
        // descarga), pero reintentarlo requeriría re-descargar todo. Preferimos
        // fallar y dejar que el front reintente.
        let bytes = bin.bytes().await
            .map_err(|e| describe_reqwest_error("download_media body", e))?;

        Ok((bytes.to_vec(), mime, file_name))
    }

    /// Lista las plantillas (`message_templates`) de una cuenta WABA. La llamada
    /// requiere WABA ID (no phone_number_id) y el mismo bearer de Meta Cloud.
    /// Devuelve el JSON crudo; el filtrado/shaping vive en el handler.
    pub async fn list_templates(&self, waba_id: &str) -> Result<serde_json::Value> {
        let url = format!(
            "https://graph.facebook.com/{}/{}/message_templates?fields=name,language,category,status,components&limit=100",
            WA_API_VERSION, waba_id
        );

        let resp = send_with_retry("list_templates", || {
            self.client.get(&url).bearer_auth(&self.access_token)
        }).await
            .map_err(|e| describe_reqwest_error("list_templates request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("WhatsApp list_templates error [{}]: {}", status, body));
        }

        let json: serde_json::Value = resp.json().await
            .map_err(|e| describe_reqwest_error("list_templates response decode", e))?;

        Ok(json)
    }

    /// Obtiene el WABA ID (`whatsapp_business_account`) asociado a un phone_number_id.
    /// Útil para backfill: admins que ya configuraron `WaSettings` sin WABA no
    /// tienen cómo listar templates hasta que lo llenemos.
    pub async fn get_whatsapp_business_account_id(&self) -> Result<String> {
        let url = format!(
            "https://graph.facebook.com/{}/{}?fields=whatsapp_business_account",
            WA_API_VERSION, self.phone_number_id
        );

        let resp = send_with_retry("get_waba_id", || {
            self.client.get(&url).bearer_auth(&self.access_token)
        }).await
            .map_err(|e| describe_reqwest_error("get_waba_id request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("WhatsApp get_waba_id error [{}]: {}", status, body));
        }

        let json: serde_json::Value = resp.json().await
            .map_err(|e| describe_reqwest_error("get_waba_id decode", e))?;

        let waba_id = json["whatsapp_business_account"]["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Meta response sin whatsapp_business_account.id"))?
            .to_string();

        Ok(waba_id)
    }

    /// Marca un mensaje entrante como leído (ticks azules en texto, mic azul
    /// en voice notes). Meta requiere una llamada POR mensaje — no propaga el
    /// read a los anteriores.
    pub async fn mark_as_read(&self, wa_message_id: &str) -> Result<()> {
        let payload = json!({
            "messaging_product": "whatsapp",
            "status": "read",
            "message_id": wa_message_id
        });

        let url = self.messages_url();
        let resp = send_with_retry("mark_as_read", || {
            self.client.post(&url).bearer_auth(&self.access_token).json(&payload)
        }).await
            .map_err(|e| describe_reqwest_error("mark_as_read request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("WhatsApp mark_as_read error [{}]: {}", status, body));
        }

        Ok(())
    }
}
