use anyhow::Result;
use serde_json::json;
use std::error::Error as StdError;

const WA_API_VERSION: &str = "v25.0";

// ---------------------------------------------------------------------------
// Tipos para la integración con Meta Templates API
// ---------------------------------------------------------------------------

/// Error estructurado que Meta devuelve en el cuerpo de un 4xx/5xx.
/// Cuando el service detecta este shape, lo envuelve en `anyhow::Error`
/// para que el handler pueda hacer `err.downcast_ref::<MetaApiError>()`
/// y convertirlo en la respuesta apropiada (p.ej. `meta_rejected`,
/// `meta_edit_rate_limited`).
#[derive(Debug, thiserror::Error)]
#[error("Meta API error [{code}]: {message}")]
pub struct MetaApiError {
    pub code: i64,
    pub message: String,
    pub error_subcode: Option<i64>,
    pub error_user_msg: Option<String>,
}

/// Respuesta de Meta al crear un template. Contiene el `id` interno que Meta
/// le asigna (equivalente a `hsm_id`) y el `status` inicial — normalmente
/// `"PENDING"`, pero puede ser `"REJECTED"` si Meta lo rechaza de inmediato.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MetaTemplateCreateResp {
    pub id: String,
    pub status: String,
    /// Algunos rejects traen `category` como campo adicional en la respuesta.
    /// Se devuelve crudo para que el handler decida qué hacer con él.
    pub category: Option<String>,
}

/// Snapshot del estado de un template en Meta. Usado para resync manual.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MetaTemplateInfo {
    /// `APPROVED` | `IN_REVIEW` | `PENDING` | `REJECTED` | `FLAGGED` | `DISABLED` | `PAUSED`.
    pub status: String,
    /// Razón cuando `status` ∈ {REJECTED}. Meta usa el campo `rejected_reason`
    /// en este endpoint (NO `reason` como en el webhook).
    pub rejected_reason: Option<String>,
    pub category: Option<String>,
}

/// Metadata de un media antes de bajar el binario.
/// `url` es firmada por Meta (TTL ~5 min) — no cachearla.
pub struct MediaInfo {
    pub url: String,
    pub mime: String,
    pub file_size: Option<u64>,
    pub file_name: Option<String>,
}

/// Snapshot de un phone_number_id devuelto por Meta. Usado por `test-connection`
/// para verificar que el par (phone_number_id, access_token) es válido y para
/// devolver al front el nombre verificado y el formato display que la UI muestra.
#[derive(Debug, Clone)]
pub struct MetaPhoneInfo {
    pub id: String,
    pub verified_name: Option<String>,
    pub display_phone_number: Option<String>,
}

/// Intenta parsear el body de un error 4xx/5xx de Meta como `MetaApiError`.
/// Si el body tiene la forma `{ "error": { "code", "message", ... } }` retorna
/// `Err(anyhow::Error::new(MetaApiError { ... }))` para que el handler pueda
/// hacer downcast. Si el body no tiene ese shape (respuesta inesperada),
/// retorna un `anyhow::Error` genérico con el texto crudo.
fn parse_meta_error(ctx: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(err_obj) = v.get("error") {
            let code = err_obj["code"].as_i64().unwrap_or(0);
            let message = err_obj["message"].as_str().unwrap_or("unknown").to_string();
            let error_subcode = err_obj["error_subcode"].as_i64();
            let error_user_msg = err_obj["error_user_msg"].as_str().map(|s| s.to_string());
            return anyhow::Error::new(MetaApiError {
                code,
                message,
                error_subcode,
                error_user_msg,
            });
        }
    }
    anyhow::anyhow!("{} error [{}]: {}", ctx, status, body)
}

/// Formatea un `reqwest::Error` con toda la cadena de causas (is_timeout,
/// is_connect, source()...) para que el log muestre la razón real en vez
/// del texto genérico "error sending request for url".
fn describe_reqwest_error(ctx: &str, e: reqwest::Error) -> anyhow::Error {
    let mut flags = Vec::new();
    if e.is_timeout() {
        flags.push("timeout");
    }
    if e.is_connect() {
        flags.push("connect");
    }
    if e.is_request() {
        flags.push("request");
    }
    if e.is_body() {
        flags.push("body");
    }
    if e.is_decode() {
        flags.push("decode");
    }

    let mut chain = format!("{}", e);
    let mut src: Option<&dyn StdError> = e.source();
    while let Some(s) = src {
        chain.push_str(" | caused by: ");
        chain.push_str(&s.to_string());
        src = s.source();
    }

    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(","))
    };
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

/// Config opcional del relay de Cloudflare Worker para descarga de media.
/// Se activa cuando la VM no puede conectar directo a `lookaside.fbsbx.com`.
#[derive(Clone)]
pub struct MediaRelay {
    pub url: String,
    pub secret: String,
}

pub struct WhatsAppService {
    access_token: String,
    phone_number_id: String,
    client: reqwest::Client,
    media_relay: Option<MediaRelay>,
}

impl WhatsAppService {
    /// Construye el service con credenciales explícitas (provienen de `WaSettings`,
    /// cifrado descifrado in-memory).
    pub fn new(client: reqwest::Client, phone_number_id: String, access_token: String) -> Self {
        Self {
            access_token,
            phone_number_id,
            client,
            media_relay: None,
        }
    }

    /// Builder: activa el relay de Cloudflare para descargas de media.
    pub fn with_media_relay(mut self, relay: MediaRelay) -> Self {
        self.media_relay = Some(relay);
        self
    }

    /// Construye un `RequestBuilder` para una URL de Meta
    /// (`graph.facebook.com` o `lookaside.fbsbx.com`). Si hay relay
    /// configurado, redirige al Worker con la URL destino como query
    /// param y el secret en header — transparente para el caller.
    /// Aplica a GET/POST/cualquier método.
    fn meta_request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        match &self.media_relay {
            Some(relay) => self
                .client
                .request(method, &relay.url)
                .query(&[("url", url)])
                .header("x-relay-secret", &relay.secret),
            None => self.client.request(method, url),
        }
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
    ///
    /// `preview_url = true` → Meta fetchea OG tags de la URL incluida en `body`
    /// (si hay) y renderiza la tarjeta de preview en el teléfono del cliente.
    pub async fn send_text(
        &self,
        to: &str,
        body: &str,
        reply_to: Option<&str>,
        preview_url: bool,
    ) -> Result<String> {
        let mut payload = json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to,
            "type": "text",
            "text": { "preview_url": preview_url, "body": body }
        });

        if let Some(wamid) = reply_to {
            payload["context"] = json!({ "message_id": wamid });
        }

        let url = self.messages_url();
        let resp = send_with_retry("send_text", || {
            self.meta_request(reqwest::Method::POST, &url)
                .bearer_auth(&self.access_token)
                .json(&payload)
        })
        .await
        .map_err(|e| describe_reqwest_error("send_text request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "WhatsApp send_text error [{}]: {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| describe_reqwest_error("send_text response decode", e))?;
        let wa_id = json["messages"][0]["id"].as_str().unwrap_or("").to_string();

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
            self.meta_request(reqwest::Method::POST, &url)
                .bearer_auth(&self.access_token)
                .json(&payload)
        })
        .await
        .map_err(|e| describe_reqwest_error("send_template request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "WhatsApp send_template error [{}]: {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| describe_reqwest_error("send_template response decode", e))?;
        let wa_id = json["messages"][0]["id"].as_str().unwrap_or("").to_string();

        Ok(wa_id)
    }

    /// Verifica el par `(phone_number_id, access_token)` contra Meta.
    /// `GET /v25.0/{phone_number_id}?fields=id,verified_name,display_phone_number`.
    /// Devuelve la metadata del número si las credenciales son válidas. En caso
    /// contrario, propaga `MetaApiError` cuando el body trae el shape estándar
    /// de error de Meta — el handler hace `downcast_ref` para mapearlo a un
    /// `ApiError` con `code` estable.
    pub async fn test_phone_number(&self) -> Result<MetaPhoneInfo> {
        let url = format!(
            "https://graph.facebook.com/{}/{}?fields=id,verified_name,display_phone_number",
            WA_API_VERSION, self.phone_number_id
        );
        let resp = send_with_retry("test_phone_number", || {
            self.meta_request(reqwest::Method::GET, &url)
                .bearer_auth(&self.access_token)
        })
        .await
        .map_err(|e| describe_reqwest_error("test_phone_number request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(parse_meta_error("test_phone_number", status, &body));
        }

        let info: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| describe_reqwest_error("test_phone_number decode", e))?;

        Ok(MetaPhoneInfo {
            id: info["id"]
                .as_str()
                .unwrap_or(&self.phone_number_id)
                .to_string(),
            verified_name: info["verified_name"].as_str().map(|s| s.to_string()),
            display_phone_number: info["display_phone_number"].as_str().map(|s| s.to_string()),
        })
    }

    /// Info de un media: URL firmada por Meta (TTL ~5 min), mime, tamaño y filename.
    pub async fn download_media_info(&self, media_id: &str) -> Result<MediaInfo> {
        let info_url = format!("https://graph.facebook.com/{}/{}", WA_API_VERSION, media_id);
        let resp = send_with_retry("download_media info", || {
            self.meta_request(reqwest::Method::GET, &info_url)
                .bearer_auth(&self.access_token)
        })
        .await
        .map_err(|e| describe_reqwest_error("download_media info", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "WhatsApp media info error [{}]: {}",
                status,
                body
            ));
        }

        let info: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| describe_reqwest_error("download_media info decode", e))?;
        let url = info["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Meta media response sin campo `url`"))?
            .to_string();
        let mime = info["mime_type"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .to_string();
        let file_name = info["file_name"].as_str().map(|s| s.to_string());
        let file_size = info["file_size"].as_u64();

        Ok(MediaInfo {
            url,
            mime,
            file_size,
            file_name,
        })
    }

    /// Descarga el binario desde la URL firmada que devolvió `download_media_info`.
    /// Se fuerza `Accept: */*` porque la CDN de Meta a veces responde 406 con
    /// los headers por defecto de reqwest.
    ///
    /// Si hay `media_relay` configurado, el request va al Worker de Cloudflare
    /// con la URL de Meta como query param. El Worker valida el secret y el
    /// host, y reenvía transparentemente. Existe como workaround al bloqueo
    /// de red desde la VM hacia `lookaside.fbsbx.com`.
    pub async fn download_media_body(&self, url: &str) -> Result<Vec<u8>> {
        let bin = send_with_retry("download_media bytes", || {
            self.meta_request(reqwest::Method::GET, url)
                .bearer_auth(&self.access_token)
                .header(reqwest::header::ACCEPT, "*/*")
        })
        .await
        .map_err(|e| describe_reqwest_error("download_media bytes", e))?;

        if !bin.status().is_success() {
            let status = bin.status();
            let body = bin.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "WhatsApp media download error [{}]: {}",
                status,
                body
            ));
        }

        let bytes = bin
            .bytes()
            .await
            .map_err(|e| describe_reqwest_error("download_media body", e))?;

        Ok(bytes.to_vec())
    }

    /// Descarga completa: info + body. Se mantiene para compatibilidad con
    /// callers que no necesitan chequear tamaño antes de bajar.
    pub async fn download_media(
        &self,
        media_id: &str,
    ) -> Result<(Vec<u8>, String, Option<String>)> {
        let info = self.download_media_info(media_id).await?;
        let bytes = self.download_media_body(&info.url).await?;
        Ok((bytes, info.mime, info.file_name))
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
            self.meta_request(reqwest::Method::GET, &url)
                .bearer_auth(&self.access_token)
        })
        .await
        .map_err(|e| describe_reqwest_error("get_waba_id request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "WhatsApp get_waba_id error [{}]: {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| describe_reqwest_error("get_waba_id decode", e))?;

        let waba_id = json["whatsapp_business_account"]["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Meta response sin whatsapp_business_account.id"))?
            .to_string();

        Ok(waba_id)
    }

    /// Envía un mensaje interactivo (reply buttons, list o cta_url). El objeto
    /// `interactive` se pasa tal cual a Meta — ya debe estar armado con la
    /// forma esperada por `type: "interactive"` según la docs de WhatsApp
    /// Cloud API. Si viene `reply_to` (wamid), se incluye `context.message_id`
    /// para que la burbuja salga citada.
    pub async fn send_interactive(
        &self,
        to: &str,
        interactive: &serde_json::Value,
        reply_to: Option<&str>,
    ) -> Result<String> {
        let mut payload = json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to,
            "type": "interactive",
            "interactive": interactive,
        });

        if let Some(wamid) = reply_to {
            payload["context"] = json!({ "message_id": wamid });
        }

        let url = self.messages_url();
        let resp = send_with_retry("send_interactive", || {
            self.meta_request(reqwest::Method::POST, &url)
                .bearer_auth(&self.access_token)
                .json(&payload)
        })
        .await
        .map_err(|e| describe_reqwest_error("send_interactive request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "WhatsApp send_interactive error [{}]: {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| describe_reqwest_error("send_interactive response decode", e))?;
        let wa_id = json["messages"][0]["id"].as_str().unwrap_or("").to_string();

        Ok(wa_id)
    }

    /// Sube un binario a Meta (`POST /{phone_number_id}/media`) y devuelve el
    /// `media_id` que luego se pasa a `send_image`/`send_video`/`send_document`
    /// /`send_audio`/`send_sticker`. Meta mantiene el archivo ~30 días antes
    /// de borrarlo.
    ///
    /// Ruta via relay de Cloudflare si está configurado (mismo motivo que
    /// `download_media`: desde la VM de Debian/VE el ISP filtra el handshake
    /// TCP a `graph.facebook.com`, no sólo a `lookaside.fbsbx.com`).
    pub async fn upload_media(
        &self,
        bytes: Vec<u8>,
        mime_type: &str,
        filename: Option<&str>,
    ) -> Result<String> {
        let url = format!(
            "https://graph.facebook.com/{}/{}/media",
            WA_API_VERSION, self.phone_number_id
        );

        // Construir el multipart en un closure porque `send_with_retry`
        // necesita poder rearmar el request; `multipart::Form` consume `bytes`,
        // así que clonamos en cada intento. El tamaño máximo ya fue validado
        // en el handler, los clones son puntuales.
        let mime_owned = mime_type.to_string();
        let filename_owned = filename
            .map(|s| s.to_string())
            .unwrap_or_else(|| "upload.bin".to_string());
        let build = || {
            let part = reqwest::multipart::Part::bytes(bytes.clone())
                .file_name(filename_owned.clone())
                .mime_str(&mime_owned)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(bytes.clone()));
            let form = reqwest::multipart::Form::new()
                .text("messaging_product", "whatsapp")
                .text("type", mime_owned.clone())
                .part("file", part);
            self.meta_request(reqwest::Method::POST, &url)
                .bearer_auth(&self.access_token)
                .multipart(form)
        };

        let resp = send_with_retry("upload_media", build)
            .await
            .map_err(|e| describe_reqwest_error("upload_media request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "WhatsApp upload_media error [{}]: {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| describe_reqwest_error("upload_media response decode", e))?;
        let media_id = json["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Meta upload_media response sin `id`"))?
            .to_string();

        Ok(media_id)
    }

    /// Builder interno: agrega `context.message_id` al payload si `reply_to`
    /// viene con un wamid. Usado por todos los `send_*` que soportan citado.
    fn with_reply_to(mut payload: serde_json::Value, reply_to: Option<&str>) -> serde_json::Value {
        if let Some(wamid) = reply_to {
            payload["context"] = json!({ "message_id": wamid });
        }
        payload
    }

    /// Envío genérico: POSTea un `payload` ya armado a `/messages` y extrae
    /// el `wa_message_id`. Todos los `send_*` (excepto los que tienen lógica
    /// extra) delegan acá para compartir retry, error handling y decode.
    async fn post_message(&self, ctx: &'static str, payload: serde_json::Value) -> Result<String> {
        let url = self.messages_url();
        let resp = send_with_retry(ctx, || {
            self.meta_request(reqwest::Method::POST, &url)
                .bearer_auth(&self.access_token)
                .json(&payload)
        })
        .await
        .map_err(|e| describe_reqwest_error(ctx, e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "WhatsApp {} error [{}]: {}",
                ctx,
                status,
                body
            ));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| describe_reqwest_error(ctx, e))?;
        let wa_id = json["messages"][0]["id"].as_str().unwrap_or("").to_string();
        Ok(wa_id)
    }

    /// Envía una imagen (media previamente subido con `upload_media`).
    pub async fn send_image(
        &self,
        to: &str,
        media_id: &str,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<String> {
        let mut image = json!({ "id": media_id });
        if let Some(c) = caption {
            image["caption"] = json!(c);
        }
        let payload = Self::with_reply_to(
            json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "image",
                "image": image,
            }),
            reply_to,
        );
        self.post_message("send_image", payload).await
    }

    /// Envía un video.
    pub async fn send_video(
        &self,
        to: &str,
        media_id: &str,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<String> {
        let mut video = json!({ "id": media_id });
        if let Some(c) = caption {
            video["caption"] = json!(c);
        }
        let payload = Self::with_reply_to(
            json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "video",
                "video": video,
            }),
            reply_to,
        );
        self.post_message("send_video", payload).await
    }

    /// Envía un documento. `filename` controla el nombre visible del archivo
    /// en el chat del cliente.
    pub async fn send_document(
        &self,
        to: &str,
        media_id: &str,
        caption: Option<&str>,
        filename: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<String> {
        let mut document = json!({ "id": media_id });
        if let Some(c) = caption {
            document["caption"] = json!(c);
        }
        if let Some(f) = filename {
            document["filename"] = json!(f);
        }
        let payload = Self::with_reply_to(
            json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "document",
                "document": document,
            }),
            reply_to,
        );
        self.post_message("send_document", payload).await
    }

    /// Envía un audio (push-to-talk o archivo). Meta ignora cualquier caption.
    pub async fn send_audio(
        &self,
        to: &str,
        media_id: &str,
        reply_to: Option<&str>,
    ) -> Result<String> {
        let payload = Self::with_reply_to(
            json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "audio",
                "audio": { "id": media_id },
            }),
            reply_to,
        );
        self.post_message("send_audio", payload).await
    }

    /// Envía un sticker. Meta sólo acepta `image/webp`.
    pub async fn send_sticker(
        &self,
        to: &str,
        media_id: &str,
        reply_to: Option<&str>,
    ) -> Result<String> {
        let payload = Self::with_reply_to(
            json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "sticker",
                "sticker": { "id": media_id },
            }),
            reply_to,
        );
        self.post_message("send_sticker", payload).await
    }

    /// Envía una ubicación geográfica. `name`/`address` son opcionales; Meta
    /// los muestra debajo del mapa en el chat del cliente.
    pub async fn send_location(
        &self,
        to: &str,
        latitude: f64,
        longitude: f64,
        name: Option<&str>,
        address: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<String> {
        let mut location = json!({ "latitude": latitude, "longitude": longitude });
        if let Some(n) = name {
            location["name"] = json!(n);
        }
        if let Some(a) = address {
            location["address"] = json!(a);
        }
        let payload = Self::with_reply_to(
            json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "location",
                "location": location,
            }),
            reply_to,
        );
        self.post_message("send_location", payload).await
    }

    /// Envía uno o más contactos (tarjetas tipo vCard). El array `contacts`
    /// es passthrough al formato de Meta — ver docs para el shape completo
    /// (`name`, `phones`, `emails`, `addresses`, `org`, `birthday`, `urls`).
    pub async fn send_contacts(
        &self,
        to: &str,
        contacts: &[serde_json::Value],
        reply_to: Option<&str>,
    ) -> Result<String> {
        let payload = Self::with_reply_to(
            json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "contacts",
                "contacts": contacts,
            }),
            reply_to,
        );
        self.post_message("send_contacts", payload).await
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
            self.meta_request(reqwest::Method::POST, &url)
                .bearer_auth(&self.access_token)
                .json(&payload)
        })
        .await
        .map_err(|e| describe_reqwest_error("mark_as_read request", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "WhatsApp mark_as_read error [{}]: {}",
                status,
                body
            ));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Template management — Meta Cloud API
    // -----------------------------------------------------------------------

    /// Crea un template en Meta bajo el WABA `waba_id`. Retorna
    /// `MetaTemplateCreateResp { id, status, category }` con los valores que
    /// Meta asigna. `status` suele ser `"PENDING"` (entra a revisión) o, si
    /// Meta lo rechaza de inmediato, `"REJECTED"`.
    ///
    /// `components` debe tener el shape Meta: array JSON con objetos
    /// `{ type, ... }` (HEADER, BODY, FOOTER, BUTTONS).
    ///
    /// En caso de error 4xx de Meta retorna `Err(anyhow::Error::new(MetaApiError { .. }))`
    /// para que el handler pueda hacer `err.downcast_ref::<MetaApiError>()` y
    /// mapearlo a `meta_rejected` con `details`.
    pub async fn create_template_meta(
        &self,
        waba_id: &str,
        name: &str,
        language: &str,
        category: &str,
        components: &serde_json::Value,
    ) -> Result<MetaTemplateCreateResp> {
        let url = format!(
            "https://graph.facebook.com/{}/{}/message_templates",
            WA_API_VERSION, waba_id
        );
        let payload = json!({
            "name": name,
            "language": language,
            "category": category,
            "components": components,
        });

        let resp = send_with_retry("create_template_meta", || {
            self.meta_request(reqwest::Method::POST, &url)
                .bearer_auth(&self.access_token)
                .json(&payload)
        })
        .await
        .map_err(|e| describe_reqwest_error("create_template_meta request", e))?;

        let http_status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| describe_reqwest_error("create_template_meta response read", e))?;

        if !http_status.is_success() {
            return Err(parse_meta_error("create_template_meta", http_status, &body));
        }

        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("create_template_meta response decode: {}", e))?;

        let id = json["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("create_template_meta: Meta response sin `id`"))?
            .to_string();
        let status = json["status"].as_str().unwrap_or("PENDING").to_string();
        let category = json["category"].as_str().map(|s| s.to_string());

        Ok(MetaTemplateCreateResp {
            id,
            status,
            category,
        })
    }

    /// Edita el/los componentes de un template en Meta usando su `meta_template_id`
    /// (`hsm_id`). Meta sólo permite modificar el BODY en plantillas `APPROVED`
    /// (1 edición por día, 10 por mes). Para plantillas en estado `DRAFT` o
    /// `REJECTED`, pasar todos los componentes actualizados.
    ///
    /// `new_components` debe ser un array JSON con los componentes nuevos.
    /// Meta usa POST para este endpoint (no PATCH ni PUT).
    ///
    /// Si Meta responde 429 (rate limit de edición), retorna
    /// `Err(anyhow::Error::new(MetaApiError { code: 429, .. }))` para que el
    /// handler lo convierta en `meta_edit_rate_limited`.
    pub async fn update_template_body_meta(
        &self,
        meta_template_id: &str,
        new_components: &serde_json::Value,
    ) -> Result<()> {
        let url = format!(
            "https://graph.facebook.com/{}/{}",
            WA_API_VERSION, meta_template_id
        );
        let payload = json!({ "components": new_components });

        let resp = send_with_retry("update_template_body_meta", || {
            self.meta_request(reqwest::Method::POST, &url)
                .bearer_auth(&self.access_token)
                .json(&payload)
        })
        .await
        .map_err(|e| describe_reqwest_error("update_template_body_meta request", e))?;

        let http_status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| describe_reqwest_error("update_template_body_meta response read", e))?;

        if http_status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Meta también devuelve 429 con un body de error estándar cuando se
            // supera el límite de 1 edición/día o 10/mes. Mapeamos explícitamente
            // para que el handler use `meta_edit_rate_limited` en la respuesta.
            let meta_err = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    let e = v.get("error")?;
                    Some(MetaApiError {
                        code: 429,
                        message: e["message"].as_str().unwrap_or("rate limited").to_string(),
                        error_subcode: e["error_subcode"].as_i64(),
                        error_user_msg: e["error_user_msg"].as_str().map(|s| s.to_string()),
                    })
                })
                .unwrap_or(MetaApiError {
                    code: 429,
                    message: "Meta edit rate limited".to_string(),
                    error_subcode: None,
                    error_user_msg: None,
                });
            return Err(anyhow::Error::new(meta_err));
        }

        if !http_status.is_success() {
            return Err(parse_meta_error(
                "update_template_body_meta",
                http_status,
                &body,
            ));
        }

        Ok(())
    }

    /// Implementa los 2 pasos del Resumable Upload API de Meta. Devuelve el
    /// `handle` (campo `h` en la respuesta del paso 2) que debe ir en
    /// `components[i].example.header_handle[0]` al crear/editar un template.
    ///
    /// El handle es **single-use y de vida corta** (~30 min). NO cachear.
    ///
    /// # Args
    /// - `app_id`: Meta App ID (de `config.whatsapp_app_id`, no confundir con app_secret).
    /// - `mime`: tipo MIME del binario (`image/jpeg`, `video/mp4`, `application/pdf`, etc).
    /// - `bytes`: binario en memoria (ya validado en tamaño por el caller).
    ///
    /// # Errores
    /// - Si Meta responde 4xx con su shape de error, retorna `MetaApiError` via anyhow
    ///   (downcasteable como hacen los otros métodos).
    /// - Errores de transporte → `describe_reqwest_error`.
    ///
    /// # Paso 1 — Crear upload session (idempotente, usa retry)
    /// `POST https://graph.facebook.com/{WA_API_VERSION}/{app_id}/uploads
    ///   ?file_length={bytes.len()}&file_type={url_encoded_mime}&access_token={token}`
    /// Respuesta: `{ "id": "upload:<hash>" }`
    ///
    /// # Paso 2 — Upload del binario (NO retry — repetir puede romper la sesión)
    /// `POST https://graph.facebook.com/{WA_API_VERSION}/{upload_id}`
    ///   Header: `Authorization: OAuth <token>` (excepción documentada — NO Bearer)
    ///   Header: `file_offset: 0`
    ///   Body: binario raw (no multipart)
    /// Respuesta: `{ "h": "<handle>" }`
    ///
    // TODO: test manual con curl:
    //   curl -X POST "https://graph.facebook.com/v25.0/{app_id}/uploads?file_length=1024&file_type=image/jpeg&access_token=..."
    //   → { "id": "upload:abc..." }
    //   curl -X POST "https://graph.facebook.com/v25.0/upload:abc..." \
    //       -H "Authorization: OAuth ..." \
    //       -H "file_offset: 0" \
    //       --data-binary @img.jpg
    //   → { "h": "..." }
    pub async fn upload_to_meta_resumable(
        &self,
        app_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<String> {
        // ---------------------------------------------------------------
        // Paso 1: crear upload session
        // ---------------------------------------------------------------
        let session_url = format!(
            "https://graph.facebook.com/{}/{}/uploads",
            WA_API_VERSION, app_id
        );

        // Parámetros en query string — el body debe quedar vacío (spec Meta).
        let file_length = bytes.len().to_string();
        let resp1 = send_with_retry("upload_to_meta_resumable step1", || {
            self.meta_request(reqwest::Method::POST, &session_url)
                .query(&[
                    ("file_length", file_length.as_str()),
                    ("file_type", mime),
                    ("access_token", self.access_token.as_str()),
                ])
        })
        .await
        .map_err(|e| describe_reqwest_error("upload_to_meta_resumable step1 request", e))?;

        let status1 = resp1.status();
        let body1 = resp1.text().await.map_err(|e| {
            describe_reqwest_error("upload_to_meta_resumable step1 response read", e)
        })?;

        if !status1.is_success() {
            return Err(parse_meta_error(
                "upload_to_meta_resumable step1",
                status1,
                &body1,
            ));
        }

        let json1: serde_json::Value = serde_json::from_str(&body1)
            .map_err(|e| anyhow::anyhow!("upload_to_meta_resumable step1 decode: {}", e))?;

        let upload_id = json1["id"]
            .as_str()
            .ok_or_else(|| {
                anyhow::anyhow!("upload_to_meta_resumable step1: Meta response sin `id`")
            })?
            .to_string();

        // ---------------------------------------------------------------
        // Paso 2: subir el binario — SIN retry (sesión de upload es stateful)
        // ---------------------------------------------------------------
        let upload_url = format!(
            "https://graph.facebook.com/{}/{}",
            WA_API_VERSION, upload_id
        );

        // Meta exige "Authorization: OAuth <token>" en este endpoint, NO "Bearer".
        // Es una excepción documentada en las Graph API docs para Resumable Upload.
        let resp2 = self
            .meta_request(reqwest::Method::POST, &upload_url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("OAuth {}", self.access_token),
            )
            .header("file_offset", "0")
            .header(reqwest::header::CONTENT_TYPE, mime)
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| describe_reqwest_error("upload_to_meta_resumable step2 request", e))?;

        let status2 = resp2.status();
        let body2 = resp2.text().await.map_err(|e| {
            describe_reqwest_error("upload_to_meta_resumable step2 response read", e)
        })?;

        if !status2.is_success() {
            return Err(parse_meta_error(
                "upload_to_meta_resumable step2",
                status2,
                &body2,
            ));
        }

        let json2: serde_json::Value = serde_json::from_str(&body2)
            .map_err(|e| anyhow::anyhow!("upload_to_meta_resumable step2 decode: {}", e))?;

        let handle = json2["h"]
            .as_str()
            .ok_or_else(|| {
                anyhow::anyhow!("upload_to_meta_resumable step2: Meta response sin `h`")
            })?
            .to_string();

        Ok(handle)
    }

    /// Lee el estado actual de un template desde Meta. Usado para resync
    /// manual cuando se perdió un webhook de status update.
    /// Endpoint Meta: `GET /{meta_template_id}?fields=status,...`.
    pub async fn get_template_meta(&self, meta_template_id: &str) -> Result<MetaTemplateInfo> {
        let url = format!(
            "https://graph.facebook.com/{}/{}?fields=status,rejected_reason,category,language,name",
            WA_API_VERSION, meta_template_id
        );

        let resp = send_with_retry("get_template_meta", || {
            self.meta_request(reqwest::Method::GET, &url)
                .bearer_auth(&self.access_token)
        })
        .await
        .map_err(|e| describe_reqwest_error("get_template_meta request", e))?;

        let http_status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| describe_reqwest_error("get_template_meta response read", e))?;

        if !http_status.is_success() {
            return Err(parse_meta_error("get_template_meta", http_status, &body));
        }

        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("get_template_meta decode: {}", e))?;

        Ok(MetaTemplateInfo {
            status: json["status"].as_str().unwrap_or("").to_string(),
            rejected_reason: json["rejected_reason"].as_str().map(|s| s.to_string()),
            category: json["category"].as_str().map(|s| s.to_string()),
        })
    }

    /// Borra UNA traducción de un template en Meta. `hsm_id` es el `id` que
    /// Meta asignó al template (campo `meta_template_id` en `WaTemplates`).
    /// Pasar `hsm_id` es obligatorio — sin él Meta borra **todas** las
    /// traducciones del mismo `name`.
    ///
    /// Si Meta responde 404 (template ya no existe del lado de Meta) se
    /// retorna `Ok(())` con un `tracing::warn!` — garantiza idempotencia.
    /// Otros errores de Meta retornan `Err(anyhow::Error::new(MetaApiError))`.
    pub async fn delete_template_meta(
        &self,
        waba_id: &str,
        hsm_id: &str,
        name: &str,
    ) -> Result<()> {
        let url = format!(
            "https://graph.facebook.com/{}/{}/message_templates",
            WA_API_VERSION, waba_id
        );

        // Meta no soporta retry automático en DELETE con los mismos parámetros
        // sin riesgo de doble borrado, pero `send_with_retry` sólo reintenta
        // ante errores de transporte (timeout/connect), no errores de aplicación,
        // así que es seguro usarlo aquí.
        let resp = send_with_retry("delete_template_meta", || {
            self.meta_request(reqwest::Method::DELETE, &url)
                .bearer_auth(&self.access_token)
                .query(&[("hsm_id", hsm_id), ("name", name)])
        })
        .await
        .map_err(|e| describe_reqwest_error("delete_template_meta request", e))?;

        let http_status = resp.status();

        if http_status == reqwest::StatusCode::NOT_FOUND {
            // El template ya no existe en Meta. Es idempotente: ignoramos y
            // continuamos para que el caller borre el doc local de todas formas.
            tracing::warn!(
                hsm_id = %hsm_id,
                name = %name,
                waba_id = %waba_id,
                "delete_template_meta: Meta devolvió 404 — template ya no existe en Meta, \
                 procediendo con borrado local"
            );
            return Ok(());
        }

        if !http_status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(parse_meta_error("delete_template_meta", http_status, &body));
        }

        Ok(())
    }
}
