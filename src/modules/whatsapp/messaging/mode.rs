use axum::http::StatusCode;
use mongodb::bson::DateTime;

use crate::error::ApiError;
use crate::models::whatsapp::{
    LocationPayload, SendMessageRequest, SendTemplatePayload, WaConversation,
};
use crate::modules::whatsapp::shared::time::{compute_freeform_state, is_within_24h, iso8601};

pub(crate) enum SendMode {
    Text {
        content: String,
    },
    Template {
        tpl: SendTemplatePayload,
    },
    Interactive {
        payload: serde_json::Value,
    },
    Image {
        media_id: String,
        caption: Option<String>,
    },
    Video {
        media_id: String,
        caption: Option<String>,
    },
    Document {
        media_id: String,
        caption: Option<String>,
        filename: Option<String>,
    },
    Audio {
        media_id: String,
    },
    Sticker {
        media_id: String,
    },
    Location {
        loc: LocationPayload,
    },
    Contacts {
        list: Vec<serde_json::Value>,
    },
}

pub(crate) fn resolve_send_mode(
    payload: &SendMessageRequest,
    conv: &WaConversation,
) -> Result<SendMode, ApiError> {
    // Gate de engagement throttle (Meta error 131049): si ya nos rebotó un
    // envío reciente y el cooldown sigue activo, bloqueamos cualquier modo
    // (texto y template). El front debe esperar a que el cliente responda o
    // a que expire `meta_throttle_until`.
    if let Some(until) = conv.meta_throttle_until {
        let now_ms = DateTime::now().timestamp_millis();
        if until.timestamp_millis() > now_ms {
            return Err(ApiError::Domain {
                status: StatusCode::CONFLICT,
                code: "template_throttled_by_meta".into(),
                field: None,
                message: "Meta bloqueó los envíos a este contacto temporalmente \
                    (recibió demasiados mensajes sin responder). Espera a que \
                    responda o vuelve a intentarlo más tarde."
                    .into(),
                details: Some(serde_json::json!({
                    "meta_throttle_until": iso8601(until),
                })),
            });
        }
    }

    // Activamos modo template si viene `type="template"` o si `template` está
    // presente. Ambos caminos requieren el objeto `template`.
    let template_mode = payload
        .msg_type
        .as_deref()
        .map(|t| t.eq_ignore_ascii_case("template"))
        .unwrap_or(false)
        || payload.template.is_some();

    if template_mode {
        let tpl = payload
            .template
            .as_ref()
            .ok_or(ApiError::MissingTemplateParams)?;

        let name = tpl.name.trim();
        let language = tpl.language.trim();
        if name.is_empty() || language.is_empty() {
            return Err(ApiError::MissingTemplateParams);
        }
        return Ok(SendMode::Template { tpl: tpl.clone() });
    }

    // Interactive: requiere ventana de 24h abierta (igual que texto freeform).
    let interactive_mode = payload
        .msg_type
        .as_deref()
        .map(|t| t.eq_ignore_ascii_case("interactive"))
        .unwrap_or(false)
        || payload.interactive.is_some();

    if interactive_mode {
        let inter = payload.interactive.as_ref().ok_or_else(|| {
            ApiError::BadRequest("interactive requerido cuando type=interactive".into())
        })?;

        if !is_within_24h(conv.last_inbound_at) {
            return Err(freeform_window_expired_error(payload, conv, "interactive"));
        }
        return Ok(SendMode::Interactive {
            payload: inter.clone(),
        });
    }

    // Tipos ricos (media + location + contacts). Todos son freeform de cara a
    // Meta, así que exigen ventana de 24h abierta — mismo gate que texto.
    let type_hint = payload.msg_type.as_deref().map(|t| t.to_ascii_lowercase());
    let explicit = |t: &str| type_hint.as_deref() == Some(t);

    if explicit("image") || (type_hint.is_none() && payload.image.is_some()) {
        let m = payload
            .image
            .as_ref()
            .ok_or_else(|| ApiError::BadRequest("image requerido cuando type=image".into()))?;
        validate_media_id(&m.media_id)?;
        if !is_within_24h(conv.last_inbound_at) {
            return Err(freeform_window_expired_error(payload, conv, "image"));
        }
        return Ok(SendMode::Image {
            media_id: m.media_id.clone(),
            caption: nonempty(&m.caption),
        });
    }
    if explicit("video") || (type_hint.is_none() && payload.video.is_some()) {
        let m = payload
            .video
            .as_ref()
            .ok_or_else(|| ApiError::BadRequest("video requerido cuando type=video".into()))?;
        validate_media_id(&m.media_id)?;
        if !is_within_24h(conv.last_inbound_at) {
            return Err(freeform_window_expired_error(payload, conv, "video"));
        }
        return Ok(SendMode::Video {
            media_id: m.media_id.clone(),
            caption: nonempty(&m.caption),
        });
    }
    if explicit("document") || (type_hint.is_none() && payload.document.is_some()) {
        let m = payload.document.as_ref().ok_or_else(|| {
            ApiError::BadRequest("document requerido cuando type=document".into())
        })?;
        validate_media_id(&m.media_id)?;
        if !is_within_24h(conv.last_inbound_at) {
            return Err(freeform_window_expired_error(payload, conv, "document"));
        }
        return Ok(SendMode::Document {
            media_id: m.media_id.clone(),
            caption: nonempty(&m.caption),
            filename: nonempty(&m.filename),
        });
    }
    if explicit("audio") || (type_hint.is_none() && payload.audio.is_some()) {
        let m = payload
            .audio
            .as_ref()
            .ok_or_else(|| ApiError::BadRequest("audio requerido cuando type=audio".into()))?;
        validate_media_id(&m.media_id)?;
        if !is_within_24h(conv.last_inbound_at) {
            return Err(freeform_window_expired_error(payload, conv, "audio"));
        }
        return Ok(SendMode::Audio {
            media_id: m.media_id.clone(),
        });
    }
    if explicit("sticker") || (type_hint.is_none() && payload.sticker.is_some()) {
        let m = payload
            .sticker
            .as_ref()
            .ok_or_else(|| ApiError::BadRequest("sticker requerido cuando type=sticker".into()))?;
        validate_media_id(&m.media_id)?;
        if !is_within_24h(conv.last_inbound_at) {
            return Err(freeform_window_expired_error(payload, conv, "sticker"));
        }
        return Ok(SendMode::Sticker {
            media_id: m.media_id.clone(),
        });
    }
    if explicit("location") || (type_hint.is_none() && payload.location.is_some()) {
        let loc = payload.location.as_ref().ok_or_else(|| {
            ApiError::BadRequest("location requerido cuando type=location".into())
        })?;
        if !loc.latitude.is_finite()
            || !loc.longitude.is_finite()
            || loc.latitude.abs() > 90.0
            || loc.longitude.abs() > 180.0
        {
            return Err(ApiError::ValidationError {
                code: "location_out_of_range".into(),
                field: "location".into(),
                message: "La latitud debe estar entre -90 y 90, y la longitud entre -180 y 180."
                    .into(),
            });
        }
        if !is_within_24h(conv.last_inbound_at) {
            return Err(freeform_window_expired_error(payload, conv, "location"));
        }
        return Ok(SendMode::Location { loc: loc.clone() });
    }
    if explicit("contacts") || (type_hint.is_none() && payload.contacts.is_some()) {
        let list = payload.contacts.as_ref().ok_or_else(|| {
            ApiError::BadRequest("contacts requerido cuando type=contacts".into())
        })?;
        if list.is_empty() {
            return Err(ApiError::ValidationError {
                code: "contacts_empty".into(),
                field: "contacts".into(),
                message: "Debes agregar al menos un contacto.".into(),
            });
        }
        for (i, c) in list.iter().enumerate() {
            let fname = c
                .get("name")
                .and_then(|n| n.get("formatted_name"))
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim();
            if fname.is_empty() {
                return Err(ApiError::ValidationError {
                    code: "contact_name_required".into(),
                    field: format!("contacts[{}].name.formatted_name", i),
                    message: "Cada contacto necesita un nombre completo.".into(),
                });
            }
        }
        if !is_within_24h(conv.last_inbound_at) {
            return Err(freeform_window_expired_error(payload, conv, "contacts"));
        }
        return Ok(SendMode::Contacts { list: list.clone() });
    }

    let content = payload.content.as_deref().unwrap_or("").trim();
    if content.is_empty() {
        return Err(ApiError::BadRequest(
            "content requerido (o template para envíos fuera de 24h)".into(),
        ));
    }

    if !is_within_24h(conv.last_inbound_at) {
        return Err(freeform_window_expired_error(payload, conv, "text"));
    }

    Ok(SendMode::Text {
        content: content.to_string(),
    })
}

fn freeform_window_expired_error(
    payload: &SendMessageRequest,
    conv: &WaConversation,
    attempted_type: &str,
) -> ApiError {
    let declared_type = payload.msg_type.as_deref().unwrap_or("text");
    let has_template = payload.template.is_some();
    let (can_send_freeform, freeform_expires_at) = compute_freeform_state(conv.last_inbound_at);
    let last_inbound_at = conv.last_inbound_at.map(iso8601);
    let conversation_id = conv.id.as_ref().map(|id| id.to_hex());

    tracing::warn!(
        conversation_id = conversation_id.as_deref().unwrap_or("unknown"),
        status = %conv.status,
        attempted_type,
        declared_type,
        has_template,
        has_content = payload.content.as_deref().is_some_and(|s| !s.trim().is_empty()),
        last_inbound_at = last_inbound_at.as_deref().unwrap_or("none"),
        freeform_expires_at = freeform_expires_at.as_deref().unwrap_or("none"),
        "whatsapp freeform blocked by expired 24h window; payload did not activate template mode"
    );

    ApiError::Domain {
        status: StatusCode::CONFLICT,
        code: "window_expired".into(),
        field: Some("type".into()),
        message: "La ventana de 24h esta cerrada. Este payload llego como mensaje libre, no como plantilla. Para enviar una plantilla usa type=\"template\" y el objeto template.".into(),
        details: Some(serde_json::json!({
            "reason": "payload_not_template",
            "attempted_type": attempted_type,
            "received_type": declared_type,
            "has_template": has_template,
            "can_send_freeform": can_send_freeform,
            "last_inbound_at": last_inbound_at,
            "freeform_expires_at": freeform_expires_at,
            "retry_with": {
                "type": "template",
                "required_field": "template",
                "note": "No envies el texto renderizado como content cuando la ventana esta cerrada."
            }
        })),
    }
}

fn nonempty(opt: &Option<String>) -> Option<String> {
    opt.as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn validate_media_id(id: &str) -> Result<(), ApiError> {
    let t = id.trim();
    if t.is_empty() {
        return Err(ApiError::ValidationError {
            code: "media_id_required".into(),
            field: "media_id".into(),
            message: "Falta `media_id`. Subí el archivo primero con POST /whatsapp/media.".into(),
        });
    }
    Ok(())
}
