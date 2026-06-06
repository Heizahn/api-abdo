use crate::models::whatsapp::{
    InboundContext, InboundMedia, InboundMessage, LocationPayload, WebhookValue,
};

/// Información normalizada derivada de un mensaje entrante para persistencia.
#[derive(Debug)]
pub(crate) struct InboundNormalizedContent {
    pub(crate) body: Option<String>,
    pub(crate) media_id: Option<String>,
    pub(crate) media_mime_type: Option<String>,
    pub(crate) media_filename: Option<String>,
    pub(crate) interactive_payload: Option<serde_json::Value>,
    pub(crate) contacts_payload: Option<serde_json::Value>,
    pub(crate) location_payload: Option<LocationPayload>,
    pub(crate) voice: bool,
}

pub(crate) fn infer_inbound_effective_type(msg: &InboundMessage) -> String {
    let original = msg.msg_type.trim().to_ascii_lowercase();
    let inferred = if msg.video.is_some() {
        Some("video")
    } else if msg.image.is_some() {
        Some("image")
    } else if msg.document.is_some() {
        Some("document")
    } else if msg.audio.is_some() {
        Some("audio")
    } else if msg.sticker.is_some() {
        Some("sticker")
    } else if msg.location.is_some() {
        Some("location")
    } else if msg.contacts.is_some() {
        Some("contacts")
    } else if msg.interactive.is_some() {
        Some("interactive")
    } else if msg.button.is_some() {
        Some("button")
    } else if msg.reaction.is_some() {
        Some("reaction")
    } else if msg.edit.is_some() {
        Some("edit")
    } else if msg.revoke.is_some() {
        Some("revoke")
    } else if msg.group.is_some() {
        Some("group")
    } else if msg.extra.contains_key("order") {
        Some("order")
    } else if msg.extra.contains_key("system") {
        Some("system")
    } else if msg.extra.contains_key("referral") {
        Some("referral")
    } else if msg.text.is_some() {
        Some("text")
    } else {
        None
    };

    let should_override = matches!(original.as_str(), "" | "unsupported" | "unknown")
        || msg.edit.is_some()
        || msg.revoke.is_some()
        || msg.group.is_some()
        || (original == "text" && msg.text.is_none() && inferred.is_some());

    if should_override {
        inferred.unwrap_or("unsupported").to_string()
    } else {
        original
    }
}

pub(crate) fn inbound_payload_markers(msg: &InboundMessage) -> String {
    let mut markers = Vec::new();
    if msg.text.is_some() {
        markers.push("text");
    }
    if msg.image.is_some() {
        markers.push("image");
    }
    if msg.document.is_some() {
        markers.push("document");
    }
    if msg.audio.is_some() {
        markers.push("audio");
    }
    if msg.video.is_some() {
        markers.push("video");
    }
    if msg.sticker.is_some() {
        markers.push("sticker");
    }
    if msg.location.is_some() {
        markers.push("location");
    }
    if msg.contacts.is_some() {
        markers.push("contacts");
    }
    if msg.interactive.is_some() {
        markers.push("interactive");
    }
    if msg.button.is_some() {
        markers.push("button");
    }
    if msg.edit.is_some() {
        markers.push("edit");
    }
    if msg.revoke.is_some() {
        markers.push("revoke");
    }
    if msg.group.is_some() {
        markers.push("group");
    }
    if msg.reaction.is_some() {
        markers.push("reaction");
    }
    for key in msg.extra.keys() {
        markers.push(key.as_str());
    }
    if markers.is_empty() {
        "none".to_string()
    } else {
        markers.join(",")
    }
}

pub(crate) fn should_store_raw_payload(msg_type: &str) -> bool {
    matches!(
        msg_type,
        "order" | "system" | "referral" | "edit" | "revoke" | "group"
    ) || !is_known_inbound_type(msg_type)
}

pub(crate) fn is_known_inbound_type(msg_type: &str) -> bool {
    matches!(
        msg_type,
        "text"
            | "image"
            | "document"
            | "audio"
            | "video"
            | "sticker"
            | "location"
            | "contacts"
            | "interactive"
            | "button"
            | "reaction"
            | "edit"
            | "revoke"
            | "group"
            | "order"
            | "system"
            | "referral"
            | "unsupported"
            | "unknown"
    )
}

pub(crate) fn inbound_raw_payload(
    msg: &InboundMessage,
    effective_msg_type: &str,
) -> Option<serde_json::Value> {
    if should_store_raw_payload(effective_msg_type) {
        serde_json::to_value(msg).ok()
    } else {
        None
    }
}

pub(crate) fn build_top_level_delta_message(
    value: &WebhookValue,
    effective_msg_type: &str,
) -> Option<InboundMessage> {
    let payload = match effective_msg_type {
        "edit" => value.edit.as_ref()?,
        "revoke" => value.revoke.as_ref()?,
        _ => return None,
    };

    let id = payload
        .pointer("/id")
        .and_then(|v| v.as_str())
        .or_else(|| payload.pointer("/message/id").and_then(|v| v.as_str()))
        .or_else(|| payload.pointer("/message/wa_id").and_then(|v| v.as_str()))
        .or_else(|| payload.pointer("/wa_id").and_then(|v| v.as_str()))
        .filter(|id| !id.trim().is_empty())
        .unwrap_or("webhook.top.level");

    let from = value
        .contacts
        .as_ref()
        .and_then(|contacts| contacts.first())
        .and_then(|c| c.wa_id.as_deref())
        .unwrap_or("webhook")
        .to_string();

    let timestamp = payload
        .pointer("/timestamp")
        .and_then(|v| v.as_str())
        .or_else(|| {
            payload
                .pointer("/message/timestamp")
                .and_then(|v| v.as_str())
        })
        .map(ToString::to_string);

    let target_context_id = extract_inbound_payload_target_wa_id(payload).and_then(|id| {
        let id = id.trim();
        if id.is_empty() {
            None
        } else {
            Some(id)
        }
    });

    let context = target_context_id.map(|id| InboundContext {
        id: id.to_string(),
        from: None,
    });

    Some(InboundMessage {
        from,
        id: id.to_string(),
        timestamp,
        msg_type: "text".to_string(),
        text: None,
        image: None,
        document: None,
        audio: None,
        video: None,
        sticker: None,
        location: None,
        contacts: None,
        interactive: None,
        button: None,
        reaction: None,
        edit: if effective_msg_type == "edit" {
            Some(payload.clone())
        } else {
            None
        },
        revoke: if effective_msg_type == "revoke" {
            Some(payload.clone())
        } else {
            None
        },
        group: None,
        context,
        extra: Default::default(),
    })
}

pub(crate) fn extract_media_fields(
    media: Option<&InboundMedia>,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    media
        .map(|m| {
            (
                m.caption.clone(),
                m.id.clone(),
                m.mime_type.clone(),
                m.filename.clone(),
            )
        })
        .unwrap_or((None, None, None, None))
}

pub(crate) fn extract_inbound_content(
    msg: &InboundMessage,
    effective_msg_type: &str,
) -> InboundNormalizedContent {
    let extract_media = |media: Option<&InboundMedia>| extract_media_fields(media);

    let (
        body,
        media_id,
        media_mime_type,
        media_filename,
        interactive_payload,
        contacts_payload,
        location_payload,
        voice,
    ) = match effective_msg_type {
        "text" => (
            msg.text.as_ref().map(|t| t.body.clone()),
            None,
            None,
            None,
            None,
            None,
            None,
            false,
        ),
        "image" => {
            let (body, id, mime, filename) = extract_media(msg.image.as_ref());
            (body, id, mime, filename, None, None, None, false)
        }
        "document" => {
            let (body, id, mime, filename) = extract_media(msg.document.as_ref());
            (body, id, mime, filename, None, None, None, false)
        }
        "audio" => {
            let (body, id, mime, filename) = extract_media(msg.audio.as_ref());
            let voice = msg.audio.as_ref().and_then(|a| a.voice).unwrap_or(false);
            (body, id, mime, filename, None, None, None, voice)
        }
        "video" => {
            let (body, id, mime, filename) = extract_media(msg.video.as_ref());
            (body, id, mime, filename, None, None, None, false)
        }
        "sticker" => {
            let body = msg
                .sticker
                .as_ref()
                .and_then(|m| m.caption.clone())
                .or_else(|| Some("[Sticker]".to_string()));

            (
                body.or_else(|| msg.text.as_ref().map(|text| text.body.clone())),
                msg.sticker.as_ref().and_then(|m| m.id.clone()),
                msg.sticker.as_ref().and_then(|m| m.mime_type.clone()),
                None,
                None,
                None,
                None,
                false,
            )
        }
        "edit" => {
            let label = normalize_delta_body(
                extract_text_from_payload(msg.edit.as_ref(), &["/text", "/caption", "/message"])
                    .or_else(|| msg.text.as_ref().map(|text| text.body.clone())),
                "Mensaje editado",
            );
            (Some(label), None, None, None, None, None, None, false)
        }
        "revoke" => {
            let label = normalize_delta_body(
                extract_text_from_payload(msg.revoke.as_ref(), &["/text", "/message", "/reason"])
                    .or_else(|| msg.text.as_ref().map(|text| text.body.clone())),
                "Mensaje revocado",
            );
            (Some(label), None, None, None, None, None, None, false)
        }
        "group" => {
            let label = extract_text_from_payload(msg.group.as_ref(), &["/text", "/body", "/name"])
                .or_else(|| msg.text.as_ref().map(|text| text.body.clone()));
            (
                label.or_else(|| Some("Evento de grupo de WhatsApp".to_string())),
                None,
                None,
                None,
                None,
                None,
                None,
                false,
            )
        }
        "location" => (
            msg.location
                .as_ref()
                .and_then(|l| l.name.clone().or_else(|| l.address.clone()))
                .or_else(|| Some("Ubicación".to_string())),
            None,
            None,
            None,
            None,
            None,
            msg.location
                .as_ref()
                .and_then(|l| match (l.latitude, l.longitude) {
                    (Some(lat), Some(lng)) => Some(LocationPayload {
                        latitude: lat,
                        longitude: lng,
                        name: l.name.clone(),
                        address: l.address.clone(),
                    }),
                    _ => None,
                }),
            false,
        ),
        "interactive" => (
            msg.interactive.as_ref().and_then(|v| {
                v.get("button_reply")
                    .and_then(|b| b.get("title"))
                    .or_else(|| v.get("list_reply").and_then(|l| l.get("title")))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            }),
            None,
            None,
            None,
            msg.interactive.clone(),
            None,
            None,
            false,
        ),
        "button" => (
            msg.button.as_ref().and_then(|v| {
                v.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            }),
            None,
            None,
            None,
            msg.button.clone(),
            None,
            None,
            false,
        ),
        "contacts" => (
            msg.contacts
                .as_ref()
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|c| c.get("name"))
                .and_then(|n| {
                    n.get("formatted_name")
                        .or_else(|| n.get("first_name"))
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                }),
            None,
            None,
            None,
            None,
            msg.contacts.clone(),
            None,
            false,
        ),
        "order" => {
            let label = msg
                .extra
                .get("order")
                .and_then(|v| first_string_at(v, &["/text", "/catalog_id"]).map(|s| s.to_string()))
                .unwrap_or_else(|| "Pedido de WhatsApp".to_string());
            (Some(label), None, None, None, None, None, None, false)
        }
        "system" => {
            let label = msg
                .extra
                .get("system")
                .and_then(|v| {
                    first_string_at(v, &["/body", "/message", "/type"]).map(|s| s.to_string())
                })
                .unwrap_or_else(|| "Mensaje de sistema".to_string());
            (Some(label), None, None, None, None, None, None, false)
        }
        "referral" => {
            let label = msg
                .extra
                .get("referral")
                .and_then(|v| {
                    first_string_at(v, &["/headline", "/body", "/source_type"])
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| "Referencia de WhatsApp".to_string());
            (Some(label), None, None, None, None, None, None, false)
        }
        "unsupported" | "unknown" => (
            Some("Mensaje no soportado por WhatsApp".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            false,
        ),
        _ => {
            if is_known_inbound_type(effective_msg_type) {
                (None, None, None, None, None, None, None, false)
            } else {
                (
                    Some(format!("Mensaje de WhatsApp ({})", effective_msg_type)),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                )
            }
        }
    };

    InboundNormalizedContent {
        body,
        media_id,
        media_mime_type,
        media_filename,
        interactive_payload,
        contacts_payload,
        location_payload,
        voice,
    }
}

pub(crate) fn first_string_at<'a>(value: &'a serde_json::Value, paths: &[&str]) -> Option<&'a str> {
    paths
        .iter()
        .find_map(|path| value.pointer(path).and_then(|v| v.as_str()))
}

pub(crate) fn extract_text_from_payload(
    payload: Option<&serde_json::Value>,
    paths: &[&str],
) -> Option<String> {
    payload.and_then(|payload| first_string_at(payload, paths).map(ToString::to_string))
}

pub(crate) fn normalize_delta_body(content: Option<String>, fallback: &str) -> String {
    match content {
        Some(text) if !text.trim().is_empty() => text,
        _ => fallback.to_string(),
    }
}

pub(crate) fn describe_top_level_group(value: &WebhookValue) -> Option<String> {
    let group = value.group.as_ref()?;

    let id = first_string_at(group, &["/id", "/group_id", "/wa_id"]).unwrap_or("");
    let name = first_string_at(group, &["/name", "/subject", "/title", "/text"]).unwrap_or("");
    let reason = first_string_at(group, &["/reason"]).unwrap_or("");
    let key_count = group.as_object().map(|o| o.len()).unwrap_or(0);

    Some(format!(
        "top-level group id='{id}' name='{name}' reason='{reason}' keys={key_count}"
    ))
}

pub(crate) fn extract_inbound_payload_target_wa_id(payload: &serde_json::Value) -> Option<&str> {
    const TARGET_PATHS: [&str; 6] = [
        "/context/id",
        "/message/context/id",
        "/message/id",
        "/message/wa_id",
        "/id",
        "/wa_id",
    ];

    TARGET_PATHS
        .iter()
        .find_map(|path| first_string_at(payload, std::slice::from_ref(path)))
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

pub(crate) fn extract_inbound_delta_target_wa_id(msg: &InboundMessage) -> Option<&str> {
    msg.context
        .as_ref()
        .and_then(|ctx| {
            let id = ctx.id.trim();
            if id.is_empty() {
                None
            } else {
                Some(id)
            }
        })
        .or_else(|| {
            msg.edit
                .as_ref()
                .and_then(extract_inbound_payload_target_wa_id)
                .or_else(|| {
                    msg.revoke
                        .as_ref()
                        .and_then(extract_inbound_payload_target_wa_id)
                })
        })
}

pub(crate) fn should_apply_message_delta_update(effective_msg_type: &str) -> bool {
    matches!(effective_msg_type, "edit" | "revoke")
}
