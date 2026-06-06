use crate::error::ApiError;
use crate::models::whatsapp::LocationPayload;
use crate::modules::whatsapp::conversations::outbound::map_template_send_error;
use crate::modules::whatsapp::service::WhatsAppService;

use super::mode::SendMode;
use super::preview::{interactive_preview, template_preview};

pub(crate) struct TemplateFields {
    pub(crate) name: String,
    pub(crate) language: String,
    pub(crate) components: Option<serde_json::Value>,
}

/// Resultado de despachar un `SendMode` al service: contiene todo lo que el
/// handler necesita para persistir el `WaMessage` + armar la `ConversationTouch`.
pub(crate) struct SentData {
    pub(crate) wa_id: String,
    pub(crate) preview: String,
    pub(crate) msg_type: &'static str,
    pub(crate) body: Option<String>,
    pub(crate) media_id: Option<String>,
    pub(crate) media_filename: Option<String>,
    pub(crate) media_mime_type: Option<String>,
    pub(crate) template_fields: Option<TemplateFields>,
    pub(crate) interactive_payload: Option<serde_json::Value>,
    pub(crate) contacts_payload: Option<serde_json::Value>,
    pub(crate) location: Option<LocationPayload>,
}

/// Dispatcher único que cubre todos los `SendMode` — usado en el envío nuevo
/// y en el retry idempotente para evitar duplicar lógica.
pub(crate) async fn dispatch_send(
    mode: &SendMode,
    wa: &WhatsAppService,
    to: &str,
    reply_to: Option<&str>,
    preview_url_flag: bool,
) -> Result<SentData, ApiError> {
    let internal = |e: anyhow::Error| ApiError::Internal(e.to_string());
    let res = match mode {
        SendMode::Text { content } => {
            let wa_id = wa
                .send_text(to, content, reply_to, preview_url_flag)
                .await
                .map_err(internal)?;
            SentData {
                wa_id,
                preview: content.clone(),
                msg_type: "text",
                body: Some(content.clone()),
                media_id: None,
                media_filename: None,
                media_mime_type: None,
                template_fields: None,
                interactive_payload: None,
                contacts_payload: None,
                location: None,
            }
        }
        SendMode::Template { tpl } => {
            let components_value = tpl
                .components
                .as_ref()
                .map(|v| serde_json::Value::Array(v.clone()));
            let wa_id = wa
                .send_template(to, &tpl.name, &tpl.language, components_value.as_ref())
                .await
                .map_err(|e| map_template_send_error(&e))?;
            let prev = template_preview(tpl);
            let body = tpl.rendered_text.clone().or_else(|| Some(prev.clone()));
            let fields = TemplateFields {
                name: tpl.name.clone(),
                language: tpl.language.clone(),
                components: tpl
                    .components
                    .as_ref()
                    .map(|v| serde_json::Value::Array(v.clone())),
            };
            SentData {
                wa_id,
                preview: prev,
                msg_type: "template",
                body,
                media_id: None,
                media_filename: None,
                media_mime_type: None,
                template_fields: Some(fields),
                interactive_payload: None,
                contacts_payload: None,
                location: None,
            }
        }
        SendMode::Interactive { payload: inter } => {
            let wa_id = wa
                .send_interactive(to, inter, reply_to)
                .await
                .map_err(internal)?;
            let prev = interactive_preview(inter);
            SentData {
                wa_id,
                preview: prev.clone(),
                msg_type: "interactive",
                body: Some(prev),
                media_id: None,
                media_filename: None,
                media_mime_type: None,
                template_fields: None,
                interactive_payload: Some(inter.clone()),
                contacts_payload: None,
                location: None,
            }
        }
        SendMode::Image { media_id, caption } => {
            let wa_id = wa
                .send_image(to, media_id, caption.as_deref(), reply_to)
                .await
                .map_err(internal)?;
            SentData {
                wa_id,
                preview: caption.clone().unwrap_or_else(|| "[imagen]".into()),
                msg_type: "image",
                body: caption.clone(),
                media_id: Some(media_id.clone()),
                media_filename: None,
                media_mime_type: None,
                template_fields: None,
                interactive_payload: None,
                contacts_payload: None,
                location: None,
            }
        }
        SendMode::Video { media_id, caption } => {
            let wa_id = wa
                .send_video(to, media_id, caption.as_deref(), reply_to)
                .await
                .map_err(internal)?;
            SentData {
                wa_id,
                preview: caption.clone().unwrap_or_else(|| "[video]".into()),
                msg_type: "video",
                body: caption.clone(),
                media_id: Some(media_id.clone()),
                media_filename: None,
                media_mime_type: None,
                template_fields: None,
                interactive_payload: None,
                contacts_payload: None,
                location: None,
            }
        }
        SendMode::Document {
            media_id,
            caption,
            filename,
        } => {
            let wa_id = wa
                .send_document(
                    to,
                    media_id,
                    caption.as_deref(),
                    filename.as_deref(),
                    reply_to,
                )
                .await
                .map_err(internal)?;
            let prev = caption
                .clone()
                .or_else(|| filename.clone())
                .unwrap_or_else(|| "[documento]".into());
            SentData {
                wa_id,
                preview: prev,
                msg_type: "document",
                body: caption.clone(),
                media_id: Some(media_id.clone()),
                media_filename: filename.clone(),
                media_mime_type: None,
                template_fields: None,
                interactive_payload: None,
                contacts_payload: None,
                location: None,
            }
        }
        SendMode::Audio { media_id } => {
            let wa_id = wa
                .send_audio(to, media_id, reply_to)
                .await
                .map_err(internal)?;
            SentData {
                wa_id,
                preview: "[audio]".into(),
                msg_type: "audio",
                body: None,
                media_id: Some(media_id.clone()),
                media_filename: None,
                media_mime_type: None,
                template_fields: None,
                interactive_payload: None,
                contacts_payload: None,
                location: None,
            }
        }
        SendMode::Sticker { media_id } => {
            let wa_id = wa
                .send_sticker(to, media_id, reply_to)
                .await
                .map_err(internal)?;
            SentData {
                wa_id,
                preview: "[sticker]".into(),
                msg_type: "sticker",
                body: None,
                media_id: Some(media_id.clone()),
                media_filename: None,
                media_mime_type: None,
                template_fields: None,
                interactive_payload: None,
                contacts_payload: None,
                location: None,
            }
        }
        SendMode::Location { loc } => {
            let wa_id = wa
                .send_location(
                    to,
                    loc.latitude,
                    loc.longitude,
                    loc.name.as_deref(),
                    loc.address.as_deref(),
                    reply_to,
                )
                .await
                .map_err(internal)?;
            let prev = loc
                .name
                .clone()
                .or_else(|| loc.address.clone())
                .unwrap_or_else(|| "Ubicación".into());
            SentData {
                wa_id,
                preview: prev,
                msg_type: "location",
                body: None,
                media_id: None,
                media_filename: None,
                media_mime_type: None,
                template_fields: None,
                interactive_payload: None,
                contacts_payload: None,
                location: Some(loc.clone()),
            }
        }
        SendMode::Contacts { list } => {
            let wa_id = wa
                .send_contacts(to, list, reply_to)
                .await
                .map_err(internal)?;
            let prev = list
                .first()
                .and_then(|c| c.get("name"))
                .and_then(|n| n.get("formatted_name").or_else(|| n.get("first_name")))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "[contacto]".into());
            SentData {
                wa_id,
                preview: prev,
                msg_type: "contacts",
                body: None,
                media_id: None,
                media_filename: None,
                media_mime_type: None,
                template_fields: None,
                interactive_payload: None,
                contacts_payload: Some(serde_json::Value::Array(list.clone())),
                location: None,
            }
        }
    };

    Ok(res)
}
