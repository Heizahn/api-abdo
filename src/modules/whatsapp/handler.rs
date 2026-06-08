use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use mongodb::bson::DateTime;
use std::sync::Arc;

use crate::{db::WhatsAppRepository, models::whatsapp::*, state::AppState};

use super::assignment::assign_conversation;
use super::messaging::download::{prefetch_media, should_prefetch_media};
use super::settings::validation::normalize_to_e164;
use super::shared;
use super::webhook::handler::{last_payload_store, verify_meta_signature};
use super::webhook::media_failures::schedule_inbound_media_failure_fallback;
use super::webhook::status::{
    has_meta_throttle_131049, is_inbound_media_failure_status, log_webhook_top_level_errors,
    process_template_status, InboundMediaFailureDetails,
};
use super::ws::{broadcast_all, broadcast_to_chat_users, ConversacionNoLeidaData, WsServerEvent};

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

            let updated_message_item = shared::build_message_item(state, updated).await;

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
                                failed_at: updated.failed_at.map(shared::time::iso8601),
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
                                        updated.conversation_id.to_hex(), shared::time::iso8601(until)
                                    );
                                    let conv_now = state
                                        .db
                                        .find_conversation_by_id(&updated.conversation_id)
                                        .await
                                        .ok()
                                        .flatten();
                                    let (can_send_freeform, freeform_expires_at) =
                                        shared::time::compute_freeform_state(
                                            conv_now.as_ref().and_then(|c| c.last_inbound_at),
                                        );
                                    let last_inbound_iso = conv_now
                                        .as_ref()
                                        .and_then(|c| c.last_inbound_at)
                                        .map(shared::time::iso8601);
                                    let estado_ev = WsServerEvent::ConversacionEstado {
                                        conversation_id: updated.conversation_id.to_hex(),
                                        last_inbound_at: last_inbound_iso,
                                        can_send_freeform,
                                        freeform_expires_at,
                                        meta_throttled: true,
                                        meta_throttle_until: Some(shared::time::iso8601(until)),
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
                        let resolved = shared::resolve_customer_name(&state, &conv_now).await;
                        // Conv recién creada → assigned_to siempre null acá.
                        let new_ev = WsServerEvent::ConversacionNueva {
                            conversation: shared::response::conv_to_item(
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
                    let reply_to = shared::resolve_reply_to_for_one(&state, &saved).await;
                    let saved_oid = saved.id;
                    let preview_text = saved.body.clone();
                    // Clon para el dispatch IA (corre en tokio::spawn más abajo).
                    let saved_for_dispatch = saved.clone();
                    let message_item = shared::mappers::msg_to_item(saved, None, reply_to);
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
                        shared::time::compute_freeform_state(Some(msg_ts));
                    let estado_ev = WsServerEvent::ConversacionEstado {
                        conversation_id: conv_id.to_hex(),
                        last_inbound_at: Some(shared::time::iso8601(msg_ts)),
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

// ============================================
// HELPERS INTERNOS
// ============================================
// ============================================
// HELPERS DE MAPEO
// ============================================

/// Convierte un timestamp de Meta (Unix seconds en string) a `bson::DateTime`.
fn parse_unix_seconds_to_bson(s: &str) -> Option<DateTime> {
    let secs: i64 = s.parse().ok()?;
    Some(DateTime::from_millis(secs.checked_mul(1000)?))
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
