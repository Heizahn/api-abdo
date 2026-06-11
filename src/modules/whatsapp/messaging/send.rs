use std::sync::Arc;

use axum::extract::{Extension, Json, Path, State};
use mongodb::bson::{oid::ObjectId, DateTime};

use crate::modules::whatsapp::conversations::outbound::{
    auto_fill_template_header_media, map_template_send_error,
};
use crate::modules::whatsapp::service::WhatsAppService;
use crate::modules::whatsapp::shared;
use crate::modules::whatsapp::url_preview::spawn_preview_job;
use crate::modules::whatsapp::ws::{broadcast_all, WsServerEvent};
use crate::{
    auth::user_jwt::UserProfileClaims,
    db::ConversationTouch,
    db::WhatsAppRepository,
    error::ApiError,
    models::whatsapp::{
        LocationPayload, SendMessageData, SendMessageRequest, SendMessageResponse, WaConversation,
        WaMessage,
    },
    state::AppState,
};

use super::mode::{resolve_send_mode, SendMode};
use super::preview::{interactive_preview, template_preview};

pub(crate) struct TemplateFields {
    pub(crate) name: String,
    pub(crate) language: String,
    pub(crate) components: Option<serde_json::Value>,
}

/// POST /v1/auth-user/whatsapp/conversations/{id}/messages
/// Envía un mensaje al número de la conversación usando `SendMode` (texto,
/// template, media, interactivo, etc.).
#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/messages",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    request_body = SendMessageRequest,
    responses(
        (status = 200, description = "Mensaje enviado", body = SendMessageResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn send_message_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, ApiError> {
    let actor = shared::authz::require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let mut conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    require_workspace_agent_or_assigned(&state, &conv, &actor).await?;

    // Decidir modo: template (siempre permitido) vs texto (sólo dentro de la
    // ventana de 24h). El discriminador `type: "template"` o la presencia del
    // campo `template` activan el modo template.
    let mut mode = resolve_send_mode(&payload, &conv)?;

    // Conversación cerrada:
    // - Template → reopen + take atómico (asignar al caller), luego enviar.
    // - Cualquier otro tipo → rechazar con 409.
    if conv.status == "closed" {
        match &mode {
            SendMode::Template { .. } => {
                // Reabrir + asignar atómicamente al caller.
                let taken = state
                    .db
                    .take_conversation(&oid, &claims.id)
                    .await
                    .map_err(|e| ApiError::DatabaseError(e))?;
                let reopened_conv = match taken {
                    Some(c) => c,
                    None => return Err(ApiError::ConversationNotTakeable),
                };

                // CHAT_TOMADO antes de enviar el template a Meta.
                let taken_by_name =
                    shared::mappers::resolve_user_name_by_id(&state, &claims.id).await;
                let ev = WsServerEvent::ChatTomado {
                    conversation_id: id.clone(),
                    taken_by: claims.id.clone(),
                    taken_by_name,
                    status: reopened_conv.status.clone(),
                    previous_status: "closed".to_string(),
                };
                broadcast_all(&state.ws_registry, &ev).await;

                // Actualizar la copia local de `conv` para que el resto del handler
                // opere sobre el estado post-reopen (status = "in_progress").
                conv = reopened_conv;
            }
            _ => {
                return Err(ApiError::ClosedRequiresTemplate);
            }
        }
    }

    // Lookup idempotente (fuente de verdad: DB, por `(conv_id, idempotency_key)`).
    // - sent/delivered/read → devolver el mismo mensaje (no reenviar a Meta).
    // - failed               → reintentar envío, actualizar `wa_message_id` + status.
    // - None (sin status)    → devolver como está (estado intermedio, no reenviamos).
    if let Some(key) = payload.idempotency_key.as_deref() {
        if let Some(existing) = state
            .db
            .find_message_by_idempotency(&oid, key)
            .await
            .map_err(|e| ApiError::DatabaseError(e))?
        {
            let existing_id = existing.id;
            let existing_msg_type = existing.msg_type.clone();
            let existing_media_filename = existing.media_filename.clone();
            let is_failed = existing.status.as_deref() == Some("failed");

            if !is_failed {
                let name = existing.sent_by.as_deref().map(|_| claims.name.clone());
                let rto = shared::mappers::resolve_reply_to_for_one(&state, &existing).await;
                let item = shared::mappers::msg_to_item(existing, name, rto);
                return Ok(Json(SendMessageResponse {
                    ok: true,
                    data: SendMessageData {
                        message_id: item.id.clone(),
                        message: item,
                    },
                }));
            }

            // Retry: reenviar a Meta con la configuración del negocio y actualizar el doc.
            // Se reusa el `reply_to` original del mensaje para mantener la cita.
            let wa =
                shared::service::resolve_service_for_phone(&state, &conv.business_phone).await?;
            let retry_reply_to = existing.reply_to_wa_message_id.clone();
            let preview_url_flag = payload.preview_url.unwrap_or(false);
            // Auto-fill del HEADER si la plantilla tiene IMAGE/VIDEO y el front no lo mandó.
            if let SendMode::Template { tpl } = &mut mode {
                auto_fill_template_header_media(
                    &state,
                    &mut tpl.components,
                    &tpl.name,
                    &tpl.language,
                    &conv.business_phone,
                    &wa,
                )
                .await?;
            }
            let sent = dispatch_send(
                &mode,
                &wa,
                &conv.phone,
                retry_reply_to.as_deref(),
                preview_url_flag,
            )
            .await?;
            let new_wa_id = sent.wa_id.clone();
            let preview = sent.preview.clone();

            let msg_oid =
                existing_id.ok_or_else(|| ApiError::Internal("mensaje previo sin _id".into()))?;
            let updated = state
                .db
                .update_message_retry(&msg_oid, &new_wa_id, "sent")
                .await
                .map_err(|e| ApiError::DatabaseError(e))?
                .ok_or_else(|| {
                    ApiError::Internal("no se pudo actualizar mensaje tras reintento".into())
                })?;

            let touch = crate::db::ConversationTouch {
                preview: &preview,
                msg_type: &existing_msg_type,
                direction: "out",
                wa_message_id: &new_wa_id,
                from_user_id: Some(claims.id.as_str()),
                media_filename: existing_media_filename.as_deref(),
                status: Some("sent"),
                increment_unread: false,
                last_message_at: None,
            };
            state
                .db
                .touch_conversation(&oid, touch)
                .await
                .map_err(|e| ApiError::DatabaseError(e))?;

            let rto = shared::mappers::resolve_reply_to_for_one(&state, &updated).await;
            let item = shared::mappers::msg_to_item(updated, Some(claims.name.clone()), rto);

            // Broadcast del retry — status vuelve a "sent", el front actualiza la burbuja.
            let ev = WsServerEvent::MensajeNuevo {
                conversation_id: id.clone(),
                message: item.clone(),
            };
            broadcast_all(&state.ws_registry, &ev).await;

            return Ok(Json(SendMessageResponse {
                ok: true,
                data: SendMessageData {
                    message_id: item.id.clone(),
                    message: item,
                },
            }));
        }
    }

    // Envío nuevo.
    let wa = shared::service::resolve_service_for_phone(&state, &conv.business_phone).await?;

    let preview_url_flag = payload.preview_url.unwrap_or(false);
    // Auto-fill del HEADER si la plantilla tiene IMAGE/VIDEO y el front no lo mandó.
    if let SendMode::Template { tpl } = &mut mode {
        auto_fill_template_header_media(
            &state,
            &mut tpl.components,
            &tpl.name,
            &tpl.language,
            &conv.business_phone,
            &wa,
        )
        .await?;
    }
    let sent = dispatch_send(
        &mode,
        &wa,
        &conv.phone,
        payload.reply_to.as_deref(),
        preview_url_flag,
    )
    .await?;

    let is_text_mode = matches!(mode, SendMode::Text { .. });
    let preview = sent.preview.clone();

    let msg = WaMessage {
        id: None,
        conversation_id: oid,
        wa_message_id: sent.wa_id,
        direction: "out".to_string(),
        msg_type: sent.msg_type.to_string(),
        body: sent.body,
        media_id: sent.media_id,
        media_mime_type: sent.media_mime_type,
        media_filename: sent.media_filename,
        status: Some("sent".to_string()),
        meta_error_code: None,
        meta_error_title: None,
        meta_error_message: None,
        meta_error_details: None,
        failed_at: None,
        sent_by: Some(claims.id.clone()),
        source: None,
        campaign_id: None,
        campaign_recipient_id: None,
        read_by_user_id: None,
        read_at: None,
        idempotency_key: payload.idempotency_key.clone(),
        reply_to_wa_message_id: payload.reply_to.clone(),
        is_forwarded: None,
        is_frequently_forwarded: None,
        url_preview: None,
        voice: false,
        template_name: sent.template_fields.as_ref().map(|f| f.name.clone()),
        template_language: sent.template_fields.as_ref().map(|f| f.language.clone()),
        template_components: sent.template_fields.and_then(|f| f.components),
        interactive_payload: sent.interactive_payload,
        contacts_payload: sent.contacts_payload,
        location: sent.location,
        reactions: vec![],
        raw_payload: None,
        ai_processed_at: None,
        timestamp: DateTime::now(),
    };

    let saved = state
        .db
        .save_message(msg)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let touch = ConversationTouch {
        preview: &preview,
        msg_type: &saved.msg_type,
        direction: "out",
        wa_message_id: &saved.wa_message_id,
        from_user_id: Some(claims.id.as_str()),
        media_filename: saved.media_filename.as_deref(),
        status: Some("sent"),
        increment_unread: false,
        last_message_at: None,
    };
    state
        .db
        .touch_conversation(&oid, touch)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    // Si el mensaje nació de un quick-reply guardado, bumpear el contador de uso.
    // Best-effort: no bloquea la respuesta ni la afecta si falla.
    if let Some(qr_id) = payload.quick_reply_id.as_deref() {
        if let Ok(qr_oid) = ObjectId::parse_str(qr_id) {
            let db = state.db.clone();
            tokio::spawn(async move {
                if let Err(e) = db.increment_quick_reply_use(&qr_oid).await {
                    tracing::warn!(
                        "[send_message] increment_quick_reply_use falló id={}: {}",
                        qr_oid,
                        e
                    );
                }
            });
        }
    }

    let rto = shared::mappers::resolve_reply_to_for_one(&state, &saved).await;
    let saved_oid = saved.id;
    let item = shared::mappers::msg_to_item(saved, Some(claims.name.clone()), rto);

    // Broadcast del mensaje outbound. El front deduplica contra `idempotency_key`
    // si ya recibió la respuesta HTTP.
    let ev = WsServerEvent::MensajeNuevo {
        conversation_id: id.clone(),
        message: item.clone(),
    };
    broadcast_all(&state.ws_registry, &ev).await;

    // URL preview sólo para texto: los templates no llevan URLs que el
    // usuario pueda escribir de forma libre.
    if is_text_mode {
        if let Some(msg_oid) = saved_oid {
            spawn_preview_job(state.clone(), msg_oid, oid, preview.clone());
        }
    }

    Ok(Json(SendMessageResponse {
        ok: true,
        data: SendMessageData {
            message_id: item.id.clone(),
            message: item,
        },
    }))
}

async fn require_workspace_agent_or_assigned(
    state: &Arc<AppState>,
    conv: &WaConversation,
    actor: &crate::models::users::User,
) -> Result<(), ApiError> {
    if conv.assigned_to.as_deref() == Some(&actor.id) {
        return Ok(());
    }

    shared::authz::require_workspace_actor_for_conversation(state, actor, &conv.business_phone)
        .await
        .map(|_| ())
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
