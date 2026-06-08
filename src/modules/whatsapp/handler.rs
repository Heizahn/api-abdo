use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Extension, Json,
};
use mongodb::bson::{oid::ObjectId, DateTime};
use std::sync::Arc;

use crate::{
    auth::user_jwt::UserProfileClaims,
    crypto::aes::decrypt_payload,
    db::{WaTemplateRepository, WaTemplateUpdatePatch, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::*,
    state::AppState,
};

use super::assignment::assign_conversation;
use super::messaging::download::{prefetch_media, should_prefetch_media};
use super::messaging::media::swap_header_handles_in_components;
use super::service::WhatsAppService;
use super::settings::validation::normalize_to_e164;
use super::shared;
use super::shared::settings_secret;
use super::templates::handlers::{template_not_found, to_template_item};
use super::webhook::handler::{last_payload_store, verify_meta_signature};
use super::webhook::media_failures::schedule_inbound_media_failure_fallback;
use super::webhook::status::{
    has_meta_throttle_131049, is_inbound_media_failure_status, log_webhook_top_level_errors,
    InboundMediaFailureDetails,
};
use super::ws::{
    broadcast_all, broadcast_to_chat_users, build_template_updated_event,
    emit_to_phone_number_agents, ConversacionNoLeidaData, WsServerEvent,
};

use super::webhook::normalize::{
    build_top_level_delta_message, describe_top_level_group, extract_inbound_content,
    extract_inbound_delta_target_wa_id, inbound_payload_markers, inbound_raw_payload,
    infer_inbound_effective_type, should_apply_message_delta_update, InboundNormalizedContent,
};

/// Cooldown que aplica el back cuando Meta rebota con error 131049
/// (engagement throttle). Mientras `now < meta_throttle_until` toda la
/// conversación queda bloqueada para envíos. Valor empírico — 6h es
/// suficiente para cubrir la ventana típica del rate limit de Meta sin
/// quedarse pegado en perpetuidad si el inbound del cliente nunca llega.
const META_THROTTLE_COOLDOWN_MS: i64 = 6 * 60 * 60 * 1000;

async fn apply_inbound_message_delta_update(
    state: &Arc<AppState>,
    msg: &InboundMessage,
    effective_msg_type: &str,
) -> bool {
    if !should_apply_message_delta_update(effective_msg_type) {
        return false;
    }

    let target_wa_id = match extract_inbound_delta_target_wa_id(msg) {
        Some(id) => id,
        None => {
            tracing::warn!(
                "[webhook] {} sin context.id target; se tratará como inbound normal",
                effective_msg_type
            );
            return false;
        }
    };

    let target_ids = vec![target_wa_id.to_string()];
    let messages = match state.db.find_messages_by_wa_ids(&target_ids).await {
        Ok(messages) => messages,
        Err(e) => {
            tracing::error!(
                "[webhook] error consultando mensaje objetivo {}: {}",
                target_wa_id,
                e
            );
            return false;
        }
    };

    if !messages.contains_key(target_wa_id) {
        tracing::debug!(
            "[webhook] {} apuntando a mensaje no encontrado: {}. Se procesará como nuevo inbound",
            effective_msg_type,
            target_wa_id
        );
        return false;
    }

    let normalized = extract_inbound_content(msg, effective_msg_type);
    let new_body = normalized.body.unwrap_or_else(|| {
        if effective_msg_type == "revoke" {
            "Mensaje revocado".to_string()
        } else {
            "Mensaje editado".to_string()
        }
    });

    let raw_payload = inbound_raw_payload(msg, effective_msg_type);

    match state
        .db
        .update_message_body_by_wa_id(
            target_wa_id,
            &new_body,
            raw_payload.as_ref(),
            effective_msg_type,
        )
        .await
    {
        Ok(Some(updated)) => {
            match state
                .db
                .update_conversation_preview_if_last(
                    &updated.conversation_id,
                    &updated.wa_message_id,
                    new_body.as_str(),
                    &updated.msg_type,
                    &updated.direction,
                    updated.sent_by.as_deref(),
                    updated.media_filename.as_deref(),
                    updated.status.as_deref(),
                    updated.timestamp,
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(
                        "[webhook] ignorando refresco de preview para delta de mensaje {}: ya no es último mensaje",
                        target_wa_id
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "[webhook] error refrescando preview para mensaje mutado {}: {}",
                        updated.wa_message_id,
                        e
                    );
                }
            }

            let updated_message_item = build_message_item(state, updated).await;

            let ev = WsServerEvent::MensajeModificado {
                conversation_id: updated_message_item.conversation_id.clone(),
                message: updated_message_item,
                change_type: effective_msg_type.to_string(),
            };
            broadcast_all(&state.ws_registry, &ev).await;

            tracing::info!(
                "[webhook] mensaje objetivo actualizado por {}: wa_id={}",
                effective_msg_type,
                target_wa_id
            );
            true
        }
        Ok(None) => {
            tracing::warn!(
                "[webhook] {} para objetivo inexistente tras requery: {}",
                effective_msg_type,
                target_wa_id
            );
            false
        }
        Err(e) => {
            tracing::error!(
                "[webhook] error actualizando mensaje objetivo {}: {}",
                target_wa_id,
                e
            );
            false
        }
    }
}

/// Persiste un evento de ciclo de vida de conversación. Best-effort:
/// si la inserción falla se loggea pero NO se propaga el error — la
/// auditoría no debe bloquear la respuesta HTTP del agente.
async fn record_conv_event(state: &AppState, input: WaConversationEventInput<'_>) {
    if let Err(e) = state.db.record_conversation_event(input).await {
        tracing::warn!("record_conversation_event failed: {}", e);
    }
}

// ============================================
// WEBHOOK (público)
// ============================================

// Note: `WebhookVerifyParams`, `verify_webhook`, `debug_last_webhook_handler`,
// `LAST_WEBHOOK_PAYLOAD`, `last_payload_store`, `verify_meta_signature`,
// `hex_decode`, and `hex_nibble` now live in `whatsapp::webhook::handler`.

/// POST /v1/webhook/whatsapp
/// Recibe notificaciones de Meta (mensajes entrantes + actualizaciones de estado).
/// Meta espera siempre HTTP 200 — cualquier otro código provoca reenvíos.
pub async fn receive_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    // Verificar firma HMAC-SHA256 si WHATSAPP_APP_SECRET está configurado
    if let Ok(app_secret) = std::env::var("WHATSAPP_APP_SECRET") {
        if !app_secret.is_empty() {
            let header_val = headers
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !verify_meta_signature(app_secret.as_bytes(), &body, header_val) {
                tracing::warn!("[webhook] firma inválida — request rechazada");
                return StatusCode::FORBIDDEN;
            }
        }
    }

    // Guardar payload crudo ANTES del parse tipado — así el debug funciona
    // incluso cuando el shape no matchea nuestros structs (Meta agrega/cambia
    // campos sin avisar; queremos verlos para poder ajustar).
    {
        let raw: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        *last_payload_store().lock().await = Some(raw);
    }

    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("[webhook] JSON inválido: {}", e);
            return StatusCode::OK;
        }
    };
    let entries = match payload.entry {
        Some(e) => e,
        None => return StatusCode::OK,
    };

    for entry in entries {
        let changes = match entry.changes {
            Some(c) => c,
            None => continue,
        };

        for change in changes {
            match change.field.as_deref() {
                Some("messages") => {}
                Some("message_template_status_update") => {
                    if let Some(value) = &change.value {
                        if let (Some(meta_id), Some(event)) =
                            (&value.message_template_id, &value.event)
                        {
                            let state_cl = state.clone();
                            let meta_id_cl = meta_id.clone();
                            let event_cl = event.clone();
                            let reason_cl = value.reason.clone();
                            tokio::spawn(async move {
                                process_template_status(
                                    &state_cl,
                                    &meta_id_cl,
                                    &event_cl,
                                    reason_cl.as_deref(),
                                )
                                .await;
                            });
                        }
                    }
                    continue;
                }
                Some("errors") => {
                    if let Some(value) = &change.value {
                        let count = log_webhook_top_level_errors(value);
                        if count > 0 {
                            tracing::debug!("[webhook] procesadas {} top-level error(s)", count);
                        }
                    }
                    continue;
                }
                _ => {
                    tracing::debug!("[webhook] campo desconocido ignorado: {:?}", change.field);
                    continue;
                }
            }
            let value = match change.value {
                Some(v) => v,
                None => continue,
            };

            // Procesar actualizaciones de estado (delivered / read / failed)
            if let Some(statuses) = value.statuses.as_ref() {
                for s in statuses {
                    if s.status == "failed" {
                        if let Some(errs) = s.errors.as_ref() {
                            for e in errs {
                                tracing::warn!(
                                    "[webhook] mensaje {} falló: code={:?} title={:?} message={:?}",
                                    s.id,
                                    e.code,
                                    e.title,
                                    e.message
                                );
                            }
                        } else {
                            tracing::warn!("[webhook] mensaje {} falló sin detalles", s.id);
                        }
                    }
                    let first_error = s.errors.as_ref().and_then(|errs| errs.first());
                    match state
                        .db
                        .update_message_status(&s.id, &s.status, first_error)
                        .await
                    {
                        Ok(Some(updated)) => {
                            // Si este mensaje era el último de la conversación, propagar el
                            // nuevo status al preview del listado (checkmarks en vivo).
                            match state
                                .db
                                .update_conversation_status_if_last(
                                    &updated.conversation_id,
                                    &updated.wa_message_id,
                                    &s.status,
                                )
                                .await
                            {
                                Ok(true) => tracing::debug!(
                                    "[webhook] last_message_status={} propagado a conv {}",
                                    s.status,
                                    updated.conversation_id.to_hex()
                                ),
                                Ok(false) => {} // no era el último — sin propagar
                                Err(e) => tracing::warn!(
                                    "[webhook] update_conversation_status_if_last error: {}",
                                    e
                                ),
                            }

                            let event = WsServerEvent::MensajeActualizado {
                                conversation_id: updated.conversation_id.to_hex(),
                                message_id: updated.wa_message_id.clone(),
                                status: s.status.clone(),
                                meta_error_code: updated.meta_error_code,
                                meta_error_title: updated.meta_error_title.clone(),
                                meta_error_message: updated.meta_error_message.clone(),
                                meta_error_details: updated.meta_error_details.clone(),
                                failed_at: updated.failed_at.map(iso8601),
                            };
                            // sent/delivered/read son routine — DEBUG. failed es
                            // accionable y queda en WARN para que no se pierda.
                            if s.status == "failed" {
                                tracing::warn!(
                                    "[webhook] status failed → broadcast (wa_id={}, conv={})",
                                    updated.wa_message_id,
                                    updated.conversation_id.to_hex()
                                );
                            } else {
                                tracing::debug!(
                                    "[webhook] status {} → broadcast (wa_id={}, conv={})",
                                    s.status,
                                    updated.wa_message_id,
                                    updated.conversation_id.to_hex()
                                );
                            }
                            broadcast_all(&state.ws_registry, &event).await;

                            // 131049 — engagement throttle de Meta. Setea cooldown
                            // en la conversación para que el siguiente envío sea
                            // bloqueado en el back y el front pueda mostrarlo.
                            // El cooldown se libera al recibir un inbound (ver
                            // `update_last_inbound_at`) o al expirar `until`.
                            let has_131049 = has_meta_throttle_131049(s.errors.as_deref());
                            if s.status == "failed" && has_131049 {
                                let until = DateTime::from_millis(
                                    DateTime::now().timestamp_millis() + META_THROTTLE_COOLDOWN_MS,
                                );
                                if let Err(e) = state
                                    .db
                                    .set_meta_throttle_until(&updated.conversation_id, until)
                                    .await
                                {
                                    tracing::warn!(
                                        "[webhook] set_meta_throttle_until error (conv={}): {}",
                                        updated.conversation_id.to_hex(),
                                        e
                                    );
                                } else {
                                    tracing::warn!(
                                        "[webhook] meta_throttle_until seteado por 131049 (conv={}, until={})",
                                        updated.conversation_id.to_hex(), iso8601(until)
                                    );
                                    let conv_now = state
                                        .db
                                        .find_conversation_by_id(&updated.conversation_id)
                                        .await
                                        .ok()
                                        .flatten();
                                    let (can_send_freeform, freeform_expires_at) =
                                        compute_freeform_state(
                                            conv_now.as_ref().and_then(|c| c.last_inbound_at),
                                        );
                                    let last_inbound_iso = conv_now
                                        .as_ref()
                                        .and_then(|c| c.last_inbound_at)
                                        .map(iso8601);
                                    let estado_ev = WsServerEvent::ConversacionEstado {
                                        conversation_id: updated.conversation_id.to_hex(),
                                        last_inbound_at: last_inbound_iso,
                                        can_send_freeform,
                                        freeform_expires_at,
                                        meta_throttled: true,
                                        meta_throttle_until: Some(iso8601(until)),
                                    };
                                    broadcast_all(&state.ws_registry, &estado_ev).await;
                                }
                            }
                        }
                        Ok(None) => {
                            // Status update para un mensaje sin doc en DB. Caso común:
                            // Meta no pudo procesar la media inbound del cliente (131052
                            // "Media download error", 131053 "Media upload error", 131056
                            // "(Recoverable) Failure"). Sin doc en DB no podemos
                            // marcar nada, pero PODEMOS avisarle al cliente que
                            // reenvíe — sino queda esperando respuesta de un archivo
                            // que nunca llegó al sistema.
                            let is_media_failure = is_inbound_media_failure_status(
                                s.status.as_str(),
                                s.errors.as_deref(),
                            );
                            if is_media_failure {
                                let recipient = s
                                    .recipient_id
                                    .as_deref()
                                    .map(str::to_string)
                                    .unwrap_or_default();
                                let business_phone = value
                                    .metadata
                                    .as_ref()
                                    .and_then(|m| m.display_phone_number.as_deref())
                                    .map(normalize_to_e164)
                                    .unwrap_or_default();
                                tracing::warn!(
                                    "[webhook] inbound media failed (Meta no pudo procesar): wa_id={} recipient='{}' business='{}' errors={:?}",
                                    s.id, recipient, business_phone, s.errors
                                );
                                if !recipient.is_empty() && !business_phone.is_empty() {
                                    let wa_id = s.id.clone();
                                    let state_cl = state.clone();
                                    let failure_details = first_error
                                        .map(InboundMediaFailureDetails::from_status_error);
                                    tokio::spawn(async move {
                                        schedule_inbound_media_failure_fallback(
                                            &state_cl,
                                            &wa_id,
                                            &recipient,
                                            &business_phone,
                                            failure_details,
                                        )
                                        .await;
                                    });
                                }
                            } else {
                                tracing::debug!(
                                    "[webhook] status {} para wa_id={} sin doc en DB (ignorado)",
                                    s.status,
                                    s.id
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "[webhook] update_message_status error (wa_id={}, status={}): {}",
                                s.id,
                                s.status,
                                e
                            );
                        }
                    }
                }
            }

            // Metadatos top-level de edición/revocación de mensajes (sin `messages`).
            // Se mantienen en modo tolerante: si no podemos resolver target, se loggea
            // y se procesa como inbound normal (que aquí no existe).
            if value.messages.is_none() {
                if let Some(summary) = describe_top_level_group(&value) {
                    tracing::warn!(
                        "[webhook] evento top-level de grupo omitido sin soporte directo: {}",
                        summary
                    );
                }

                let has_top_level_edit_or_revoke = value.edit.is_some() || value.revoke.is_some();
                if has_top_level_edit_or_revoke {
                    let business_phone_raw = value
                        .metadata
                        .as_ref()
                        .and_then(|m| m.display_phone_number.as_deref())
                        .unwrap_or("")
                        .to_string();
                    let business_phone = normalize_to_e164(&business_phone_raw);

                    match state.db.find_wa_settings_by_phone(&business_phone).await {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            tracing::debug!(
                                "[webhook] número de negocio no configurado o inactivo: raw={} norm={}",
                                business_phone_raw,
                                business_phone
                            );
                            continue;
                        }
                        Err(e) => {
                            tracing::error!("[webhook] error buscando wa_settings: {}", e);
                            continue;
                        }
                    }
                }

                for delta_kind in ["edit", "revoke"] {
                    if let Some(msg) = build_top_level_delta_message(&value, delta_kind) {
                        let effective_msg_type = infer_inbound_effective_type(&msg);
                        if should_apply_message_delta_update(&effective_msg_type)
                            && apply_inbound_message_delta_update(&state, &msg, &effective_msg_type)
                                .await
                        {
                            continue;
                        }
                    }
                }
            }

            // Procesar mensajes entrantes
            if let Some(messages) = value.messages {
                let contacts = value.contacts.unwrap_or_default();

                // El número del negocio que recibió el mensaje (normalizado a E.164 sin "+")
                let business_phone_raw = value
                    .metadata
                    .as_ref()
                    .and_then(|m| m.display_phone_number.clone())
                    .unwrap_or_default();
                let business_phone = normalize_to_e164(&business_phone_raw);

                // find_wa_settings_by_phone ya filtra por active: true
                let settings = match state.db.find_wa_settings_by_phone(&business_phone).await {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        tracing::debug!(
                            "[webhook] número de negocio no configurado o inactivo: raw={} norm={}",
                            business_phone_raw,
                            business_phone
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::error!("[webhook] error buscando wa_settings: {}", e);
                        continue;
                    }
                };

                for msg in messages {
                    let effective_msg_type = infer_inbound_effective_type(&msg);
                    if effective_msg_type != msg.msg_type {
                        tracing::warn!(
                            "[webhook] tipo inbound ajustado wa_id={} from={} original={} effective={} payload_keys={}",
                            msg.id,
                            msg.from,
                            msg.msg_type,
                            effective_msg_type,
                            inbound_payload_markers(&msg)
                        );
                    } else if msg.msg_type == "unsupported" {
                        tracing::warn!(
                            "[webhook] unsupported inbound wa_id={} from={} payload_keys={}",
                            msg.id,
                            msg.from,
                            inbound_payload_markers(&msg)
                        );
                    }

                    // === REACCIONES — early return, no toca conversación ni persistencia ===
                    if effective_msg_type == "reaction"
                        && super::messaging::reactions::handle_inbound_reaction(&state, &msg).await
                    {
                        continue; // CRÍTICO: no caer en el resto del loop (no upsert, no touch, no insert).
                    }

                    // Edits/revoke: intentan mutar mensaje previo con mismo `context.id`.
                    if should_apply_message_delta_update(&effective_msg_type)
                        && apply_inbound_message_delta_update(&state, &msg, &effective_msg_type)
                            .await
                    {
                        // Si el objetivo existe y se actualizó, no crear mensaje nuevo.
                        continue;
                    }

                    let name = contacts
                        .iter()
                        .find(|c| c.wa_id.as_deref() == Some(&msg.from))
                        .and_then(|c| c.profile.as_ref())
                        .and_then(|p| p.name.clone());

                    // Upsert conversación (clave compuesta: contacto + número de negocio)
                    let (conv, conv_created) = match state
                        .db
                        .upsert_conversation(&msg.from, &business_phone, name)
                        .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::error!("upsert_conversation error: {}", e);
                            continue;
                        }
                    };
                    let conv_id = match conv.id {
                        Some(id) => id,
                        None => continue,
                    };

                    // Conversación nueva → registrar `created` (actor=None: lo
                    // disparó un inbound, no un agente humano).
                    if conv_created {
                        record_conv_event(
                            &state,
                            WaConversationEventInput {
                                conversation_id: &conv_id,
                                business_phone: &conv.business_phone,
                                event_type: "created",
                                actor_id: None,
                                actor_name: None,
                                target_id: None,
                                target_name: None,
                                note: Some("inbound"),
                            },
                        )
                        .await;
                    }

                    // Si la conversación estaba cerrada, reabrirla en pending (sin dueño).
                    // El auto-assign de abajo la reasignará al agente con menos carga.
                    let was_reopened = if conv.status == "closed" {
                        match state.db.reopen_conversation(&conv_id).await {
                            Ok(changed) => changed,
                            Err(e) => {
                                tracing::warn!("reopen_conversation error: {}", e);
                                false
                            }
                        }
                    } else {
                        false
                    };

                    if was_reopened {
                        record_conv_event(
                            &state,
                            WaConversationEventInput {
                                conversation_id: &conv_id,
                                business_phone: &conv.business_phone,
                                event_type: "reopened",
                                actor_id: None,
                                actor_name: None,
                                target_id: None,
                                target_name: None,
                                note: Some("inbound"),
                            },
                        )
                        .await;
                    }

                    let InboundNormalizedContent {
                        body,
                        media_id,
                        media_mime_type,
                        media_filename,
                        interactive_payload,
                        contacts_payload,
                        location_payload,
                        voice,
                    } = extract_inbound_content(&msg, effective_msg_type.as_str());

                    let preview = body
                        .clone()
                        .unwrap_or_else(|| format!("[{}]", effective_msg_type));

                    tracing::debug!(
                        "[webhook] guardando mensaje de cliente registrado: {} | tipo: {} | preview: {}",
                        msg.from, effective_msg_type, preview
                    );

                    // Timestamp real desde Meta (Unix seconds en string), fallback a ahora.
                    let msg_ts = msg
                        .timestamp
                        .as_deref()
                        .and_then(parse_unix_seconds_to_bson)
                        .unwrap_or_else(DateTime::now);

                    let wa_msg = WaMessage {
                        id: None,
                        conversation_id: conv_id,
                        wa_message_id: msg.id.clone(),
                        direction: "in".to_string(),
                        msg_type: effective_msg_type.clone(),
                        body,
                        media_id,
                        media_mime_type,
                        media_filename,
                        status: None,
                        meta_error_code: None,
                        meta_error_title: None,
                        meta_error_message: None,
                        meta_error_details: None,
                        failed_at: None,
                        sent_by: None,
                        read_by_user_id: None,
                        read_at: None,
                        idempotency_key: None,
                        reply_to_wa_message_id: msg.context.as_ref().map(|c| c.id.clone()),
                        url_preview: None,
                        voice,
                        template_name: None,
                        template_language: None,
                        template_components: None,
                        interactive_payload,
                        contacts_payload,
                        location: location_payload,
                        reactions: vec![],
                        raw_payload: inbound_raw_payload(&msg, &effective_msg_type),
                        ai_processed_at: None,
                        timestamp: msg_ts,
                    };

                    let saved = match state.db.save_message(wa_msg).await {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::error!("save_message error: {}", e);
                            continue;
                        }
                    };

                    // Prefetch del binario: el agente casi siempre abre el
                    // media a los pocos segundos. Si ya está en Redis cuando
                    // hace el GET, responde en ms en vez de 2 viajes a Meta.
                    if let Some(ref mid) = saved.media_id {
                        if should_prefetch_media(&saved.msg_type) {
                            let state_cl = state.clone();
                            let phone_cl = conv.business_phone.clone();
                            let mid_cl = mid.clone();
                            tokio::spawn(async move {
                                prefetch_media(state_cl, phone_cl, mid_cl).await;
                            });
                        }
                    }

                    // Capture unread count before touch so we can tell if
                    // this message pushes a clean conversation into unread.
                    let pre_touch_unread = conv.unread_count;

                    let touch = crate::db::ConversationTouch {
                        preview: &preview,
                        msg_type: &effective_msg_type,
                        direction: "in",
                        wa_message_id: &msg.id,
                        from_user_id: None,
                        media_filename: saved.media_filename.as_deref(),
                        status: None,
                        increment_unread: true,
                        last_message_at: Some(msg_ts),
                    };
                    if let Err(e) = state.db.touch_conversation(&conv_id, touch).await {
                        tracing::warn!("touch_conversation error: {}", e);
                    }

                    // EMIT BADGE: CONVERSACION_NO_LEIDA
                    // Design accepts always-emit on increment_unread (pending_total is authoritative).
                    // Pre-touch_unread is captured above; always emit — front uses pending_total as truth.
                    let _ = pre_touch_unread; // retained for documentation; always emit
                    let unread_pending = state.db.count_unread_conversations().await.unwrap_or(0);
                    let unread_ev = WsServerEvent::ConversacionNoLeida {
                        data: ConversacionNoLeidaData {
                            pending_total: unread_pending,
                            conversation_id: conv_id.to_hex(),
                            delta: 1,
                        },
                    };
                    if let Ok(badge_payload) = serde_json::to_string(&unread_ev) {
                        let _ = broadcast_to_chat_users(&state, badge_payload).await;
                    }

                    // Actualizar `last_inbound_at` → reabre la ventana de 24h.
                    if let Err(e) = state.db.update_last_inbound_at(&conv_id, msg_ts).await {
                        tracing::warn!("update_last_inbound_at error: {}", e);
                    }

                    // Releer conversación para emitir estado actualizado (unread_count, last_*).
                    let conv_now = state
                        .db
                        .find_conversation_by_id(&conv_id)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(conv);

                    // Si la conversación es nueva, avisar al front antes del mensaje.
                    if conv_created {
                        let ws_name =
                            Some(settings.workspace_name.clone()).filter(|w| !w.is_empty());
                        let resolved = resolve_customer_name(&state, &conv_now).await;
                        // Conv recién creada → assigned_to siempre null acá.
                        let new_ev = WsServerEvent::ConversacionNueva {
                            conversation: conv_to_item(
                                conv_now.clone(),
                                false,
                                None,
                                ws_name,
                                resolved,
                                None,
                                None,
                            ),
                        };
                        broadcast_all(&state.ws_registry, &new_ev).await;
                    } else if was_reopened {
                        // Cerrada → pending: el front debe re-integrarla en la bandeja activa.
                        let reopened_ev = WsServerEvent::ChatEstadoCambio {
                            conversation_id: conv_id.to_hex(),
                            new_status: "pending".to_string(),
                        };
                        broadcast_all(&state.ws_registry, &reopened_ev).await;
                    }

                    // MENSAJE_NUEVO a todos los conectados; el front filtra por conversación abierta.
                    let reply_to = resolve_reply_to_for_one(&state, &saved).await;
                    let saved_oid = saved.id;
                    let preview_text = saved.body.clone();
                    // Clon para el dispatch IA (corre en tokio::spawn más abajo).
                    let saved_for_dispatch = saved.clone();
                    let message_item = msg_to_item(saved, None, reply_to);
                    let agent_count = state.ws_registry.read().await.len();
                    tracing::debug!(
                        "[webhook] broadcast MENSAJE_NUEVO wa_id={} conv={} → {} agente(s) conectados",
                        message_item.wa_message_id, conv_id.to_hex(), agent_count
                    );
                    let msg_ev = WsServerEvent::MensajeNuevo {
                        conversation_id: conv_id.to_hex(),
                        message: message_item,
                    };
                    broadcast_all(&state.ws_registry, &msg_ev).await;

                    // Ventana de 24h: el inbound reabre la ventana. Emitimos el
                    // evento siempre para que los countdowns del front se
                    // re-sincronicen con el nuevo `freeform_expires_at`.
                    // El inbound también libera cualquier engagement throttle
                    // (131049) activo (lo limpia `update_last_inbound_at`).
                    let (can_send_freeform, freeform_expires_at) =
                        compute_freeform_state(Some(msg_ts));
                    let estado_ev = WsServerEvent::ConversacionEstado {
                        conversation_id: conv_id.to_hex(),
                        last_inbound_at: Some(iso8601(msg_ts)),
                        can_send_freeform,
                        freeform_expires_at,
                        meta_throttled: false,
                        meta_throttle_until: None,
                    };
                    broadcast_all(&state.ws_registry, &estado_ev).await;

                    // URL preview: fire-and-forget. Si el cuerpo trae una URL,
                    // el job fetchea OG tags y emite URL_PREVIEW_READY cuando termina.
                    if let (Some(msg_oid), Some(text)) = (saved_oid, preview_text) {
                        super::url_preview::spawn_preview_job(
                            state.clone(),
                            msg_oid,
                            conv_id,
                            text,
                        );
                    }

                    // Auto-asignación: solo si sigue pending sin dueño.
                    if conv_now.assigned_to.is_none() {
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            assign_conversation(state_clone, conv_id).await;
                        });
                    }

                    // Dispatch IA (shadow/live). Corre en background — si hay
                    // agente activo para este workspace, procesa el turno y
                    // persiste `AiInteraction`. En shadow loguea qué habría
                    // contestado; en live envía la respuesta vía Meta y
                    // emite `MENSAJE_NUEVO` para los agentes humanos.
                    let should_dispatch_ai =
                        !matches!(effective_msg_type.as_str(), "edit" | "revoke" | "group");
                    if should_dispatch_ai {
                        if let Some(ws_id) = settings.id {
                            crate::modules::ai_agent::dispatch::dispatch_inbound_async(
                                state.clone(),
                                saved_for_dispatch,
                                ws_id,
                            );
                        }
                    }
                }
            }
        }
    }

    StatusCode::OK
}

// ============================================
// ENDPOINTS DE STAFF/ADMIN (user JWT)
// ============================================

// moved to `messaging::send`

// moved to `messaging::mode`

// ============================================
// MEDIA (descarga proxy)
// ============================================

/// Proxy de descarga para media subido por el cliente. El binario real vive en la
/// CDN de Meta y sólo es accesible con el access token del negocio — por eso la
/// ruta pasa por el backend en vez de entregar la URL directa al front.
///
/// Autorización: el agente debe estar en `WaSettings.agents` del `business_phone`
/// de la conversación a la que pertenece el media.
// ============================================
// QUICK REPLIES (mensajes rápidos)
// ============================================

#[allow(unused_imports)]
pub use super::quick_replies::handlers::{
    create_quick_reply_handler, delete_quick_reply_handler, duplicate_quick_reply_handler,
    list_quick_replies_handler, set_quick_reply_active_handler, update_quick_reply_handler,
    QuickRepliesQuery,
};

#[allow(unused_imports)]
pub use super::templates::handlers::{
    create_template_handler, delete_template_handler, get_template_handler, list_templates_handler,
    resync_template_handler, TemplatesListQuery,
};

// ============================================
// HELPERS INTERNOS
// ============================================
// ============================================
// HELPERS DE MAPEO
// ============================================

#[allow(dead_code)]
fn iso8601(dt: DateTime) -> String {
    dt.try_to_rfc3339_string().unwrap_or_default()
}

fn conv_to_item(
    c: WaConversation,
    include_client_id: bool,
    last_opened_at: Option<DateTime>,
    workspace_name: Option<String>,
    resolved_name: Option<String>,
    last_message_from_user_name: Option<String>,
    assigned_to_name: Option<String>,
) -> ConversationItem {
    shared::response::conv_to_item(
        c,
        include_client_id,
        last_opened_at,
        workspace_name,
        resolved_name,
        last_message_from_user_name,
        assigned_to_name,
    )
}

/// Devuelve `(meta_throttled, meta_throttle_until_iso)`. Si el cooldown ya
/// expiró, devuelve `(false, None)` — un campo seteado en el pasado no debe
/// confundir al front.
#[allow(dead_code)]
fn compute_meta_throttle_state(until: Option<DateTime>) -> (bool, Option<String>) {
    shared::response::compute_meta_throttle_state(until)
}

/// Resuelve el nombre del contacto para una conversación contra `Clients`:
/// si tiene `client_id` linkeado lo usa; si no, intenta por teléfono. Devuelve
/// `None` cuando no matchea en DB — el caller cae a `WaConversation.name`.
async fn resolve_customer_name(state: &Arc<AppState>, conv: &WaConversation) -> Option<String> {
    shared::mappers::resolve_customer_name(state, conv).await
}

/// Resuelve el nombre del agente que envió el último mensaje.
async fn resolve_last_message_agent_name_one(
    state: &Arc<AppState>,
    conv: &WaConversation,
) -> Option<String> {
    shared::mappers::resolve_last_message_agent_name_one(state, conv).await
}

/// Resuelve el nombre del agente actualmente asignado.
async fn resolve_assigned_agent_name_one(
    state: &Arc<AppState>,
    conv: &WaConversation,
) -> Option<String> {
    shared::mappers::resolve_assigned_agent_name_one(state, conv).await
}

/// Devuelve `(can_send_freeform, freeform_expires_at_iso)`.
fn compute_freeform_state(last_inbound_at: Option<DateTime>) -> (bool, Option<String>) {
    shared::time::compute_freeform_state(last_inbound_at)
}

/// Atajo para handlers que tocan una sola conversación: resuelve `workspace_name`
/// por su `business_phone` vía `WaSettings`.
async fn resolve_workspace_name(state: &Arc<AppState>, business_phone: &str) -> Option<String> {
    shared::workspace::resolve_workspace_name(state, business_phone).await
}

fn msg_to_item(
    m: WaMessage,
    from_user_name: Option<String>,
    reply_to: Option<ReplyToItem>,
) -> MessageItem {
    shared::mappers::msg_to_item(m, from_user_name, reply_to)
}

/// Atajo usado por jobs async (`url_preview`, `ai_agent::dispatch`) para armar
/// un `MessageItem` completo a partir de un `WaMessage` recién releído:
/// resuelve `sent_by_name` y `reply_to` en un solo call. Costo: 1-2 queries
/// a `Users` / `WaMessages`.
pub async fn build_message_item(state: &Arc<AppState>, m: WaMessage) -> MessageItem {
    shared::mappers::build_message_item(state, m).await
}

/// Trunca el cuerpo del mensaje citado a ~80 chars (seguro en UTF-8).
/// Se usa sólo para preview en la UI; el mensaje original completo sigue
/// disponible por su `wa_message_id`.
#[allow(dead_code)]
fn preview_truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// Atajo para un solo mensaje: reusa el helper batch y devuelve el `ReplyToItem`
/// correspondiente si existe.
async fn resolve_reply_to_for_one(state: &Arc<AppState>, m: &WaMessage) -> Option<ReplyToItem> {
    shared::mappers::resolve_reply_to_for_one(state, m).await
}

/// Convierte un timestamp de Meta (Unix seconds en string) a `bson::DateTime`.
fn parse_unix_seconds_to_bson(s: &str) -> Option<DateTime> {
    let secs: i64 = s.parse().ok()?;
    Some(DateTime::from_millis(secs.checked_mul(1000)?))
}

/// Construye un `ConversationItem` completo desde un `WaConversation` resolviendo
/// workspace_name + nombres en una sola pasada. Reusable desde otros módulos
/// del feature (tickets) sin tener que reexportar todos los helpers internos.
#[allow(dead_code)]
pub(super) async fn build_conversation_item(
    state: &Arc<AppState>,
    conv: WaConversation,
    caller_id: &str,
) -> Result<ConversationItem, ApiError> {
    let oid = conv.id.unwrap_or_default();
    let opens = state
        .db
        .get_conversation_opens(caller_id, &[oid])
        .await
        .map_err(ApiError::DatabaseError)?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(state, &conv.business_phone).await;
    let resolved = resolve_customer_name(state, &conv).await;
    let agent_name = resolve_last_message_agent_name_one(state, &conv).await;
    let assigned_name = resolve_assigned_agent_name_one(state, &conv).await;
    Ok(conv_to_item(
        conv,
        true,
        last_opened,
        workspace_name,
        resolved,
        agent_name,
        assigned_name,
    ))
}

// ============================================
// TEMPLATES — helper compartido
// ============================================

/// Cuenta placeholders únicos `{{1}}..{{N}}` en un string. Devuelve el máximo
/// índice encontrado (los placeholders en Meta son consecutivos).
fn count_placeholders(text: &str) -> u32 {
    let bytes = text.as_bytes();
    let mut max_idx: u32 = 0;
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let mut j = i + 2;
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start && j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                if let Ok(n) = std::str::from_utf8(&bytes[start..j])
                    .unwrap_or("")
                    .parse::<u32>()
                {
                    if n > max_idx {
                        max_idx = n;
                    }
                }
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    max_idx
}

/// Slugifica una cadena a formato Meta-safe:
/// lowercase, non-alnum → `_`, strip non-ASCII, colapsar `_` consecutivos,
/// trim trailing `_`, max 512 chars.
fn slugify(s: &str) -> String {
    // Eliminar caracteres no-ASCII (emojis, acentos, etc.)
    let ascii_only: String = s.chars().filter(|c| c.is_ascii()).collect();
    let lower = ascii_only.to_lowercase();
    // Reemplazar todo lo que no sea alphanumeric con `_`
    let replaced: String = lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    // Colapsar `_` consecutivos
    let mut collapsed = String::with_capacity(replaced.len());
    let mut prev_underscore = false;
    for c in replaced.chars() {
        if c == '_' {
            if !prev_underscore {
                collapsed.push(c);
            }
            prev_underscore = true;
        } else {
            collapsed.push(c);
            prev_underscore = false;
        }
    }
    // Trim trailing `_`
    let trimmed = collapsed.trim_end_matches('_');
    // Truncar a 512 chars
    if trimmed.len() > 512 {
        &trimmed[..512]
    } else {
        trimmed
    }
    .to_string()
}

/// Genera el `name` Meta a partir del `name_input` y el flag `is_system`.
pub(in crate::modules::whatsapp) fn generate_template_name(
    name_input: &str,
    is_system: bool,
) -> String {
    let slug = slugify(name_input);
    if is_system {
        let today = chrono::Utc::now().format("%Y%m%d").to_string();
        format!("sistema_abdo_{}_{}", slug, today)
    } else {
        slug
    }
}

/// Valida los componentes del template. Devuelve el `body_placeholders` count.
/// Construye el array `components` que espera Meta a partir de los campos
/// flat del request del front (`header`, `body`, `body_samples`, `footer`,
/// `buttons`). Mapea 1:1 a la estructura oficial:
///
/// - `header` → `{ type: "HEADER", format, text?, example? }`
/// - `body`   → `{ type: "BODY", text, example?: { body_text: [[…samples]] } }`
/// - `footer` → `{ type: "FOOTER", text }` (omite si vacío)
/// - `buttons`→ `{ type: "BUTTONS", buttons: […] }` (omite si vacío)
pub(in crate::modules::whatsapp) fn flat_to_components(
    header: Option<&WaTemplateHeaderInput>,
    body: &str,
    body_samples: Option<&Vec<String>>,
    footer: Option<&str>,
    buttons: Option<&Vec<WaTemplateButtonInput>>,
) -> Vec<serde_json::Value> {
    let mut comps: Vec<serde_json::Value> = Vec::new();

    if let Some(h) = header {
        let mut comp = serde_json::json!({
            "type": "HEADER",
            "format": h.kind.to_uppercase(),
        });
        if let Some(t) = &h.text {
            comp["text"] = serde_json::json!(t);
        }
        if let Some(ex) = &h.example {
            comp["example"] = ex.clone();
        }
        comps.push(comp);
    }

    let mut body_comp = serde_json::json!({ "type": "BODY", "text": body });
    if let Some(samples) = body_samples {
        if !samples.is_empty() {
            // Meta espera body_text como array de arrays (un set de ejemplos
            // por cada juego de placeholders). Mandamos uno solo.
            body_comp["example"] = serde_json::json!({ "body_text": [samples] });
        }
    }
    comps.push(body_comp);

    if let Some(f) = footer {
        if !f.trim().is_empty() {
            comps.push(serde_json::json!({ "type": "FOOTER", "text": f }));
        }
    }

    if let Some(btns) = buttons {
        if !btns.is_empty() {
            let mut button_arr: Vec<serde_json::Value> = Vec::new();
            for b in btns {
                let mut bobj = serde_json::json!({
                    "type": b.kind.to_uppercase(),
                    "text": b.text,
                });
                if let Some(u) = &b.url {
                    bobj["url"] = serde_json::json!(u);
                }
                if let Some(p) = &b.phone_number {
                    bobj["phone_number"] = serde_json::json!(p);
                }
                if let Some(ex) = &b.example {
                    bobj["example"] = serde_json::json!(ex);
                }
                button_arr.push(bobj);
            }
            comps.push(serde_json::json!({ "type": "BUTTONS", "buttons": button_arr }));
        }
    }

    comps
}

pub(in crate::modules::whatsapp) fn validate_components(
    comps: &[serde_json::Value],
) -> Result<u32, ApiError> {
    let has_body = comps.iter().any(|c| {
        c.get("type")
            .and_then(|v| v.as_str())
            .map(|t| t.eq_ignore_ascii_case("BODY"))
            .unwrap_or(false)
    });
    if !has_body {
        return Err(ApiError::domain_with_details(
            StatusCode::BAD_REQUEST,
            "invalid_component",
            "Se requiere componente BODY",
            serde_json::json!({ "component_index": null, "reason": "body_required" }),
        ));
    }

    let mut body_placeholders: u32 = 0;

    for (idx, comp) in comps.iter().enumerate() {
        let comp_type = comp
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();

        match comp_type.as_str() {
            "BODY" => {
                let text = comp.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "BODY.text no puede estar vacío",
                        serde_json::json!({ "component_index": idx, "reason": "body_text_required" }),
                    ));
                }
                if text.len() > 1024 {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "BODY.text excede 1024 caracteres",
                        serde_json::json!({ "component_index": idx, "reason": "body_text_too_long" }),
                    ));
                }
                body_placeholders = count_placeholders(text);
            }
            "FOOTER" => {
                if let Some(text) = comp.get("text").and_then(|v| v.as_str()) {
                    if text.len() > 60 {
                        return Err(ApiError::domain_with_details(
                            StatusCode::BAD_REQUEST,
                            "invalid_component",
                            "FOOTER.text excede 60 caracteres",
                            serde_json::json!({ "component_index": idx, "reason": "footer_text_too_long" }),
                        ));
                    }
                }
            }
            "HEADER" => {
                let format = comp
                    .get("format")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_uppercase();
                let valid_formats = ["NONE", "TEXT", "IMAGE", "VIDEO", "DOCUMENT"];
                if !valid_formats.contains(&format.as_str()) {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        format!("HEADER.format inválido: {}", format),
                        serde_json::json!({ "component_index": idx, "reason": "header_format_invalid" }),
                    ));
                }
                if format == "TEXT" {
                    if let Some(text) = comp.get("text").and_then(|v| v.as_str()) {
                        if text.len() > 60 {
                            return Err(ApiError::domain_with_details(
                                StatusCode::BAD_REQUEST,
                                "invalid_component",
                                "HEADER.text excede 60 caracteres",
                                serde_json::json!({ "component_index": idx, "reason": "header_text_too_long" }),
                            ));
                        }
                    }
                }
            }
            "BUTTONS" => {
                let buttons = match comp.get("buttons").and_then(|v| v.as_array()) {
                    Some(b) => b,
                    None => continue,
                };
                // Recopilar tipos
                let types: Vec<String> = buttons
                    .iter()
                    .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
                    .map(|s| s.to_uppercase())
                    .collect();

                let all_qr = types.iter().all(|t| t == "QUICK_REPLY");
                let all_url = types.iter().all(|t| t == "URL");
                let all_phone = types.iter().all(|t| t == "PHONE_NUMBER");

                if !all_qr && !all_url && !all_phone {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "No se pueden mezclar tipos de botones",
                        serde_json::json!({ "component_index": idx, "reason": "buttons_mixed_types" }),
                    ));
                }
                if all_qr && buttons.len() > 3 {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "Máximo 3 botones QUICK_REPLY",
                        serde_json::json!({ "component_index": idx, "reason": "buttons_too_many" }),
                    ));
                }
                if (all_url || all_phone) && buttons.len() > 1 {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "Máximo 1 botón de tipo URL o PHONE_NUMBER",
                        serde_json::json!({ "component_index": idx, "reason": "buttons_too_many" }),
                    ));
                }
            }
            _ => {}
        }
    }

    Ok(body_placeholders)
}

/// Convierte un error de Meta (anyhow con posible downcast a `MetaApiError`)
/// en un `ApiError::Domain`. Si es 429, emite `meta_edit_rate_limited`.
pub(crate) fn map_meta_error(err: &anyhow::Error, default_msg: &str) -> ApiError {
    use super::service::MetaApiError;
    if let Some(me) = err.downcast_ref::<MetaApiError>() {
        if me.code == 429 {
            return ApiError::domain_with_details(
                StatusCode::TOO_MANY_REQUESTS,
                "meta_edit_rate_limited",
                "Meta limita las ediciones a 1 por día y 10 por mes. Intenta más tarde",
                serde_json::json!({}),
            );
        }
        let user_msg = me.error_user_msg.clone();
        return ApiError::domain_with_details(
            StatusCode::BAD_GATEWAY,
            "meta_rejected",
            default_msg,
            serde_json::json!({
                "meta_error_code": me.code.to_string(),
                "meta_error_message": me.message,
                "rejection_reason": user_msg,
            }),
        );
    }
    ApiError::domain_with_details(
        StatusCode::BAD_GATEWAY,
        "meta_rejected",
        default_msg,
        serde_json::json!({
            "meta_error_code": "0",
            "meta_error_message": err.to_string(),
            "rejection_reason": null,
        }),
    )
}

/// Exige `nRole == 0` (SUPERADMIN). Devuelve `403` si no se cumple.
pub(super) async fn require_superadmin(
    state: &Arc<AppState>,
    user_id: &str,
) -> Result<crate::models::users::User, ApiError> {
    shared::authz::require_superadmin(state, user_id).await
}

// ---------------------------------------------------------------------------
// PATCH /v1/auth-user/whatsapp/templates/:id
// ---------------------------------------------------------------------------

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/templates/{id}",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la plantilla")),
    request_body = UpdateWaTemplateRequest,
    responses(
        (status = 200, description = "Plantilla actualizada", body = WaTemplateResponse),
        (status = 400, description = "invalid_component"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "cannot_edit_approved o Sólo SUPERADMIN"),
        (status = 404, description = "template_not_found"),
        (status = 409, description = "cannot_edit_pending, name_already_exists"),
        (status = 429, description = "meta_edit_rate_limited"),
        (status = 502, description = "meta_rejected"),
    )
)]
pub async fn update_template_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(body): Json<UpdateWaTemplateRequest>,
) -> Result<Json<WaTemplateResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| template_not_found())?;

    // 1. Cargar doc
    let doc = state
        .db
        .find_template_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(template_not_found)?;

    let prev_status = doc.status;

    // 2. Construir new_components_opt desde los flat fields (header/body/footer/...).
    //    Si CUALQUIERA de esos fields viene en el payload, reconstruimos el
    //    array completo. En ese caso `body` es obligatorio (BODY siempre va en
    //    components según Meta).
    let any_flat_components = body.header.is_some()
        || body.body.is_some()
        || body.body_samples.is_some()
        || body.footer.is_some()
        || body.buttons.is_some();

    let new_components_opt: Option<Vec<serde_json::Value>> = if any_flat_components {
        let body_text = body.body.as_deref().ok_or_else(|| {
            ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "body_required",
                "body",
                "Para editar componentes (header/footer/buttons) debes incluir también el body",
            )
        })?;
        Some(flat_to_components(
            body.header.as_ref(),
            body_text,
            body.body_samples.as_ref(),
            body.footer.as_deref(),
            body.buttons.as_ref(),
        ))
    } else {
        None
    };

    // 3. Validar edit policy según status
    match prev_status {
        WaTemplateStatus::Pending | WaTemplateStatus::Paused | WaTemplateStatus::Disabled => {
            return Err(ApiError::domain_simple(
                StatusCode::CONFLICT,
                "cannot_edit_pending",
                "No se puede editar una plantilla en revisión",
            ));
        }
        WaTemplateStatus::Approved => {
            // Solo BODY editable. Verificar que no trae cambios prohibidos.
            let has_forbidden =
                body.name_input.is_some() || body.category.is_some() || body.is_system.is_some();
            if has_forbidden {
                return Err(ApiError::domain_simple(
                    StatusCode::FORBIDDEN,
                    "cannot_edit_approved",
                    "Solo el cuerpo es editable en plantillas aprobadas",
                ));
            }
            // Si hay components nuevos, validar que son solo BODY
            if let Some(ref new_comps) = new_components_opt {
                let has_non_body = new_comps.iter().any(|c| {
                    c.get("type")
                        .and_then(|v| v.as_str())
                        .map(|t| !t.eq_ignore_ascii_case("BODY"))
                        .unwrap_or(false)
                });
                if has_non_body {
                    return Err(ApiError::domain_simple(
                        StatusCode::FORBIDDEN,
                        "cannot_edit_approved",
                        "Solo el cuerpo es editable en plantillas aprobadas",
                    ));
                }
            }
        }
        WaTemplateStatus::Draft | WaTemplateStatus::Rejected => {}
    }

    // Acumular campos a actualizar
    let mut patch = WaTemplateUpdatePatch {
        name: None,
        display_name: None,
        name_input: None,
        category: body.category,
        components: None,
        body_placeholders: None,
        status: None,
        rejection_reason: None,
        meta_template_id: None,
        is_system: body.is_system,
        submit_to_meta: None,
    };

    // 4. Si cambia name_input (sólo Draft/Rejected): regenerar name + unicidad
    if let Some(ref new_name_input) = body.name_input {
        if new_name_input.trim().is_empty() {
            return Err(ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "name_required",
                "name_input",
                "El nombre es requerido",
            ));
        }
        let is_system = body.is_system.unwrap_or(doc.is_system);
        let new_name = generate_template_name(new_name_input, is_system);
        {
            let re = regex::Regex::new(r"^[a-z][a-z0-9_]{0,511}$").expect("regex válido");
            if !re.is_match(&new_name) {
                return Err(ApiError::domain_with_field(
                    StatusCode::BAD_REQUEST,
                    "name_invalid",
                    "name_input",
                    "Nombre inválido. Usa solo letras minúsculas, números y guión bajo (debe empezar con letra)",
                ));
            }
        }
        // Verificar unicidad si el nombre cambió
        if new_name != doc.name {
            let existing = state
                .db
                .find_template_by_phone_name_lang(&doc.phone_number_id, &new_name, &doc.language)
                .await
                .map_err(ApiError::DatabaseError)?;
            if existing.is_some() {
                return Err(ApiError::domain_with_field(
                    StatusCode::CONFLICT,
                    "name_already_exists",
                    "name_input",
                    "Ya existe una plantilla con ese nombre en este idioma",
                ));
            }
            patch.name = Some(new_name);
        }
        patch.display_name = Some(new_name_input.clone());
        patch.name_input = Some(new_name_input.clone());
    }

    // 5. Si submit_to_meta pasa de false a true (DRAFT → PENDING)
    if body.submit_to_meta == Some(true) && !doc.submit_to_meta {
        let settings = state
            .db
            .find_wa_settings_by_phone_number_id(&doc.phone_number_id)
            .await
            .map_err(ApiError::DatabaseError)?
            .ok_or_else(|| {
                ApiError::domain_with_field(
                    StatusCode::NOT_FOUND,
                    "phone_number_not_found",
                    "phone_number_id",
                    "El número de WhatsApp no está configurado",
                )
            })?;

        let token = decrypt_payload(&settings_secret(), &settings.access_token)
            .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
        let waba_id = settings.whatsapp_business_account_id.trim().to_string();

        let name_for_meta = patch.name.as_deref().unwrap_or(&doc.name);
        let category_str = match patch.category.unwrap_or(doc.category) {
            WaTemplateCategory::Marketing => "MARKETING",
            WaTemplateCategory::Utility => "UTILITY",
            WaTemplateCategory::Authentication => "AUTHENTICATION",
        };
        // Clonar + swap header media_ids → handles Meta (antes de mover el token al service)
        let mut comps_for_meta = patch.components.as_ref().unwrap_or(&doc.components).clone();
        swap_header_handles_in_components(&state, &mut comps_for_meta, &token).await?;
        let comps_val = serde_json::Value::Array(comps_for_meta);

        let wa = WhatsAppService::new(
            state.reqwest_client.clone(),
            settings.phone_number_id.clone(),
            token,
        );

        match wa
            .create_template_meta(
                &waba_id,
                name_for_meta,
                &doc.language,
                category_str,
                &comps_val,
            )
            .await
        {
            Ok(resp) => {
                patch.status = Some(WaTemplateStatus::Pending);
                patch.meta_template_id = Some(Some(resp.id));
                patch.submit_to_meta = Some(true);
            }
            Err(e) => {
                return Err(map_meta_error(&e, "Meta rechazó la plantilla"));
            }
        }
    }

    // 6. Si cambió BODY de un Approved: llamar update_template_body_meta
    if prev_status == WaTemplateStatus::Approved {
        if let Some(ref new_comps) = new_components_opt {
            let settings = state
                .db
                .find_wa_settings_by_phone_number_id(&doc.phone_number_id)
                .await
                .map_err(ApiError::DatabaseError)?
                .ok_or_else(|| {
                    ApiError::domain_with_field(
                        StatusCode::NOT_FOUND,
                        "phone_number_not_found",
                        "phone_number_id",
                        "El número de WhatsApp no está configurado",
                    )
                })?;
            let token = decrypt_payload(&settings_secret(), &settings.access_token)
                .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;

            let meta_id = doc.meta_template_id.as_deref().ok_or_else(|| {
                ApiError::Internal("plantilla aprobada sin meta_template_id".into())
            })?;

            // Swap header media_ids → handles Meta (antes de mover el token al service)
            let mut comps_for_meta = new_comps.clone();
            swap_header_handles_in_components(&state, &mut comps_for_meta, &token).await?;
            let comps_val = serde_json::Value::Array(comps_for_meta);

            let wa = WhatsAppService::new(
                state.reqwest_client.clone(),
                settings.phone_number_id.clone(),
                token,
            );

            if let Err(e) = wa.update_template_body_meta(meta_id, &comps_val).await {
                return Err(map_meta_error(&e, "Meta rechazó la edición del template"));
            }
        }
    }

    // Actualizar components y recomputar body_placeholders
    if let Some(ref new_comps) = new_components_opt {
        let bp = validate_components(new_comps)?;
        patch.components = Some(new_comps.clone());
        patch.body_placeholders = Some(bp);
    }

    // Ejecutar update en DB
    let updated = state
        .db
        .update_template(&oid, patch)
        .await
        .map_err(|e| {
            if e == "name_already_exists" {
                ApiError::domain_with_field(
                    StatusCode::CONFLICT,
                    "name_already_exists",
                    "name_input",
                    "Ya existe una plantilla con ese nombre en este idioma",
                )
            } else {
                ApiError::DatabaseError(e)
            }
        })?
        .ok_or_else(template_not_found)?;

    let item = to_template_item(updated);

    // Emitir WS (prev_status si cambió)
    let prev_for_ws = if item.status != prev_status {
        Some(prev_status)
    } else {
        None
    };
    let ws_payload = build_template_updated_event(&item, prev_for_ws);
    emit_to_phone_number_agents(&state, &item.phone_number_id, ws_payload).await;

    Ok(Json(WaTemplateResponse {
        ok: true,
        data: item,
    }))
}

// ---------------------------------------------------------------------------
// Bundle 6 — process_template_status (webhook handler)
// ---------------------------------------------------------------------------

/// Procesa un evento `message_template_status_update` del webhook de Meta.
/// Mapea el `event` a `WaTemplateStatus`, actualiza en DB, emite WS.
/// Siempre retorna sin error — el webhook debe devolver 200.
async fn process_template_status(
    state: &Arc<AppState>,
    meta_template_id: &str,
    event: &str,
    reason: Option<&str>,
) {
    // 1. Mapear event Meta → (WaTemplateStatus, rejection_reason)
    let (new_status, rejection_reason): (WaTemplateStatus, Option<String>) =
        match event.to_uppercase().as_str() {
            "APPROVED" => (WaTemplateStatus::Approved, None),
            "REJECTED" => (WaTemplateStatus::Rejected, reason.map(|s| s.to_string())),
            "FLAGGED" => (
                WaTemplateStatus::Rejected,
                Some("flagged_by_meta_quality".to_string()),
            ),
            "PAUSED" => (WaTemplateStatus::Paused, reason.map(|s| s.to_string())),
            "DISABLED" => (WaTemplateStatus::Disabled, reason.map(|s| s.to_string())),
            "PENDING" | "IN_REVIEW" => (WaTemplateStatus::Pending, None),
            other => {
                tracing::warn!(
                    "[webhook] process_template_status: evento desconocido '{}' para meta_id={}",
                    other,
                    meta_template_id
                );
                return;
            }
        };

    // 2. Actualizar en DB
    match state
        .db
        .update_template_status(meta_template_id, new_status, rejection_reason)
        .await
    {
        Ok(None) => {
            tracing::warn!(
                "[webhook] process_template_status: template con meta_id={} no encontrado en DB",
                meta_template_id
            );
        }
        Ok(Some((updated_doc, prev_status))) => {
            // 3. Si cambió el status, emitir WS
            if prev_status != new_status {
                let item = to_template_item(updated_doc.clone());
                let ws_payload = build_template_updated_event(&item, Some(prev_status));
                emit_to_phone_number_agents(state, &updated_doc.phone_number_id, ws_payload).await;
            }
        }
        Err(e) => {
            tracing::error!(
                "[webhook] process_template_status: DB error para meta_id={}: {}",
                meta_template_id,
                e
            );
        }
    }
}

#[cfg(test)]
mod webhook_normalization_tests {
    use crate::modules::whatsapp::webhook::normalize::is_known_inbound_type;

    use super::*;
    use std::fs;

    fn load_fixture(filename: &str) -> String {
        let path = format!(
            "{}/src/modules/whatsapp/fixtures/{filename}",
            env!("CARGO_MANIFEST_DIR")
        );
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture missing: {} ({})", path, e))
    }

    fn first_message(payload: &WebhookPayload) -> &InboundMessage {
        payload
            .entry
            .as_ref()
            .and_then(|entries| entries.first())
            .and_then(|entry| entry.changes.as_ref())
            .and_then(|changes| changes.first())
            .and_then(|change| change.value.as_ref())
            .and_then(|value| value.messages.as_ref())
            .and_then(|messages| messages.first())
            .unwrap_or_else(|| panic!("fixture no trae mensajes entrantes"))
    }

    #[test]
    fn inbound_edit_inference_and_markers() {
        let payload: WebhookPayload =
            serde_json::from_str(&load_fixture("webhook_edit.json")).unwrap();
        let msg = first_message(&payload);

        assert_eq!(infer_inbound_effective_type(msg), "edit");
        assert!(inbound_payload_markers(msg).contains("edit"));
        assert!(is_known_inbound_type("edit"));
    }

    #[test]
    fn inbound_revoke_inference_and_markers() {
        let payload: WebhookPayload =
            serde_json::from_str(&load_fixture("webhook_revoke.json")).unwrap();
        let msg = first_message(&payload);

        assert_eq!(infer_inbound_effective_type(msg), "revoke");
        assert!(inbound_payload_markers(msg).contains("revoke"));
        assert!(is_known_inbound_type("revoke"));
        assert_eq!(
            extract_inbound_delta_target_wa_id(msg),
            Some("wamid.orig.002")
        );
    }

    #[test]
    fn inbound_group_inference_and_markers() {
        let payload: WebhookPayload =
            serde_json::from_str(&load_fixture("webhook_group.json")).unwrap();
        let msg = first_message(&payload);

        assert_eq!(infer_inbound_effective_type(msg), "group");
        assert!(inbound_payload_markers(msg).contains("group"));
        assert!(is_known_inbound_type("group"));
    }

    #[test]
    fn inbound_edit_revoke_extract_target_id() {
        let edit_payload: WebhookPayload =
            serde_json::from_str(&load_fixture("webhook_edit.json")).unwrap();

        let edit_msg = first_message(&edit_payload);

        assert_eq!(
            extract_inbound_delta_target_wa_id(edit_msg),
            Some("wamid.orig.001")
        );
        assert_eq!(should_apply_message_delta_update("edit"), true);
        assert_eq!(should_apply_message_delta_update("revoke"), true);
        assert_eq!(should_apply_message_delta_update("text"), false);

        let make_message_base = || InboundMessage {
            from: "1".into(),
            id: "2".into(),
            timestamp: None,
            msg_type: "text".into(),
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
            edit: None,
            revoke: None,
            group: None,
            context: None,
            extra: Default::default(),
        };

        let msg_without_target = make_message_base();

        assert_eq!(
            extract_inbound_delta_target_wa_id(&msg_without_target),
            None
        );

        let msg_with_blank_target = InboundMessage {
            context: Some(InboundContext {
                id: "   ".to_string(),
                from: None,
            }),
            ..make_message_base()
        };
        assert_eq!(
            extract_inbound_delta_target_wa_id(&msg_with_blank_target),
            None
        );

        let msg_with_edit_context_in_payload = InboundMessage {
            context: Some(InboundContext {
                id: "   ".to_string(),
                from: None,
            }),
            edit: Some(serde_json::json!({
                "context": { "id": "wamid.payload.ctx.001" },
                "text": "Actualizado"
            })),
            ..make_message_base()
        };

        assert_eq!(
            extract_inbound_delta_target_wa_id(&msg_with_edit_context_in_payload),
            Some("wamid.payload.ctx.001")
        );

        let msg_with_message_id_in_revoke_payload = InboundMessage {
            context: None,
            edit: None,
            revoke: Some(
                serde_json::json!({ "id": "wamid.payload.revoke.001", "reason": "policy" }),
            ),
            ..make_message_base()
        };

        assert_eq!(
            extract_inbound_delta_target_wa_id(&msg_with_message_id_in_revoke_payload),
            Some("wamid.payload.revoke.001")
        );
    }

    #[test]
    fn top_level_delta_payload_builds_synthetic_message() {
        let value: WebhookValue = serde_json::from_str(
            r#"{
                "metadata": {"display_phone_number":"+15551234567"},
                "contacts": [{"wa_id":"5841400000000","profile":{"name":"Ana"}}],
                "revoke": {
                    "context": {"id": "wamid.orig.010"},
                    "id": "wamid.revoke.top.001",
                    "reason": "message_revoked_by_sender"
                }
            }"#,
        )
        .unwrap();

        let msg = build_top_level_delta_message(&value, "revoke").unwrap();
        assert_eq!(msg.id, "wamid.revoke.top.001");
        assert_eq!(infer_inbound_effective_type(&msg), "revoke");
        assert_eq!(msg.context.as_ref().unwrap().id, "wamid.orig.010");

        assert_eq!(
            extract_inbound_delta_target_wa_id(&msg),
            Some("wamid.orig.010")
        );
    }

    #[test]
    fn inbound_edit_revoke_fallback_content() {
        let edit_payload = InboundMessage {
            from: "573001234567".to_string(),
            id: "wamid.payload.009".to_string(),
            timestamp: None,
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
            edit: Some(serde_json::json!({})),
            revoke: None,
            group: None,
            context: Some(InboundContext {
                id: "wamid.orig.001".to_string(),
                from: None,
            }),
            extra: Default::default(),
        };

        let revoke_payload = InboundMessage {
            from: "573001234567".to_string(),
            id: "wamid.payload.010".to_string(),
            timestamp: None,
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
            edit: None,
            revoke: Some(serde_json::json!({ "text": "" })),
            group: None,
            context: Some(InboundContext {
                id: "wamid.orig.002".to_string(),
                from: None,
            }),
            extra: Default::default(),
        };

        let inferred_edit = infer_inbound_effective_type(&edit_payload);
        let inferred_revoke = infer_inbound_effective_type(&revoke_payload);

        assert_eq!(inferred_edit, "edit");
        assert_eq!(inferred_revoke, "revoke");

        let edit_content = extract_inbound_content(&edit_payload, &inferred_edit);
        let revoke_content = extract_inbound_content(&revoke_payload, &inferred_revoke);

        assert_eq!(edit_content.body, Some("Mensaje editado".to_string()));
        assert_eq!(revoke_content.body, Some("Mensaje revocado".to_string()));
    }

    #[test]
    fn inbound_raw_payload_is_stored_for_delta_types() {
        let edit_payload = InboundMessage {
            from: "573001234567".to_string(),
            id: "wamid.payload.011".to_string(),
            timestamp: None,
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
            edit: Some(serde_json::json!({ "text": "hola" })),
            revoke: None,
            group: None,
            context: None,
            extra: Default::default(),
        };

        let revoke_payload = InboundMessage {
            from: "573001234567".to_string(),
            id: "wamid.payload.012".to_string(),
            timestamp: None,
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
            edit: None,
            revoke: Some(serde_json::json!({ "reason": "policy" })),
            group: None,
            context: None,
            extra: Default::default(),
        };

        let plain_payload = InboundMessage {
            from: "573001234567".to_string(),
            id: "wamid.payload.013".to_string(),
            timestamp: None,
            msg_type: "text".to_string(),
            text: Some(InboundText {
                body: "hola".to_string(),
            }),
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
            edit: None,
            revoke: None,
            group: None,
            context: None,
            extra: Default::default(),
        };

        assert!(inbound_raw_payload(&edit_payload, "edit").is_some());
        assert!(inbound_raw_payload(&revoke_payload, "revoke").is_some());
        assert!(inbound_raw_payload(&plain_payload, "text").is_none());
    }

    #[test]
    fn top_level_errors_are_parsed() {
        let payload: WebhookPayload =
            serde_json::from_str(&load_fixture("webhook_errors.json")).unwrap();
        let change = payload
            .entry
            .as_ref()
            .and_then(|entries| entries.first())
            .and_then(|entry| entry.changes.as_ref())
            .and_then(|changes| changes.first())
            .unwrap_or_else(|| panic!("payload sin cambios"));

        assert_eq!(change.field.as_deref(), Some("errors"));
        let value = change
            .value
            .as_ref()
            .unwrap_or_else(|| panic!("top-level errors sin value"));
        let errors = value
            .errors
            .as_ref()
            .unwrap_or_else(|| panic!("top-level errors sin lista"));

        assert!(!errors.is_empty());
        assert_eq!(errors[0].code, Some(130429));
    }
}
