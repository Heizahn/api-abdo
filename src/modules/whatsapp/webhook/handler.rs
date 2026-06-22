use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use hmac::{Hmac, Mac};
use mongodb::bson::DateTime;
use sha2::Sha256;
use std::{sync::Arc, sync::OnceLock};
use tokio::sync::Mutex;

use crate::{db::WhatsAppRepository, models::whatsapp::*, state::AppState};

use crate::modules::whatsapp::{
    assignment::assign_conversation,
    messaging::{
        download::{prefetch_media, should_prefetch_media},
        reactions::handle_inbound_reaction,
    },
    settings::validation::normalize_to_e164,
    shared,
    url_preview::spawn_preview_job,
    ws::{broadcast_all, broadcast_to_chat_users, ConversacionNoLeidaData, WsServerEvent},
};

use super::{
    media_failures::schedule_inbound_media_failure_fallback,
    normalize::{
        build_top_level_delta_message, describe_top_level_group, extract_inbound_content,
        extract_inbound_delta_target_wa_id, inbound_payload_markers, inbound_raw_payload,
        infer_inbound_effective_type, should_apply_message_delta_update, InboundNormalizedContent,
    },
    record_conv_event,
    status::{
        has_meta_throttle_131049, is_inbound_media_failure_status, log_webhook_top_level_errors,
        process_template_status, InboundMediaFailureDetails,
    },
};

type HmacSha256 = Hmac<Sha256>;

/// Cooldown que aplica el back cuando Meta rebota con error 131049
/// (engagement throttle). Mientras `now < meta_throttle_until` toda la
/// conversación queda bloqueada para envíos. Valor empírico — 6h es
/// suficiente para cubrir la ventana típica del rate limit de Meta sin
/// quedarse pegado en perpetuidad si el inbound del cliente nunca llega.
const META_THROTTLE_COOLDOWN_MS: i64 = 6 * 60 * 60 * 1000;

/// Almacena el último payload crudo recibido de Meta (solo para debug).
static LAST_WEBHOOK_PAYLOAD: OnceLock<Mutex<Option<serde_json::Value>>> = OnceLock::new();

pub(crate) fn last_payload_store() -> &'static Mutex<Option<serde_json::Value>> {
    LAST_WEBHOOK_PAYLOAD.get_or_init(|| Mutex::new(None))
}

fn log_parse_error_sanitized(error: &serde_json::Error, raw: &serde_json::Value) {
    let first_message = raw
        .pointer("/entry/0/changes/0/value/messages/0")
        .unwrap_or(&serde_json::Value::Null);
    let raw_type = first_message
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let has_messages = raw.pointer("/entry/0/changes/0/value/messages").is_some();
    let has_context = first_message.get("context").is_some();
    let has_context_id = first_message.pointer("/context/id").is_some();
    let has_image = first_message.get("image").is_some();
    let has_image_id = first_message.pointer("/image/id").is_some();
    let has_errors = first_message.get("errors").is_some()
        || raw.pointer("/entry/0/changes/0/value/errors").is_some();

    tracing::warn!(
        "[webhook] parse error sanitized: error={} has_messages={} raw_type={} has_context={} context_id={} has_image={} image_id={} has_errors={}",
        error,
        has_messages,
        raw_type,
        has_context,
        if has_context_id { "present" } else { "missing" },
        has_image,
        if has_image_id { "present" } else { "missing" },
        has_errors
    );
}

fn is_meta_unknown_type_notice(msg: &InboundMessage, effective_msg_type: &str) -> bool {
    matches!(effective_msg_type, "unsupported" | "unknown")
        && msg
            .errors
            .as_ref()
            .is_some_and(|errors| errors.iter().any(|error| error.code == Some(131051)))
        && msg.image.is_none()
        && msg.document.is_none()
        && msg.audio.is_none()
        && msg.video.is_none()
        && msg.sticker.is_none()
}

#[derive(serde::Deserialize)]
pub struct WebhookVerifyParams {
    #[serde(rename = "hub.mode")]
    pub mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub challenge: Option<String>,
}

/// GET /v1/webhook/whatsapp
/// Verificación del webhook por parte de Meta.
pub async fn verify_webhook(Query(params): Query<WebhookVerifyParams>) -> impl IntoResponse {
    let expected = std::env::var("WHATSAPP_VERIFY_TOKEN").unwrap_or_default();

    if params.mode.as_deref() == Some("subscribe")
        && params.verify_token.as_deref() == Some(expected.as_str())
    {
        tracing::info!("WhatsApp webhook verificado correctamente");
        (StatusCode::OK, params.challenge.unwrap_or_default())
    } else {
        tracing::warn!("WhatsApp webhook: token inválido");
        (StatusCode::FORBIDDEN, "token_invalido".to_string())
    }
}

/// GET /v1/auth-user/whatsapp/debug/last-webhook
/// Retorna el último payload crudo recibido de Meta. Solo para diagnóstico.
pub async fn debug_last_webhook_handler() -> Json<serde_json::Value> {
    let store = last_payload_store().lock().await;
    match store.as_ref() {
        Some(payload) => {
            Json(serde_json::json!({ "ok": true, "received": true, "payload": payload }))
        }
        None => Json(serde_json::json!({ "ok": true, "received": false, "payload": null })),
    }
}

pub(crate) fn verify_meta_signature(app_secret: &[u8], body: &[u8], header_val: &str) -> bool {
    let expected_hex = match header_val.strip_prefix("sha256=") {
        Some(h) => h,
        None => return false,
    };
    let expected_bytes = match hex_decode(expected_hex) {
        Some(b) => b,
        None => return false,
    };
    let mut mac = match HmacSha256::new_from_slice(app_secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify_slice(&expected_bytes).is_ok()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

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
    let raw: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    *last_payload_store().lock().await = Some(raw.clone());

    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            log_parse_error_sanitized(&e, &raw);
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
                                Ok(false) => {}
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
                                        updated.conversation_id.to_hex(),
                                        shared::time::iso8601(until)
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

            if let Some(messages) = value.messages {
                let contacts = value.contacts.unwrap_or_default();

                let business_phone_raw = value
                    .metadata
                    .as_ref()
                    .and_then(|m| m.display_phone_number.clone())
                    .unwrap_or_default();
                let business_phone = normalize_to_e164(&business_phone_raw);

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
                    let context = msg.context.as_ref();
                    let is_forwarded = context.is_some_and(|c| c.is_forwarded());
                    let is_frequently_forwarded =
                        context.is_some_and(|c| c.is_frequently_forwarded());
                    let has_errors = msg.errors.as_ref().is_some_and(|errs| !errs.is_empty());
                    tracing::debug!(
                        "[webhook] inbound message id={} from={} type={} has_context={} context_id={} forwarded={} frequently_forwarded={} has_image={} image_id={} has_errors={}",
                        msg.id,
                        msg.from,
                        effective_msg_type,
                        msg.context.is_some(),
                        if msg.context.as_ref().and_then(|c| c.id.as_deref()).is_some() { "present" } else { "missing" },
                        is_forwarded,
                        is_frequently_forwarded,
                        msg.image.is_some(),
                        if msg.image.as_ref().and_then(|image| image.id.as_deref()).is_some() { "present" } else { "missing" },
                        has_errors
                    );
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
                    if effective_msg_type == "image" {
                        if let Some(media_id) =
                            msg.image.as_ref().and_then(|image| image.id.as_deref())
                        {
                            if is_forwarded || is_frequently_forwarded {
                                tracing::debug!(
                                    "[webhook] image inbound forwarded message_id={} media_id={} forwarded={} frequently_forwarded={}",
                                    msg.id,
                                    media_id,
                                    is_forwarded,
                                    is_frequently_forwarded
                                );
                            } else {
                                tracing::debug!(
                                    "[webhook] image inbound normal message_id={} media_id={}",
                                    msg.id,
                                    media_id
                                );
                            }
                        }
                    } else if matches!(effective_msg_type.as_str(), "unsupported" | "unknown") {
                        let first_error = msg.errors.as_ref().and_then(|errs| errs.first());
                        tracing::warn!(
                            "[webhook] unsupported/unknown inbound message_id={} error_code={:?} error_title={:?}",
                            msg.id,
                            first_error.and_then(|e| e.code),
                            first_error.and_then(|e| e.title.as_deref())
                        );
                    }

                    if is_meta_unknown_type_notice(&msg, &effective_msg_type) {
                        tracing::warn!(
                            "[webhook] omitiendo aviso Meta 131051 como burbuja visible message_id={} payload_keys={}",
                            msg.id,
                            inbound_payload_markers(&msg)
                        );
                        continue;
                    }

                    if effective_msg_type == "reaction"
                        && handle_inbound_reaction(&state, &msg).await
                    {
                        continue;
                    }

                    if should_apply_message_delta_update(&effective_msg_type)
                        && apply_inbound_message_delta_update(&state, &msg, &effective_msg_type)
                            .await
                    {
                        continue;
                    }

                    let name = contacts
                        .iter()
                        .find(|c| c.wa_id.as_deref() == Some(&msg.from))
                        .and_then(|c| c.profile.as_ref())
                        .and_then(|p| p.name.clone());

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
                        source: None,
                        campaign_id: None,
                        campaign_recipient_id: None,
                        read_by_user_id: None,
                        read_at: None,
                        idempotency_key: None,
                        reply_to_wa_message_id: msg.context.as_ref().and_then(|c| c.reply_to_id()),
                        is_forwarded: Some(is_forwarded).filter(|v| *v),
                        is_frequently_forwarded: Some(is_frequently_forwarded).filter(|v| *v),
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
                        audio_transcription: None,
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

                    let _ = pre_touch_unread;
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

                    if let Err(e) = state.db.update_last_inbound_at(&conv_id, msg_ts).await {
                        tracing::warn!("update_last_inbound_at error: {}", e);
                    }

                    let conv_now = state
                        .db
                        .find_conversation_by_id(&conv_id)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(conv);

                    if conv_created {
                        let ws_name =
                            Some(settings.workspace_name.clone()).filter(|w| !w.is_empty());
                        let resolved = shared::resolve_customer_name(&state, &conv_now).await;
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
                        let reopened_ev = WsServerEvent::ChatEstadoCambio {
                            conversation_id: conv_id.to_hex(),
                            new_status: "pending".to_string(),
                        };
                        broadcast_all(&state.ws_registry, &reopened_ev).await;
                    }

                    let reply_to = shared::resolve_reply_to_for_one(&state, &saved).await;
                    let saved_oid = saved.id;
                    let preview_text = saved.body.clone();
                    let saved_for_dispatch = saved.clone();
                    let message_item = shared::mappers::msg_to_item(saved, None, reply_to);
                    let agent_count = state.ws_registry.read().await.len();
                    tracing::debug!(
                        "[webhook] broadcast MENSAJE_NUEVO wa_id={} conv={} → {} agente(s) conectados",
                        message_item.wa_message_id,
                        conv_id.to_hex(),
                        agent_count
                    );
                    let msg_ev = WsServerEvent::MensajeNuevo {
                        conversation_id: conv_id.to_hex(),
                        message: message_item,
                    };
                    broadcast_all(&state.ws_registry, &msg_ev).await;

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

                    if let (Some(msg_oid), Some(text)) = (saved_oid, preview_text) {
                        spawn_preview_job(state.clone(), msg_oid, conv_id, text);
                    }

                    if conv_now.assigned_to.is_none() {
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            assign_conversation(state_clone, conv_id).await;
                        });
                    }

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

/// Convierte un timestamp de Meta (Unix seconds en string) a `bson::DateTime`.
fn parse_unix_seconds_to_bson(s: &str) -> Option<DateTime> {
    let secs: i64 = s.parse().ok()?;
    Some(DateTime::from_millis(secs.checked_mul(1000)?))
}
