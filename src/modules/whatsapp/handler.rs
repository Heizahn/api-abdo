use axum::{
    body::Bytes,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use tokio::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

/// Almacena el último payload crudo recibido de Meta (solo para debug).
static LAST_WEBHOOK_PAYLOAD: OnceLock<Mutex<Option<serde_json::Value>>> = OnceLock::new();

fn last_payload_store() -> &'static Mutex<Option<serde_json::Value>> {
    LAST_WEBHOOK_PAYLOAD.get_or_init(|| Mutex::new(None))
}
use mongodb::bson::{oid::ObjectId, DateTime};
use std::sync::Arc;

use crate::{
    auth::user_jwt::UserProfileClaims,
    crypto::aes::{decrypt_payload, encrypt_payload},
    db::{
        ProfileRepository, StoreTemplateMediaInput, WaTemplateListFilter,
        WaTemplateMediaRepository, WaTemplateRepository, WaTemplateUpdatePatch, WhatsAppRepository,
    },
    error::ApiError,
    models::whatsapp::*,
    state::AppState,
    utils::get_bson_amount::get_bson_amount,
};

use super::assignment::assign_conversation;
use crate::cache::MEDIA_CACHE_MAX_BYTES;

use super::service::WhatsAppService;
use super::ws::{
    broadcast_all, broadcast_except, broadcast_to_chat_users, build_template_created_event,
    build_template_deleted_event, build_template_updated_event, emit_to_phone_number_agents,
    send_to_user, ConversacionNoLeidaData, TicketPendienteData, WsServerEvent,
};

/// Cooldown que aplica el back cuando Meta rebota con error 131049
/// (engagement throttle). Mientras `now < meta_throttle_until` toda la
/// conversación queda bloqueada para envíos. Valor empírico — 6h es
/// suficiente para cubrir la ventana típica del rate limit de Meta sin
/// quedarse pegado en perpetuidad si el inbound del cliente nunca llega.
const META_THROTTLE_COOLDOWN_MS: i64 = 6 * 60 * 60 * 1000;

/// Persiste un evento de ciclo de vida de conversación. Best-effort:
/// si la inserción falla se loggea pero NO se propaga el error — la
/// auditoría no debe bloquear la respuesta HTTP del agente.
async fn record_conv_event(state: &AppState, input: WaConversationEventInput<'_>) {
    if let Err(e) = state.db.record_conversation_event(input).await {
        tracing::warn!("record_conversation_event failed: {}", e);
    }
}

/// Fuerza un refresh de badges de mensajería (no leídos + tickets abiertos)
/// para todos los usuarios con acceso al inbox WA.
///
/// Se usa en cambios de configuración de números (alta/edición/baja), donde
/// el universo visible puede cambiar sin que exista un `conversation_id`
/// concreto para emitir delta (+1/-1).
async fn emit_chat_badges_refresh(state: &Arc<AppState>, reason: &str) {
    let unread_total = state.db.count_unread_conversations().await.unwrap_or(0);
    let unread_event = WsServerEvent::ConversacionNoLeida {
        data: ConversacionNoLeidaData {
            pending_total: unread_total,
            conversation_id: format!("__refresh__:{reason}"),
            delta: 0,
        },
    };
    if let Ok(payload) = serde_json::to_string(&unread_event) {
        let _ = broadcast_to_chat_users(state, payload).await;
    }

    let tickets_total = state.db.count_open_tickets().await.unwrap_or(0);
    let tickets_event = WsServerEvent::TicketPendiente {
        data: TicketPendienteData {
            pending_total: tickets_total,
            ticket_id: format!("__refresh__:{reason}"),
            previous_status: None,
            new_status: "refresh".to_string(),
        },
    };
    if let Ok(payload) = serde_json::to_string(&tickets_event) {
        let _ = broadcast_to_chat_users(state, payload).await;
    }
}

// ============================================
// WEBHOOK (público)
// ============================================

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
            if let Some(statuses) = value.statuses {
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
                    match state.db.update_message_status(&s.id, &s.status).await {
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
                            let has_131049 = s
                                .errors
                                .as_ref()
                                .is_some_and(|errs| errs.iter().any(|e| e.code == Some(131049)));
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
                            let is_media_failure = s.status == "failed"
                                && s.errors.as_ref().is_some_and(|errs| {
                                    errs.iter().any(|e| {
                                        matches!(e.code, Some(131052) | Some(131053) | Some(131056))
                                    })
                                });
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
                                    let state_cl = state.clone();
                                    tokio::spawn(async move {
                                        notify_inbound_media_failure(
                                            &state_cl,
                                            &recipient,
                                            &business_phone,
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
                        tracing::info!(
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
                    // === REACCIONES — early return, no toca conversación ni persistencia ===
                    if msg.msg_type == "reaction" {
                        let reaction = match &msg.reaction {
                            Some(v) => v,
                            None => {
                                tracing::warn!(
                                    "[webhook] reaction sin payload, ignorando: from={}",
                                    msg.from
                                );
                                continue;
                            }
                        };
                        let target_wamid = reaction
                            .get("message_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let emoji = reaction.get("emoji").and_then(|v| v.as_str()).unwrap_or("");
                        if target_wamid.is_empty() {
                            tracing::warn!("[webhook] reaction sin message_id, ignorando");
                            continue;
                        }

                        match state
                            .db
                            .update_message_reactions(target_wamid, "customer", emoji, None)
                            .await
                        {
                            Ok(Some(updated)) => {
                                let event = WsServerEvent::ReaccionMensaje {
                                    conversation_id: updated.conversation_id.to_hex(),
                                    message_id: updated.id.map(|o| o.to_hex()).unwrap_or_default(),
                                    wa_message_id: target_wamid.to_string(),
                                    emoji: emoji.to_string(),
                                    from: "customer".to_string(),
                                    sender_name: None,
                                };
                                broadcast_all(&state.ws_registry, &event).await;
                            }
                            Ok(None) => {
                                tracing::info!(
                                    "[webhook] reaction sobre wamid desconocido (ignorada): {}",
                                    target_wamid
                                );
                            }
                            Err(e) => {
                                tracing::error!("[webhook] update_message_reactions error: {}", e);
                            }
                        }
                        continue; // CRÍTICO: no caer en el resto del loop (no upsert, no touch, no insert).
                    }

                    let agents = settings.agents.clone();

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

                    // Extraer contenido según tipo (body, media_id, mime, filename)
                    let extract_media = |m: Option<&InboundMedia>| {
                        m.map(|x| {
                            (
                                x.caption.clone(),
                                x.id.clone(),
                                x.mime_type.clone(),
                                x.filename.clone(),
                            )
                        })
                        .unwrap_or((None, None, None, None))
                    };
                    let (body, media_id, media_mime_type, media_filename) = match msg
                        .msg_type
                        .as_str()
                    {
                        "text" => (msg.text.as_ref().map(|t| t.body.clone()), None, None, None),
                        "image" => extract_media(msg.image.as_ref()),
                        "document" => extract_media(msg.document.as_ref()),
                        "audio" => extract_media(msg.audio.as_ref()),
                        "video" => extract_media(msg.video.as_ref()),
                        "sticker" => msg
                            .sticker
                            .as_ref()
                            .map(|m| (None, m.id.clone(), m.mime_type.clone(), None))
                            .unwrap_or((None, None, None, None)),
                        "location" => {
                            // Preview: nombre del lugar → dirección → "Ubicación" genérico.
                            // Las coordenadas ya van en el campo `location`
                            // estructurado; no las ponemos en el preview.
                            let label = msg
                                .location
                                .as_ref()
                                .and_then(|l| l.name.clone().or_else(|| l.address.clone()))
                                .unwrap_or_else(|| "Ubicación".to_string());
                            (Some(label), None, None, None)
                        }
                        // Respuesta a botón/lista interactivo: Meta envía
                        // `interactive.button_reply.{id,title}` o
                        // `interactive.list_reply.{id,title,description}`.
                        // Guardamos el `title` elegido como body (para preview) y
                        // el objeto crudo en `interactive_payload` para que el
                        // front pueda renderizar el contexto completo.
                        "interactive" => {
                            let txt = msg.interactive.as_ref().and_then(|v| {
                                v.get("button_reply")
                                    .and_then(|b| b.get("title"))
                                    .or_else(|| v.get("list_reply").and_then(|l| l.get("title")))
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
                            });
                            (txt, None, None, None)
                        }
                        // Legacy: botón de template (quick-reply de template).
                        // Meta envía `button.text` con el label tapped.
                        "button" => {
                            let txt = msg.button.as_ref().and_then(|v| {
                                v.get("text")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
                            });
                            (txt, None, None, None)
                        }
                        // Tarjeta de contacto: Meta manda un array; usamos el
                        // nombre del primero para el preview del listado.
                        "contacts" => {
                            let name = msg
                                .contacts
                                .as_ref()
                                .and_then(|v| v.as_array())
                                .and_then(|arr| arr.first())
                                .and_then(|c| c.get("name"))
                                .and_then(|n| {
                                    n.get("formatted_name")
                                        .or_else(|| n.get("first_name"))
                                        .and_then(|x| x.as_str())
                                        .map(|s| s.to_string())
                                });
                            (name, None, None, None)
                        }
                        _ => (None, None, None, None),
                    };

                    // Voice note: sólo relevante en `audio`. Meta envía `voice: true`
                    // para push-to-talk y `false` para archivos de audio subidos.
                    let voice = msg.msg_type == "audio"
                        && msg.audio.as_ref().and_then(|a| a.voice).unwrap_or(false);

                    let preview = body
                        .clone()
                        .unwrap_or_else(|| format!("[{}]", msg.msg_type));

                    tracing::info!(
                        "[webhook] guardando mensaje de cliente registrado: {} | tipo: {} | preview: {}",
                        msg.from, msg.msg_type, preview
                    );

                    // Timestamp real desde Meta (Unix seconds en string), fallback a ahora.
                    let msg_ts = msg
                        .timestamp
                        .as_deref()
                        .and_then(parse_unix_seconds_to_bson)
                        .unwrap_or_else(DateTime::now);

                    // Para inbound de tipo `interactive`/`button`, preservamos el
                    // objeto crudo de Meta (incluye `button_reply`/`list_reply`)
                    // para que el front pueda renderizar la burbuja con el
                    // contexto completo de la selección.
                    let interactive_payload = match msg.msg_type.as_str() {
                        "interactive" => msg.interactive.clone(),
                        "button" => msg.button.clone(),
                        _ => None,
                    };

                    // Payload completo de contactos compartidos (vCard).
                    let contacts_payload = if msg.msg_type == "contacts" {
                        msg.contacts.clone()
                    } else {
                        None
                    };

                    // Datos estructurados de ubicación para que el front
                    // renderice el mapa (iframe de OSM/Google, img estática,
                    // o link a maps — lo decide el front).
                    let location_payload = if msg.msg_type == "location" {
                        msg.location
                            .as_ref()
                            .and_then(|l| match (l.latitude, l.longitude) {
                                (Some(lat), Some(lng)) => {
                                    Some(crate::models::whatsapp::LocationPayload {
                                        latitude: lat,
                                        longitude: lng,
                                        name: l.name.clone(),
                                        address: l.address.clone(),
                                    })
                                }
                                _ => None,
                            })
                    } else {
                        None
                    };

                    let wa_msg = WaMessage {
                        id: None,
                        conversation_id: conv_id,
                        wa_message_id: msg.id.clone(),
                        direction: "in".to_string(),
                        msg_type: msg.msg_type.clone(),
                        body,
                        media_id,
                        media_mime_type,
                        media_filename,
                        status: None,
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
                        msg_type: &msg.msg_type,
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
                    tracing::info!(
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
                            assign_conversation(state_clone, conv_id, agents).await;
                        });
                    }

                    // Dispatch IA (shadow/live). Corre en background — si hay
                    // agente activo para este workspace, procesa el turno y
                    // persiste `AiInteraction`. En shadow loguea qué habría
                    // contestado; en live envía la respuesta vía Meta y
                    // emite `MENSAJE_NUEVO` para los agentes humanos.
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

    StatusCode::OK
}

// ============================================
// ENDPOINTS DE STAFF/ADMIN (user JWT)
// ============================================

#[derive(serde::Deserialize)]
pub struct ConversationsQuery {
    pub status: Option<String>,
    pub assigned_to: Option<String>,
    pub business_phone: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

#[derive(serde::Deserialize)]
pub struct MessagesQuery {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

#[derive(serde::Deserialize)]
pub struct ConversationStatsQuery {
    pub business_phone: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/stats",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("business_phone" = Option<String>, Query, description = "Filtrar el scope a un solo número de negocio (E.164 sin '+'). Si se omite, cuenta sobre todos los números."),
    ),
    responses(
        (status = 200, description = "Contadores de conversaciones por categoría", body = ConversationStatsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn conversations_stats_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<ConversationStatsQuery>,
) -> Result<Json<ConversationStatsResponse>, ApiError> {
    let business_phone_norm = q.business_phone.as_deref().map(normalize_to_e164);
    let stats = state
        .db
        .get_conversation_stats(business_phone_norm.as_deref(), &claims.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(ConversationStatsResponse {
        ok: true,
        data: stats,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("status" = Option<String>, Query, description = "Filtrar por estado: pending | in_progress | closed"),
        ("assigned_to" = Option<String>, Query, description = "Filtrar por UUID de agente"),
        ("business_phone" = Option<String>, Query, description = "Filtrar por número de negocio (E.164 sin '+')"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco para paginación (copiar de next_cursor)"),
        ("limit" = Option<i64>, Query, description = "Resultados por página (default: 20, max: 100)"),
    ),
    responses(
        (status = 200, description = "Lista de conversaciones", body = ConversationsListResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_conversations_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<ConversationsQuery>,
) -> Result<Json<ConversationsListResponse>, ApiError> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let business_phone_norm = q.business_phone.as_deref().map(normalize_to_e164);

    let convs = state
        .db
        .get_conversations(
            q.status.as_deref(),
            q.assigned_to.as_deref(),
            business_phone_norm.as_deref(),
            q.cursor.as_deref(),
            limit,
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let next_cursor = if (convs.len() as i64) < limit {
        None
    } else {
        convs.last().and_then(|c| {
            Some(format!(
                "{}_{}",
                c.last_message_at.timestamp_millis(),
                c.id?.to_hex()
            ))
        })
    };

    // Batch-fetch last_opened_at del agente actual para todas las conversaciones.
    let ids: Vec<ObjectId> = convs.iter().filter_map(|c| c.id).collect();
    let opens = state
        .db
        .get_conversation_opens(&claims.id, &ids)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    // Batch-fetch workspace_name por business_phone único.
    let mut unique_phones: Vec<String> = convs.iter().map(|c| c.business_phone.clone()).collect();
    unique_phones.sort();
    unique_phones.dedup();
    let workspaces = state
        .db
        .get_workspace_names(&unique_phones)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    // Batch-resolve nombre del contacto contra Clients: primero por client_id,
    // luego por teléfono para los que no tienen link. Evita N+1 en listados.
    let (names_by_id, names_by_phone) = {
        use crate::db::ProfileRepository;
        let client_ids: Vec<ObjectId> = convs.iter().filter_map(|c| c.client_id).collect();
        let mut customer_phones: Vec<String> = convs
            .iter()
            .filter(|c| c.client_id.is_none())
            .map(|c| c.phone.clone())
            .collect();
        customer_phones.sort();
        customer_phones.dedup();
        let (ids_res, phones_res) = tokio::join!(
            state.db.get_client_names_by_ids(&client_ids),
            state.db.get_client_names_by_phones(&customer_phones),
        );
        (
            ids_res.map_err(ApiError::DatabaseError)?,
            phones_res.map_err(ApiError::DatabaseError)?,
        )
    };

    // Batch-resolve nombres de agentes (autor del último mensaje outbound + asignado).
    let agent_names = resolve_last_message_agent_names(&state, &convs).await;
    let assigned_names = resolve_assigned_agent_names(&state, &convs).await;

    let data = convs
        .into_iter()
        .map(|c| {
            let last_opened = c.id.and_then(|id| opens.get(&id).copied());
            let ws = workspaces.get(&c.business_phone).cloned();
            let resolved = c
                .client_id
                .and_then(|id| names_by_id.get(&id).cloned())
                .or_else(|| names_by_phone.get(&c.phone).cloned());
            let agent_name = c
                .last_message_from_user_id
                .as_ref()
                .and_then(|id| agent_names.get(id).cloned());
            let assigned_name = c
                .assigned_to
                .as_ref()
                .and_then(|id| assigned_names.get(id).cloned());
            conv_to_item(
                c,
                false,
                last_opened,
                ws,
                resolved,
                agent_name,
                assigned_name,
            )
        })
        .collect();

    Ok(Json(ConversationsListResponse {
        ok: true,
        data,
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Detalle de conversación", body = ConversationDetailResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let opens = state
        .db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv).await;
    let assigned_name = resolve_assigned_agent_name_one(&state, &conv).await;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        data: conv_to_item(
            conv,
            true,
            last_opened,
            workspace_name,
            resolved,
            agent_name,
            assigned_name,
        ),
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/{id}/client-link",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Resolución del número del chat a cliente único o múltiples servicios", body = ConversationClientLinkResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "Conversación no encontrada"),
    )
)]
pub async fn get_conversation_client_link_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ConversationClientLinkResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let clients = state
        .db
        .find_clients_by_phone(&conv.phone)
        .await
        .map_err(ApiError::DatabaseError)?;

    if clients.is_empty() {
        let fallback_client_id = conv.client_id.map(|o| o.to_hex());
        return Ok(Json(ConversationClientLinkResponse {
            ok: true,
            data: ConversationClientLinkData {
                available: fallback_client_id.is_some(),
                resolution_type: if fallback_client_id.is_some() {
                    "single".into()
                } else {
                    "none".into()
                },
                client_id: fallback_client_id,
                services: vec![],
            },
        }));
    }

    if clients.len() == 1 {
        return Ok(Json(ConversationClientLinkResponse {
            ok: true,
            data: ConversationClientLinkData {
                available: true,
                resolution_type: "single".into(),
                client_id: Some(clients[0]._id.to_hex()),
                services: vec![],
            },
        }));
    }

    let seed_id = conv.client_id.unwrap_or_else(|| clients[0]._id);
    let raw = state
        .db
        .get_clients_by_phone_group(seed_id.to_hex())
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let mut services: Vec<ConversationClientLinkItem> = raw
        .into_iter()
        .map(|doc| ConversationClientLinkItem {
            id: doc
                .get_object_id("_id")
                .map(|v| v.to_hex())
                .unwrap_or_default(),
            name: doc.get_str("sName").unwrap_or_default().to_string(),
            phone: doc.get_str("sPhone").unwrap_or_default().to_string(),
            status: doc.get_str("sState").ok().map(|s| s.to_string()),
            balance: doc
                .contains_key("nBalance")
                .then(|| get_bson_amount(&doc, "nBalance")),
        })
        .filter(|item| !item.id.is_empty())
        .collect();

    services.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));

    Ok(Json(ConversationClientLinkResponse {
        ok: true,
        data: ConversationClientLinkData {
            available: !services.is_empty(),
            resolution_type: if services.len() <= 1 {
                "single".into()
            } else {
                "multiple".into()
            },
            client_id: if services.len() == 1 {
                services.first().map(|s| s.id.clone())
            } else {
                None
            },
            services,
        },
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/{id}/messages",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "ID de la conversación"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco (copiar de next_cursor)"),
        ("limit" = Option<i64>, Query, description = "Mensajes por página (default: 50, max: 200)"),
    ),
    responses(
        (status = 200, description = "Detalle de conversación + mensajes (más recientes primero)", body = ConversationMessagesResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_conversation_messages_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<ConversationMessagesResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    // Verificar existencia (404 si no existe). No bindeamos: leer mensajes ya
    // no depende del estado de la conv (sin transición pending → in_progress).
    state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);

    let messages = state
        .db
        .get_messages(&oid, q.cursor.as_deref(), limit)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let agent_names = resolve_sent_by_names(&state, &messages).await;
    let reply_items = resolve_reply_to_items(&state, &messages).await;

    let next_cursor = if (messages.len() as i64) < limit {
        None
    } else {
        messages.last().and_then(|m| {
            Some(format!(
                "{}_{}",
                m.timestamp.timestamp_millis(),
                m.id?.to_hex()
            ))
        })
    };

    // Registrar "chat abierto" por este agente (siempre, incluso en paginaciones).
    // Esto es tracking de lectura — NO toca ownership ni status.
    if let Err(e) = state.db.record_conversation_open(&claims.id, &oid).await {
        tracing::warn!("record_conversation_open error: {}", e);
    }

    // NOTA: leer una conversación NO la toma ni cambia su status. La transición
    // pending → in_progress ocurre SOLO vía acciones explícitas: POST /take,
    // POST /intervene (y el reopen+take de envío sobre conv cerrada). Antes el
    // GET transicionaba si el lector era el asignado, lo que "tomaba" la conv
    // (y pausaba la IA) con solo abrirla. Removido a propósito.

    Ok(Json(ConversationMessagesResponse {
        ok: true,
        data: messages
            .into_iter()
            .map(|m| {
                let name = m
                    .sent_by
                    .as_deref()
                    .and_then(|id| agent_names.get(id).cloned());
                let rto = m
                    .reply_to_wa_message_id
                    .as_deref()
                    .and_then(|wid| reply_items.get(wid).cloned());
                msg_to_item(m, name, rto)
            })
            .collect(),
        next_cursor,
    }))
}

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
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let mut conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

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
                let taken_by_name = resolve_user_name_by_id(&state, &claims.id).await;
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
                let rto = resolve_reply_to_for_one(&state, &existing).await;
                let item = msg_to_item(existing, name, rto);
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
            let wa = resolve_service_for_phone(&state, &conv.business_phone).await?;
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

            let rto = resolve_reply_to_for_one(&state, &updated).await;
            let item = msg_to_item(updated, Some(claims.name.clone()), rto);

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
    let wa = resolve_service_for_phone(&state, &conv.business_phone).await?;

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
        sent_by: Some(claims.id.clone()),
        read_by_user_id: None,
        read_at: None,
        idempotency_key: payload.idempotency_key.clone(),
        reply_to_wa_message_id: payload.reply_to.clone(),
        url_preview: None,
        voice: false,
        template_name: sent.template_fields.as_ref().map(|f| f.name.clone()),
        template_language: sent.template_fields.as_ref().map(|f| f.language.clone()),
        template_components: sent.template_fields.and_then(|f| f.components),
        interactive_payload: sent.interactive_payload,
        contacts_payload: sent.contacts_payload,
        location: sent.location,
        reactions: vec![],
        ai_processed_at: None,
        timestamp: DateTime::now(),
    };

    let saved = state
        .db
        .save_message(msg)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let touch = crate::db::ConversationTouch {
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

    let rto = resolve_reply_to_for_one(&state, &saved).await;
    let saved_oid = saved.id;
    let item = msg_to_item(saved, Some(claims.name.clone()), rto);

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
            super::url_preview::spawn_preview_job(state.clone(), msg_oid, oid, preview.clone());
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

/// Decide el modo de envío según el payload y la ventana de 24h.
enum SendMode {
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

struct TemplateFields {
    name: String,
    language: String,
    components: Option<serde_json::Value>,
}

fn resolve_send_mode(
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
            return Err(ApiError::WindowClosed);
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
            return Err(ApiError::WindowExpired);
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
            return Err(ApiError::WindowExpired);
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
            return Err(ApiError::WindowExpired);
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
            return Err(ApiError::WindowExpired);
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
            return Err(ApiError::WindowExpired);
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
            return Err(ApiError::WindowExpired);
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
            return Err(ApiError::WindowExpired);
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
        return Err(ApiError::WindowExpired);
    }

    Ok(SendMode::Text {
        content: content.to_string(),
    })
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

/// Resultado de despachar un `SendMode` al service: contiene todo lo que el
/// handler necesita para persistir el `WaMessage` + armar la `ConversationTouch`.
struct SentData {
    wa_id: String,
    preview: String,
    msg_type: &'static str,
    body: Option<String>,
    media_id: Option<String>,
    media_filename: Option<String>,
    media_mime_type: Option<String>,
    template_fields: Option<TemplateFields>,
    interactive_payload: Option<serde_json::Value>,
    contacts_payload: Option<serde_json::Value>,
    location: Option<LocationPayload>,
}

/// Si el front no incluyó componente HEADER en `components` y la plantilla
/// guardada en nuestra DB tiene header `IMAGE` o `VIDEO`, levanta el binario
/// del GridFS, lo sube a la Cloud Media API de Meta, y mete el componente
/// HEADER al inicio del array.
///
/// **NO aplica para `DOCUMENT`** — los documentos típicamente cambian por
/// envío (recibos, facturas, comprobantes) y deben venir explícitos del front.
/// **NO aplica para `TEXT`** — Meta no exige `parameters` para headers TEXT
/// sin placeholder. Si tiene placeholder, el front manda el HEADER explícito.
///
/// No-ops si: ya hay HEADER en `components`, la plantilla no está en nuestra
/// DB, no se encuentra el binario en GridFS, o el `header_handle` no es un
/// ObjectId nuestro (caso de plantilla migrada con handle Meta legacy).
async fn auto_fill_template_header_media(
    state: &Arc<AppState>,
    components: &mut Option<Vec<serde_json::Value>>,
    template_name: &str,
    template_language: &str,
    business_phone: &str,
    wa: &WhatsAppService,
) -> Result<(), ApiError> {
    // ¿Ya tiene HEADER? Passthrough — el front quiso personalizar.
    let has_header = components.as_deref().unwrap_or(&[]).iter().any(|c| {
        c.get("type")
            .and_then(|v| v.as_str())
            .map(|t| t.eq_ignore_ascii_case("HEADER"))
            .unwrap_or(false)
    });
    if has_header {
        return Ok(());
    }

    // Resolver phone_number_id desde business_phone
    let settings = match state
        .db
        .find_wa_settings_by_phone(business_phone)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        Some(s) => s,
        None => return Ok(()),
    };

    // Buscar plantilla en nuestra DB
    let doc = match state
        .db
        .find_template_by_phone_name_lang(
            &settings.phone_number_id,
            template_name,
            template_language,
        )
        .await
        .map_err(ApiError::DatabaseError)?
    {
        Some(d) => d,
        None => return Ok(()),
    };

    // Buscar componente HEADER en los components guardados
    let header_comp = match doc.components.iter().find(|c| {
        c.get("type")
            .and_then(|v| v.as_str())
            .map(|t| t.eq_ignore_ascii_case("HEADER"))
            .unwrap_or(false)
    }) {
        Some(c) => c,
        None => return Ok(()),
    };

    // Sólo IMAGE y VIDEO se auto-rellenan
    let format = header_comp
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();
    let media_kind: &str = match format.as_str() {
        "IMAGE" => "image",
        "VIDEO" => "video",
        _ => return Ok(()),
    };

    // Extraer ObjectId nuestro de example.header_handle[0]
    let our_media_id = match header_comp
        .pointer("/example/header_handle/0")
        .and_then(|v| v.as_str())
    {
        Some(s) => s,
        None => return Ok(()),
    };
    let oid = match ObjectId::parse_str(our_media_id) {
        Ok(o) => o,
        Err(_) => return Ok(()),
    };

    // Leer binario del GridFS
    let (bytes, mime) = match state
        .db
        .read_template_media_bytes(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        Some(t) => t,
        None => {
            return Err(ApiError::Internal(format!(
                "Template {} tiene header media {} pero el binario no existe en GridFS",
                template_name, our_media_id
            )))
        }
    };

    // Upload a la Cloud Media API de Meta (≠ Resumable Upload del approval)
    let meta_media_id = wa.upload_media(bytes, &mime, None).await.map_err(|e| {
        ApiError::Internal(format!(
            "auto-fill: falló upload del header media a Meta: {}",
            e
        ))
    })?;

    // Construir el componente HEADER. La estructura es:
    //   { type: "HEADER", parameters: [{ type: "image", image: { id: "..." } }] }
    // El nombre del campo dinámico (image/video) se setea con un Map manual
    // porque la macro `json!` no soporta keys variables en runtime.
    let mut media_obj = serde_json::Map::new();
    media_obj.insert("id".to_string(), serde_json::Value::String(meta_media_id));

    let mut param = serde_json::Map::new();
    param.insert(
        "type".to_string(),
        serde_json::Value::String(media_kind.to_string()),
    );
    param.insert(media_kind.to_string(), serde_json::Value::Object(media_obj));

    let header_param = serde_json::json!({
        "type": "HEADER",
        "parameters": [serde_json::Value::Object(param)]
    });

    let mut comps = components.take().unwrap_or_default();
    comps.insert(0, header_param);
    *components = Some(comps);

    Ok(())
}

/// Dispatcher único que cubre todos los `SendMode` — usado en el envío nuevo
/// y en el retry idempotente para evitar duplicar lógica.
async fn dispatch_send(
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
                .map_err(internal)?;
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

/// Resumen legible de un payload `interactive` para persistir como preview
/// y poblar el feed de "último mensaje" de la conversación. No hace
/// validación — si el payload viene mal armado, devuelve un fallback genérico.
fn interactive_preview(payload: &serde_json::Value) -> String {
    // Preferimos el texto del body, luego del header, luego un fallback.
    if let Some(b) = payload
        .get("body")
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
    {
        let t = b.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(h) = payload
        .get("header")
        .and_then(|h| h.get("text"))
        .and_then(|t| t.as_str())
    {
        let t = h.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "[mensaje interactivo]".to_string()
}

fn template_preview(tpl: &SendTemplatePayload) -> String {
    if let Some(rendered) = tpl.rendered_text.as_deref() {
        let t = rendered.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    format!("[plantilla: {}]", tpl.name)
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/mark-read",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Mensajes marcados como leídos", body = MarkReadResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn mark_read_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<MarkReadResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // Actualizar status de inbound en DB y obtener los que cambiaron.
    // El `agent_id` queda persistido en `read_by_user_id` (first-read-wins)
    // para que la auditoría pueda atribuir el inbound a quien lo atendió.
    let changed_ids = state
        .db
        .mark_inbound_as_read(&oid, &claims.id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    // Capture old unread_count BEFORE reset (conv was fetched above).
    let old_unread = conv.unread_count;

    // Resetear contador local en la conversación.
    let _ = state.db.reset_unread(&oid).await;

    // EMIT BADGE: CONVERSACION_NO_LEIDA — only if there was something to clear.
    if old_unread > 0 {
        let pending_total = state.db.count_unread_conversations().await.unwrap_or(0);
        let unread_ev = WsServerEvent::ConversacionNoLeida {
            data: ConversacionNoLeidaData {
                pending_total,
                conversation_id: id.clone(),
                delta: -1,
            },
        };
        if let Ok(badge_payload) = serde_json::to_string(&unread_ev) {
            let _ = broadcast_to_chat_users(&state, badge_payload).await;
        }
    }

    // Notificar a Meta (ticks azules + mic azul en voice notes) para cada
    // inbound del batch. Meta NO propaga `read` a mensajes anteriores — en
    // particular, los audios sólo muestran el mic azul en el teléfono del
    // cliente si se llama `status: "read"` sobre ese `wa_message_id` puntual.
    // Best-effort: si falta credencial o Meta responde error, logueamos y
    // seguimos (no bloquea el endpoint, va en spawn).
    if !changed_ids.is_empty() {
        match resolve_service_for_phone(&state, &conv.business_phone).await {
            Ok(wa) => {
                let ids_to_ack = changed_ids.clone();
                let conv_hex = oid.to_hex();
                tokio::spawn(async move {
                    let mut ok = 0usize;
                    let mut err = 0usize;
                    for wamid in &ids_to_ack {
                        match wa.mark_as_read(wamid).await {
                            Ok(()) => ok += 1,
                            Err(e) => {
                                err += 1;
                                tracing::warn!(
                                    "[mark-read] Meta mark_as_read falló conv={} wamid={}: {}",
                                    conv_hex,
                                    wamid,
                                    e
                                );
                            }
                        }
                    }
                    tracing::info!(
                        "[mark-read] Meta ACK conv={} total={} ok={} err={}",
                        conv_hex,
                        ids_to_ack.len(),
                        ok,
                        err
                    );
                });
            }
            Err(e) => {
                tracing::warn!("[mark-read] no se pudo resolver WhatsAppService: {:?}", e);
            }
        }
    }

    // Broadcast del batch. El front propaga `status: "read"` en la UI local.
    if !changed_ids.is_empty() {
        let ev = WsServerEvent::MensajesVistos {
            conversation_id: id.clone(),
            message_ids: changed_ids.clone(),
            status: "read".to_string(),
        };
        broadcast_all(&state.ws_registry, &ev).await;
    }

    Ok(Json(MarkReadResponse {
        ok: true,
        data: MarkReadData {
            message_ids: changed_ids,
        },
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/take",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Conversación tomada, reasignada o reabierta. Acepta: `pending` (toma/reasignación, transiciona a `in_progress`) y `closed` (reopen+take, también transiciona a `in_progress`).", body = TakeConversationResponse),
        (status = 409, description = "La conversación no es tomable (está en `in_progress`)"),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn take_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<TakeConversationResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let existing = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let previous_status = existing.status.clone();
    let prev_owner = existing.assigned_to.clone();
    let was_already_mine = prev_owner.as_deref() == Some(claims.id.as_str());

    // Sólo `pending` y `closed` son tomables. `in_progress` ya tiene dueño activo → 409.
    if previous_status != "pending" && previous_status != "closed" {
        return Err(ApiError::ConversationNotTakeable);
    }

    // `take_conversation` acepta `pending` (toma/reasignación) y `closed`
    // (reopen+take). En ambos casos el resultado queda en `in_progress`.
    let taken = state
        .db
        .take_conversation(&oid, &claims.id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let conv = match taken {
        Some(c) => c,
        None => return Err(ApiError::ConversationNotTakeable),
    };

    // Ajuste de carga: si había un dueño distinto a mí, le bajamos la carga.
    // Si yo no era dueño, me sube la carga.
    if !was_already_mine {
        state.redis.incr_agent_load(&claims.id).await;
        if let Some(prev) = prev_owner.as_deref() {
            if prev != claims.id {
                state.redis.decr_agent_load(prev).await;
            }
        }
    }

    // Resolver datos adicionales que van tanto en la respuesta HTTP como en el
    // evento WS (para que el resto de agentes vea la conversación actualizada).
    let opens = state
        .db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv).await;
    // Acabamos de asignar la conv al `claims.id`, así que el `assigned_to_name`
    // es directamente el `claims.name` que ya tenemos del JWT (sin DB lookup).
    let assigned_name = Some(claims.name.clone());

    // Broadcast a los demás agentes según el estado previo y el dueño previo:
    // - `closed` → siempre CHAT_TOMADO con broadcast_all (el chat vuelve al mundo).
    // - `pending` sin dueño previo → CHAT_TOMADO con broadcast_except (toma nueva).
    // - `pending` con dueño distinto → CHAT_TRANSFERIDO (reasignación manual).
    // - `pending` ya era mío → idempotente, no emitir.
    if previous_status == "closed" {
        let ev = WsServerEvent::ChatTomado {
            conversation_id: id.clone(),
            taken_by: claims.id.clone(),
            taken_by_name: assigned_name.clone(),
            status: conv.status.clone(),
            previous_status: "closed".to_string(),
        };
        broadcast_all(&state.ws_registry, &ev).await;
        record_conv_event(
            &state,
            WaConversationEventInput {
                conversation_id: &oid,
                business_phone: &conv.business_phone,
                event_type: "taken",
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(claims.name.as_str()),
                target_id: Some(claims.id.as_str()),
                target_name: Some(claims.name.as_str()),
                note: Some("after_reopen"),
            },
        )
        .await;
    } else if !was_already_mine {
        let conv_item = conv_to_item(
            conv.clone(),
            true,
            last_opened,
            workspace_name.clone(),
            resolved.clone(),
            agent_name.clone(),
            assigned_name.clone(),
        );
        let is_takeover = matches!(prev_owner.as_deref(), Some(prev) if prev != claims.id);
        let ev = if is_takeover {
            WsServerEvent::ChatTransferido {
                conversation_id: id.clone(),
                from_user_id: prev_owner.clone(),
                to_user_id: claims.id.clone(),
                conversation: conv_item,
            }
        } else {
            WsServerEvent::ChatTomado {
                conversation_id: id.clone(),
                taken_by: claims.id.clone(),
                taken_by_name: assigned_name.clone(),
                status: conv.status.clone(),
                previous_status: "pending".to_string(),
            }
        };
        // is_takeover: broadcast_all para que el agente destino también reciba
        // el status actualizado (`in_progress`) sin depender solo de la respuesta HTTP.
        // toma nueva: broadcast_except es suficiente (el tomador ya tiene la resp).
        if is_takeover {
            broadcast_all(&state.ws_registry, &ev).await;
            let json = serde_json::to_string(&ev).unwrap_or_default();
            send_to_user(&state.ws_registry, &claims.id, json).await;
            tracing::debug!("[take/takeover] targeted push sent to {}", claims.id);
        } else {
            broadcast_except(&state.ws_registry, &claims.id, &ev).await;
        }
        record_conv_event(
            &state,
            WaConversationEventInput {
                conversation_id: &oid,
                business_phone: &conv.business_phone,
                event_type: if is_takeover { "transferred" } else { "taken" },
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(claims.name.as_str()),
                target_id: Some(claims.id.as_str()),
                target_name: Some(claims.name.as_str()),
                note: None,
            },
        )
        .await;
    }

    Ok(Json(TakeConversationResponse {
        ok: true,
        data: conv_to_item(
            conv,
            true,
            last_opened,
            workspace_name,
            resolved,
            agent_name,
            assigned_name,
        ),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/transfer",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    request_body = TransferConversationRequest,
    responses(
        (status = 200, description = "Conversación transferida", body = ConversationDetailResponse),
        (status = 404, description = "Conversación o usuario destino no encontrado"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn transfer_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<TransferConversationRequest>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // Validar que el usuario destino exista.
    use crate::db::UserRepository;
    let target = state
        .db
        .find_user_by_id(&payload.user_id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or_else(|| ApiError::NotFound)?;

    let from_agent = conv.assigned_to.clone();

    state
        .db
        .assign_conversation(&oid, Some(&payload.user_id))
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    if let Some(prev) = from_agent.as_deref() {
        if prev != payload.user_id {
            state.redis.decr_agent_load(prev).await;
        }
    }
    state.redis.incr_agent_load(&payload.user_id).await;

    if let Some(note) = payload.note.as_deref() {
        tracing::info!(
            "[transfer] conv={} de {:?} → {} por {} ({}): {}",
            id,
            from_agent,
            payload.user_id,
            claims.id,
            claims.name,
            note
        );
    }

    let conv_after = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let opens = state
        .db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv_after.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv_after).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv_after).await;
    let assigned_name = resolve_assigned_agent_name_one(&state, &conv_after).await;
    let conv_item = conv_to_item(
        conv_after,
        true,
        last_opened,
        workspace_name,
        resolved,
        agent_name,
        assigned_name,
    );

    // Emitir tras tener el item listo — incluye el estado actualizado con workspace_name y assigned_to nuevo.
    let ev = WsServerEvent::ChatTransferido {
        conversation_id: id.clone(),
        from_user_id: from_agent.clone(),
        to_user_id: payload.user_id.clone(),
        conversation: conv_item.clone(),
    };
    broadcast_all(&state.ws_registry, &ev).await;
    let json = serde_json::to_string(&ev).unwrap_or_default();
    send_to_user(&state.ws_registry, &payload.user_id, json).await;
    tracing::debug!("[transfer] targeted push sent to {}", payload.user_id);

    record_conv_event(
        &state,
        WaConversationEventInput {
            conversation_id: &oid,
            business_phone: &conv.business_phone,
            event_type: "transferred",
            actor_id: Some(claims.id.as_str()),
            actor_name: Some(claims.name.as_str()),
            target_id: Some(payload.user_id.as_str()),
            target_name: Some(target.name.as_str()),
            note: payload.note.as_deref(),
        },
    )
    .await;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        data: conv_item,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/close",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Conversación cerrada", body = ConversationDetailResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn close_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // Capturar al agente ANTES de cerrar — `close_conversation` desasigna.
    let prev_agent = conv.assigned_to.clone();

    state
        .db
        .close_conversation(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    if let Some(agent) = prev_agent.as_deref() {
        state.redis.decr_agent_load(agent).await;
    }

    // Limpieza de counters AI por conversación al cerrar.
    state.redis.clear_ai_conv_counters(&id).await;

    let ev = WsServerEvent::ChatCerrado {
        conversation_id: id.clone(),
    };
    broadcast_all(&state.ws_registry, &ev).await;

    // EMIT BADGE: CONVERSACION_NO_LEIDA — cerrar puede bajar el conteo si había mensajes sin leer.
    if conv.unread_count > 0 {
        let pending_total = state.db.count_unread_conversations().await.unwrap_or(0);
        let unread_ev = WsServerEvent::ConversacionNoLeida {
            data: ConversacionNoLeidaData {
                pending_total,
                conversation_id: id.clone(),
                delta: -1,
            },
        };
        if let Ok(badge_payload) = serde_json::to_string(&unread_ev) {
            let _ = broadcast_to_chat_users(&state, badge_payload).await;
        }
    }

    record_conv_event(
        &state,
        WaConversationEventInput {
            conversation_id: &oid,
            business_phone: &conv.business_phone,
            event_type: "closed",
            actor_id: Some(claims.id.as_str()),
            actor_name: Some(claims.name.as_str()),
            target_id: None,
            target_name: None,
            note: None,
        },
    )
    .await;

    let conv_after = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let opens = state
        .db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv_after.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv_after).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv_after).await;
    let assigned_name = resolve_assigned_agent_name_one(&state, &conv_after).await;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        data: conv_to_item(
            conv_after,
            true,
            last_opened,
            workspace_name,
            resolved,
            agent_name,
            assigned_name,
        ),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/reopen",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación cerrada")),
    responses(
        (status = 200, description = "Conversación reabierta (status: pending, assigned_to: null) o detalle actual si ya estaba abierta (idempotente).", body = ConversationDetailResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene `bCanChat=true`"),
        (status = 404, description = "Conversación no encontrada"),
    )
)]
pub async fn reopen_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    // Pre-check de existencia: `reopen_conversation` sólo actúa si status==closed.
    // Distinguir "no existe" (404) de "ya abierta" (idempotente) requiere este paso.
    if state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }

    let reopened = state
        .db
        .reopen_conversation(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;

    let conv_after = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let opens = state
        .db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(ApiError::DatabaseError)?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv_after.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv_after).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv_after).await;
    let assigned_name = resolve_assigned_agent_name_one(&state, &conv_after).await;
    let business_phone_for_audit = conv_after.business_phone.clone();
    let conversation_item = conv_to_item(
        conv_after,
        true,
        last_opened,
        workspace_name,
        resolved,
        agent_name,
        assigned_name,
    );

    // Sólo emitimos el evento si realmente se reabrió (transición real).
    // Si era una llamada idempotente sobre una conv ya abierta, no disparamos
    // nada para no confundir a los otros clientes conectados.
    if reopened {
        // Reopen = arranque limpio: limpiamos counters AI por conv.
        state.redis.clear_ai_conv_counters(&id).await;

        let ev = WsServerEvent::ChatReabierto {
            conversation_id: id.clone(),
            conversation: conversation_item.clone(),
        };
        broadcast_all(&state.ws_registry, &ev).await;

        // Notificar al front que ai_conv_state fue limpiado (null = borrado).
        let ev_ia = WsServerEvent::ConversacionEstadoIa {
            conversation_id: id.clone(),
            ai_conv_state: None,
        };
        broadcast_all(&state.ws_registry, &ev_ia).await;

        record_conv_event(
            &state,
            WaConversationEventInput {
                conversation_id: &oid,
                business_phone: &business_phone_for_audit,
                event_type: "reopened",
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(claims.name.as_str()),
                target_id: None,
                target_name: None,
                note: None,
            },
        )
        .await;
    }

    Ok(Json(ConversationDetailResponse {
        ok: true,
        data: conversation_item,
    }))
}

// ============================================
// INICIAR CONVERSACIÓN (agent outbound first)
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/initiate",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = InitiateConversationRequest,
    responses(
        (status = 200, description = "Template enviado y conversación creada/reutilizada", body = SendMessageResponse),
        (status = 400, description = "Parámetros inválidos o template mal formado"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene permiso de chat o no pertenece al workspace"),
        (status = 404, description = "Workspace (business_phone_id) no encontrado"),
    )
)]
pub async fn initiate_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(payload): Json<InitiateConversationRequest>,
) -> Result<Json<SendMessageResponse>, ApiError> {
    // Contrato explícito para frontend: distinguir "sin permiso de chat"
    // de otros 403 del flujo de workspace.
    let caller = {
        use crate::db::UserRepository;
        let user = state
            .db
            .find_user_by_id(&claims.id)
            .await
            .map_err(ApiError::DatabaseError)?
            .ok_or_else(|| {
                ApiError::domain_simple(
                    StatusCode::FORBIDDEN,
                    "whatsapp_chat_permission_required",
                    "Usuario no autorizado para mensajeria WhatsApp",
                )
            })?;
        if user.role != 0.0 && !user.can_chat {
            return Err(ApiError::domain_simple(
                StatusCode::FORBIDDEN,
                "whatsapp_chat_permission_required",
                "Este usuario requiere can_chat=true para iniciar conversaciones",
            ));
        }
        user
    };

    let business_phone_id = payload.business_phone_id.trim().to_string();
    tracing::info!(
        user_id = %caller.id,
        business_phone_id = %business_phone_id,
        expected_field = "WaSettings._id (ObjectId hex de 24 chars)",
        lookup = "find_wa_settings_by_id({_id: ObjectId(...)})",
        "initiate: validando workspace emisor para template outbound"
    );
    let workspace_oid = ObjectId::parse_str(&business_phone_id).map_err(|_| {
        tracing::warn!(
            user_id = %caller.id,
            business_phone_id = %business_phone_id,
            expected_field = "WaSettings._id (ObjectId hex de 24 chars)",
            hint = "No enviar WaSettings.phone_number_id (Meta) ni phone E.164",
            "initiate: business_phone_id invalido para parse ObjectId"
        );
        ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "whatsapp_workspace_id_invalid",
            "business_phone_id",
            "business_phone_id debe ser el _id del workspace (ObjectId hex)",
        )
    })?;

    let settings_opt = state
        .db
        .find_wa_settings_by_id(&workspace_oid)
        .await
        .map_err(ApiError::DatabaseError)?;
    let settings = match settings_opt {
        Some(s) => s,
        None => {
            tracing::warn!(
                user_id = %caller.id,
                workspace_id = %workspace_oid.to_hex(),
                lookup = "find_wa_settings_by_id({_id: ObjectId(...)})",
                "initiate: workspace no encontrado por _id"
            );
            return Err(ApiError::domain_with_field(
                StatusCode::NOT_FOUND,
                "whatsapp_workspace_not_found",
                "business_phone_id",
                "No existe workspace para el business_phone_id enviado",
            ));
        }
    };
    tracing::info!(
        user_id = %caller.id,
        workspace_id = %workspace_oid.to_hex(),
        phone_number_id = %settings.phone_number_id,
        active = settings.active,
        agents_count = settings.agents.len(),
        "initiate: workspace resuelto para envio de template"
    );

    if !settings.agents.iter().any(|a| a == &caller.id) {
        tracing::warn!(
            user_id = %caller.id,
            workspace_id = %workspace_oid.to_hex(),
            "initiate: usuario autenticado no pertenece al workspace.agents"
        );
        return Err(ApiError::domain_simple(
            StatusCode::FORBIDDEN,
            "whatsapp_workspace_membership_required",
            "No tienes permiso sobre este workspace",
        ));
    }

    if !settings.active {
        tracing::warn!(
            user_id = %caller.id,
            workspace_id = %workspace_oid.to_hex(),
            "initiate: workspace inactivo"
        );
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "whatsapp_workspace_inactive",
            "El workspace esta inactivo",
        ));
    }

    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        tracing::warn!(
            user_id = %caller.id,
            workspace_id = %workspace_oid.to_hex(),
            phone_number_id_empty = settings.phone_number_id.is_empty(),
            access_token_empty = settings.access_token.is_empty(),
            "initiate: workspace sin credenciales WhatsApp completas"
        );
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "whatsapp_workspace_credentials_missing",
            "Workspace sin phone_number_id o access_token configurados",
        ));
    }

    let mut tpl = payload.template;
    // Owned para que el borrow de `tpl.name`/`tpl.language` se libere antes
    // del `&mut tpl.components` que necesita `auto_fill_template_header_media`.
    let tpl_name: String = tpl.name.trim().to_string();
    let tpl_lang: String = tpl.language.trim().to_string();
    if tpl_name.is_empty() || tpl_lang.is_empty() {
        return Err(ApiError::MissingTemplateParams);
    }

    let idempotency_key = payload.idempotency_key.trim().to_string();
    if idempotency_key.is_empty() {
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "whatsapp_idempotency_key_required",
            "idempotency_key",
            "idempotency_key es requerido",
        ));
    }

    let to = normalize_to_e164(&payload.to);
    if to.is_empty() {
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "whatsapp_recipient_invalid",
            "to",
            "Numero de destino invalido. Usa formato internacional, ej: 58414XXXXXXX",
        ));
    }

    // Linkear con cliente ISP si el teléfono matchea (best-effort — el link
    // sirve para mostrar datos del cliente en la UI, no bloquea el envío).
    let client_id = {
        use crate::db::ProfileRepository;
        state
            .db
            .find_clients_by_phone(&to)
            .await
            .ok()
            .and_then(|list| list.into_iter().next().map(|c| c._id))
    };

    // Upsert conversación. El nombre lo dejamos en None — si hay inbound
    // posterior, Meta lo trae y se actualiza automáticamente.
    let (conv, conv_created) = state
        .db
        .upsert_conversation(&to, &settings.phone, None)
        .await
        .map_err(ApiError::DatabaseError)?;
    let conv_id = conv
        .id
        .ok_or_else(|| ApiError::Internal("conversación sin _id tras upsert".into()))?;

    // Outbound first → registrar `created` con el agente como actor.
    if conv_created {
        record_conv_event(
            &state,
            WaConversationEventInput {
                conversation_id: &conv_id,
                business_phone: &conv.business_phone,
                event_type: "created",
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(claims.name.as_str()),
                target_id: None,
                target_name: None,
                note: Some("outbound_initiate"),
            },
        )
        .await;
    }

    // Engagement throttle (Meta error 131049): si la conversación ya está en
    // cooldown, no llamamos a la Cloud API — Meta rechazaría igual y gastaríamos
    // request. Mismo error que `resolve_send_mode` para uniformidad en el front.
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

    // Si se creó nueva y matcheó cliente, persistir el link. No reescribimos
    // client_id en conversaciones existentes para no pisar un link manual.
    if conv_created {
        if let Some(cid) = client_id {
            if let Err(e) = state.db.update_conversation_client_id(&conv_id, &cid).await {
                tracing::warn!("initiate: no se pudo vincular client_id: {}", e);
            }
        }
    }

    // Asignar al iniciador si la conversación no tiene dueño. Esto evita que
    // el auto-assign la reasigne a otro agente al primer inbound.
    let needs_assign = conv.assigned_to.is_none();
    if needs_assign {
        if let Err(e) = state
            .db
            .assign_conversation(&conv_id, Some(&claims.id))
            .await
        {
            tracing::warn!("initiate: assign_conversation error: {}", e);
        } else {
            state.redis.incr_agent_load(&claims.id).await;
        }
    }

    // Idempotencia: si ya existe un mensaje con la misma key para esta
    // conversación, devolverlo sin re-enviar (salvo que esté `failed`).
    if let Some(existing) = state
        .db
        .find_message_by_idempotency(&conv_id, &idempotency_key)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        let is_failed = existing.status.as_deref() == Some("failed");
        if !is_failed {
            let rto = resolve_reply_to_for_one(&state, &existing).await;
            let item = msg_to_item(existing, Some(claims.name.clone()), rto);
            return Ok(Json(SendMessageResponse {
                ok: true,
                data: SendMessageData {
                    message_id: item.id.clone(),
                    message: item,
                },
            }));
        }
    }

    // Descifrar access_token y construir el cliente Meta.
    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
    let wa = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id.clone(),
        token,
    );

    // Auto-fill del HEADER si la plantilla tiene IMAGE/VIDEO y el front no lo mandó.
    auto_fill_template_header_media(
        &state,
        &mut tpl.components,
        &tpl_name,
        &tpl_lang,
        &settings.phone,
        &wa,
    )
    .await?;

    let components_value = tpl
        .components
        .as_ref()
        .map(|v| serde_json::Value::Array(v.clone()));
    let wa_id = wa
        .send_template(&to, &tpl_name, &tpl_lang, components_value.as_ref())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let preview = tpl
        .rendered_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("[plantilla: {}]", tpl_name));

    let msg = WaMessage {
        id: None,
        conversation_id: conv_id,
        wa_message_id: wa_id,
        direction: "out".to_string(),
        msg_type: "template".to_string(),
        body: Some(preview.clone()),
        media_id: None,
        media_mime_type: None,
        media_filename: None,
        status: Some("sent".to_string()),
        sent_by: Some(claims.id.clone()),
        read_by_user_id: None,
        read_at: None,
        idempotency_key: Some(idempotency_key),
        reply_to_wa_message_id: None,
        url_preview: None,
        voice: false,
        template_name: Some(tpl_name.to_string()),
        template_language: Some(tpl_lang.to_string()),
        template_components: components_value,
        interactive_payload: None,
        contacts_payload: None,
        location: None,
        reactions: vec![],
        ai_processed_at: None,
        timestamp: DateTime::now(),
    };

    let saved = state
        .db
        .save_message(msg)
        .await
        .map_err(ApiError::DatabaseError)?;
    let touch = crate::db::ConversationTouch {
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
        .touch_conversation(&conv_id, touch)
        .await
        .map_err(ApiError::DatabaseError)?;

    // Releer para emitir `ConversacionNueva` con el estado final (assigned_to,
    // client_id, etc).
    let conv_now = state
        .db
        .find_conversation_by_id(&conv_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .unwrap_or(conv);

    let rto = resolve_reply_to_for_one(&state, &saved).await;
    let item = msg_to_item(saved, Some(claims.name.clone()), rto);

    if conv_created {
        let ws_name = Some(settings.workspace_name.clone()).filter(|w| !w.is_empty());
        let resolved = resolve_customer_name(&state, &conv_now).await;
        // El último mensaje es el template recién enviado por `claims`, así que
        // el nombre del agente sale del token — evitamos un round-trip a Users.
        let agent_name = Some(claims.name.clone());
        // assigned_to acá es claims.id (template envía con el agente como dueño).
        let assigned_name = conv_now.assigned_to.as_ref().map(|_| claims.name.clone());
        let new_ev = WsServerEvent::ConversacionNueva {
            conversation: conv_to_item(
                conv_now,
                false,
                None,
                ws_name,
                resolved,
                agent_name,
                assigned_name,
            ),
        };
        broadcast_all(&state.ws_registry, &new_ev).await;
    }

    let msg_ev = WsServerEvent::MensajeNuevo {
        conversation_id: conv_id.to_hex(),
        message: item.clone(),
    };
    broadcast_all(&state.ws_registry, &msg_ev).await;

    Ok(Json(SendMessageResponse {
        ok: true,
        data: SendMessageData {
            message_id: item.id.clone(),
            message: item,
        },
    }))
}

// ============================================
// AGENTES TRANSFERIBLES
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/transferable-agents",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Usuarios con permiso para atender chats (bCanChat == true)", body = TransferableAgentsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_transferable_agents_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<TransferableAgentsResponse>, ApiError> {
    use crate::db::UserRepository;
    let users = state
        .db
        .find_chat_agents()
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let data = users
        .into_iter()
        .map(|u| TransferableAgentItem {
            id: u.id,
            name: u.name,
            email: u.email,
            role: u.role,
            is_bot: u.is_bot,
        })
        .collect();
    Ok(Json(TransferableAgentsResponse { ok: true, data }))
}

// ============================================
// SETTINGS — Configuración de números y agentes
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/settings",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Lista de configuraciones", body = SettingsListResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_settings_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SettingsListResponse>, ApiError> {
    let items = state
        .db
        .get_all_wa_settings()
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    Ok(Json(SettingsListResponse {
        ok: true,
        data: items.into_iter().map(settings_to_item).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/settings",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = CreateSettingsRequest,
    responses(
        (status = 200, description = "Configuración creada", body = SettingsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn create_settings_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateSettingsRequest>,
) -> Result<Json<SettingsResponse>, ApiError> {
    // Normalizar el número a E.164 venezolano sin "+"
    let phone = normalize_to_e164(&payload.phone);
    let now = mongodb::bson::DateTime::now();

    let access_token = validate_access_token(&payload.access_token)?;
    if payload.phone_number_id.trim().is_empty() {
        return Err(ApiError::BadRequest("phone_number_id requerido".into()));
    }
    let waba_id = payload.whatsapp_business_account_id.trim().to_string();
    if waba_id.is_empty() {
        return Err(ApiError::BadRequest(
            "whatsapp_business_account_id requerido".into(),
        ));
    }

    let encrypted = encrypt_payload(&settings_secret(), access_token);

    let doc = WaSettings {
        id: None,
        phone,
        workspace_name: payload.workspace_name,
        phone_number_id: payload.phone_number_id,
        whatsapp_business_account_id: waba_id,
        access_token: encrypted,
        agents: payload.agents,
        active: true,
        purposes: payload.purposes.unwrap_or_default(),
        templates_synced_at: None,
        enable_guardrails: true,
        enable_conversation_state: true,
        pre_classifier_enabled: false,
        trivial_responses: Vec::new(),
        created_at: now,
        updated_at: now,
    };

    let created = state
        .db
        .create_wa_settings(doc)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    emit_chat_badges_refresh(&state, "settings_created").await;
    Ok(Json(SettingsResponse {
        ok: true,
        data: settings_to_item(created),
    }))
}

#[utoipa::path(
    put,
    path = "/v1/auth-user/whatsapp/settings/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la configuración")),
    request_body = UpdateSettingsRequest,
    responses(
        (status = 200, description = "Configuración actualizada", body = UpdateResponse),
        (status = 404, description = "No encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn update_settings_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateSettingsRequest>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    // Cifrar access_token si vino con valor. `None` o vacío ⇒ no tocar el guardado.
    let encrypted_token = match payload.access_token.as_deref() {
        Some(raw) if !raw.trim().is_empty() => {
            let clean = validate_access_token(raw)?;
            Some(encrypt_payload(&settings_secret(), clean))
        }
        _ => None,
    };

    // WABA id: `Some("")` se ignora (permitir payloads sin borrar el campo).
    let waba = payload.whatsapp_business_account_id.and_then(|v| {
        let t = v.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    });

    state
        .db
        .update_wa_settings(
            &oid,
            payload.workspace_name,
            payload.phone_number_id,
            waba,
            encrypted_token,
            payload.agents,
            payload.active,
            payload.purposes,
            payload.enable_guardrails,
            payload.enable_conversation_state,
            payload.pre_classifier_enabled,
            payload.trivial_responses,
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    emit_chat_badges_refresh(&state, "settings_updated").await;
    Ok(Json(UpdateResponse { ok: true }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/settings/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la configuración")),
    responses(
        (status = 200, description = "Configuración eliminada", body = UpdateResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn delete_settings_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    state
        .db
        .delete_wa_settings(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    emit_chat_badges_refresh(&state, "settings_deleted").await;
    Ok(Json(UpdateResponse { ok: true }))
}

// ============================================
// TEST CONNECTION (verificación contra Meta)
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/settings/test-connection",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = WaTestConnectionRequest,
    responses(
        (status = 200, description = "Credenciales válidas", body = WaTestConnectionResponse),
        (status = 400, description = "phone_number_id o access_token faltante / inválido"),
        (status = 401, description = "No autorizado"),
        (status = 502, description = "Meta rechazó las credenciales"),
    )
)]
pub async fn test_settings_connection_raw_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<WaTestConnectionRequest>,
) -> Result<Json<WaTestConnectionResponse>, ApiError> {
    let phone_number_id = payload
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::BadRequest("phone_number_id requerido".into()))?
        .to_string();
    let token_raw = payload
        .access_token
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("access_token requerido".into()))?;
    let token = validate_access_token(token_raw)?.to_string();

    let svc = apply_media_relay(
        &state,
        WhatsAppService::new(state.reqwest_client.clone(), phone_number_id.clone(), token),
    );

    let info = svc
        .test_phone_number()
        .await
        .map_err(|e| map_meta_error(&e, "no se pudo validar las credenciales contra Meta"))?;

    Ok(Json(WaTestConnectionResponse {
        ok: true,
        data: WaTestConnectionData {
            reachable: true,
            phone_number_id: info.id,
            verified_name: info.verified_name,
            display_phone_number: info.display_phone_number,
            source: WaTestConnectionSource::Body,
        },
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/settings/{id}/test-connection",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del WaSettings a re-validar")),
    request_body = WaTestConnectionRequest,
    responses(
        (status = 200, description = "Credenciales válidas", body = WaTestConnectionResponse),
        (status = 400, description = "id inválido o access_token de override mal formado"),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "WaSettings no encontrado"),
        (status = 502, description = "Meta rechazó las credenciales"),
    )
)]
pub async fn test_settings_connection_stored_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<WaTestConnectionRequest>,
) -> Result<Json<WaTestConnectionResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let settings = state
        .db
        .find_wa_settings_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // Resolver phone_number_id: override del body si vino con valor, si no el guardado.
    let phone_override = payload
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let phone_number_id = phone_override
        .clone()
        .unwrap_or_else(|| settings.phone_number_id.clone());
    if phone_number_id.is_empty() {
        return Err(ApiError::BadRequest(
            "phone_number_id no configurado y no se envió override".into(),
        ));
    }

    // Resolver token: override del body (validado) o el guardado descifrado.
    let token_override = match payload.access_token.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(validate_access_token(raw)?.to_string()),
        _ => None,
    };
    let token = match token_override.as_ref() {
        Some(t) => t.clone(),
        None => {
            if settings.access_token.is_empty() {
                return Err(ApiError::BadRequest(
                    "access_token no guardado y no se envió override".into(),
                ));
            }
            decrypt_payload(&settings_secret(), &settings.access_token)
                .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?
        }
    };

    // `source = body` cuando CUALQUIER credencial vino en el body — refleja que
    // lo validado no es 100% lo guardado.
    let source = if phone_override.is_some() || token_override.is_some() {
        WaTestConnectionSource::Body
    } else {
        WaTestConnectionSource::Stored
    };

    let svc = apply_media_relay(
        &state,
        WhatsAppService::new(state.reqwest_client.clone(), phone_number_id.clone(), token),
    );

    let info = svc
        .test_phone_number()
        .await
        .map_err(|e| map_meta_error(&e, "no se pudo validar las credenciales contra Meta"))?;

    Ok(Json(WaTestConnectionResponse {
        ok: true,
        data: WaTestConnectionData {
            reachable: true,
            phone_number_id: info.id,
            verified_name: info.verified_name,
            display_phone_number: info.display_phone_number,
            source,
        },
    }))
}

// ============================================
// MEDIA (descarga proxy)
// ============================================

/// Proxy de descarga para media subido por el cliente. El binario real vive en la
/// CDN de Meta y sólo es accesible con el access token del negocio — por eso la
/// ruta pasa por el backend en vez de entregar la URL directa al front.
///
/// Autorización: el agente debe estar en `WaSettings.agents` del `business_phone`
/// de la conversación a la que pertenece el media.
#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/media/{media_id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("media_id" = String, Path, description = "ID del media reportado por Meta en el webhook")),
    responses(
        (status = 200, description = "Binario del media con el Content-Type correcto",
            content_type = "application/octet-stream"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Agente no asignado al número de negocio"),
        (status = 404, description = "Media no encontrado"),
    )
)]
pub async fn get_media_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(media_id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    // 1. Mensaje que contiene el media.
    let msg = state
        .db
        .find_message_by_media_id(&media_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 2. Conversación → business_phone.
    let conv = state
        .db
        .find_conversation_by_id(&msg.conversation_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 3. Gate de acceso al módulo: SUPERADMIN siempre, o cualquier usuario
    // con `bCanChat == true`. No exigimos pertenecer a `WaSettings.agents`
    // para descargar media; eso era demasiado restrictivo para supervisión y
    // operación normal del panel.
    require_can_chat(&state, &claims.id).await?;

    // 4. Settings del negocio (credenciales del número).
    let settings = state
        .db
        .find_wa_settings_by_phone(&conv.business_phone)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "wa_settings inactivo o no encontrado para {}",
                conv.business_phone
            ))
        })?;

    // 5. Hot path: cache de Redis. Los media_id son inmutables, así que el
    // primero que haya abierto el media (o el prefetch del webhook) ya lo dejó.
    let t0 = std::time::Instant::now();
    if let Some((bytes, mime, remote_filename)) = state.redis.get_media_cache(&media_id).await {
        tracing::info!(
            "[media] HIT {} ({} bytes, {}) redis={}ms",
            media_id,
            bytes.len(),
            mime,
            t0.elapsed().as_millis()
        );
        let filename = msg
            .media_filename
            .clone()
            .or(remote_filename)
            .unwrap_or_else(|| media_id.clone());
        return Ok(build_media_response(bytes, &mime, &filename));
    }

    // 5.5. Miss + prefetch posiblemente en vuelo: si el lock ya está tomado,
    // hay otra tarea bajándolo. Esperamos ~2s en polls de 100ms a ver si
    // aparece en cache antes de disparar una segunda descarga al Worker.
    if !state.redis.try_lock_media_prefetch(&media_id).await {
        tracing::info!(
            "[media] MISS→WAIT {} — prefetch en vuelo, esperando",
            media_id
        );
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Some((bytes, mime, remote_filename)) =
                state.redis.get_media_cache(&media_id).await
            {
                tracing::info!(
                    "[media] WAIT→HIT {} ({} bytes, {}) wait={}ms",
                    media_id,
                    bytes.len(),
                    mime,
                    t0.elapsed().as_millis()
                );
                let filename = msg
                    .media_filename
                    .clone()
                    .or(remote_filename)
                    .unwrap_or_else(|| media_id.clone());
                return Ok(build_media_response(bytes, &mime, &filename));
            }
        }
        // El otro task tardó demasiado o falló — seguimos con descarga propia.
        tracing::warn!(
            "[media] MISS→WAIT timeout para {} — bajando por nuestra cuenta",
            media_id
        );
    } else {
        tracing::warn!(
            "[media] MISS {} — cayendo a Meta (prefetch no completó a tiempo o falló)",
            media_id
        );
    }
    // Guard: al salir del handler liberamos el lock. Si la descarga falla,
    // otro request puede reintentar inmediatamente en vez de esperar el TTL.
    let _lock_guard = MediaPrefetchGuard {
        redis: state.redis.clone(),
        media_id: media_id.clone(),
    };

    // 6. Cache miss → descargar de Meta.
    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        return Err(ApiError::Internal(
            "wa_settings sin phone_number_id o access_token configurados".into(),
        ));
    }
    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
    let wa = apply_media_relay(
        &state,
        WhatsAppService::new(
            state.reqwest_client.clone(),
            settings.phone_number_id,
            token,
        ),
    );

    let t_meta = std::time::Instant::now();
    let (bytes, mime, remote_filename) = wa.download_media(&media_id).await.map_err(|e| {
        tracing::error!(
            "[media] download_media falló para {} tras {}ms: {}",
            media_id,
            t_meta.elapsed().as_millis(),
            e
        );
        ApiError::Internal(e.to_string())
    })?;
    tracing::info!(
        "[media] MISS→FETCH {} ({} bytes, {}) meta={}ms",
        media_id,
        bytes.len(),
        mime,
        t_meta.elapsed().as_millis()
    );

    // Guardar en cache fire-and-forget para la próxima request (y para los
    // demás agentes que abran el mismo chat).
    {
        let state_cl = state.clone();
        let mid_cl = media_id.clone();
        let bytes_cl = bytes.clone();
        let mime_cl = mime.clone();
        let filename_cl = remote_filename.clone();
        tokio::spawn(async move {
            state_cl
                .redis
                .set_media_cache(&mid_cl, &bytes_cl, &mime_cl, filename_cl.as_deref())
                .await;
        });
    }

    let filename = msg
        .media_filename
        .clone()
        .or(remote_filename)
        .unwrap_or_else(|| media_id.clone());
    Ok(build_media_response(bytes, &mime, &filename))
}

// Mime types aceptados por Meta Cloud API para cada tipo de upload.
// Referencia: https://developers.facebook.com/docs/whatsapp/cloud-api/reference/media
const MIME_IMAGE: &[&str] = &["image/jpeg", "image/png"];
const MIME_VIDEO: &[&str] = &["video/mp4", "video/3gpp"];
const MIME_AUDIO: &[&str] = &[
    "audio/aac",
    "audio/mp4",
    "audio/mpeg",
    "audio/amr",
    "audio/ogg",
];
const MIME_DOCUMENT: &[&str] = &[
    "application/pdf",
    "application/vnd.ms-powerpoint",
    "application/msword",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "text/plain",
];
const MIME_STICKER: &[&str] = &["image/webp"];

// Tamaños máximos por tipo — son los límites oficiales de Meta Cloud API
// (protocolo, iguales para todas las cuentas). Hardcoded aquí porque no hay
// caso de uso real para tunearlos por deploy/workspace.
// Sticker a 500 KB cubre tanto estáticos (Meta: 100 KB) como animados (500 KB);
// Meta rechaza server-side si el static supera su sub-límite interno.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 16 * 1024 * 1024;
const MAX_AUDIO_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 100 * 1024 * 1024;
const MAX_STICKER_BYTES: u64 = 500 * 1024;

/// Convierte bytes a texto human-readable ("16 MB", "100 KB", "800 B").
/// Usa 1 decimal solo si aporta (no muestra "16.0 MB").
fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if n >= MB {
        let mb = n as f64 / MB as f64;
        if (mb.fract() * 10.0).round() == 0.0 {
            format!("{:.0} MB", mb)
        } else {
            format!("{:.1} MB", mb)
        }
    } else if n >= KB {
        format!("{} KB", (n as f64 / KB as f64).round() as u64)
    } else {
        format!("{} B", n)
    }
}

/// Label en español + lista de extensiones aceptadas para un tipo de media.
/// Usado al formatear mensajes de error user-facing.
fn media_type_label(type_str: &str) -> (&'static str, &'static str) {
    match type_str {
        "image" => ("imagen", "jpeg, png"),
        "video" => ("video", "mp4, 3gp"),
        "audio" => ("audio", "aac, amr, mp3, m4a, ogg"),
        "document" => ("documento", "pdf, doc(x), ppt(x), xls(x), txt"),
        "sticker" => ("sticker", "webp"),
        _ => ("archivo", ""),
    }
}

/// Resuelve `(max_bytes, mime_allowlist)` para un string de tipo.
fn media_type_limits(type_str: &str) -> Option<(u64, &'static [&'static str])> {
    match type_str {
        "image" => Some((MAX_IMAGE_BYTES, MIME_IMAGE)),
        "video" => Some((MAX_VIDEO_BYTES, MIME_VIDEO)),
        "audio" => Some((MAX_AUDIO_BYTES, MIME_AUDIO)),
        "document" => Some((MAX_DOCUMENT_BYTES, MIME_DOCUMENT)),
        "sticker" => Some((MAX_STICKER_BYTES, MIME_STICKER)),
        _ => None,
    }
}

/// Deriva el `type` de Meta a partir del `Content-Type` del file part.
/// Usado cuando el front sube sin mandar el campo `type` explícito.
/// `image/webp` → `sticker` (Meta sólo acepta webp en stickers, no en image).
/// `application/octet-stream` o mimes raros → `document` (catch-all).
fn infer_type_from_mime(mime: &str) -> Option<&'static str> {
    if mime == "image/webp" {
        return Some("sticker");
    }
    if mime.starts_with("image/") {
        return Some("image");
    }
    if mime.starts_with("video/") {
        return Some("video");
    }
    if mime.starts_with("audio/") {
        return Some("audio");
    }
    // PDFs, Word/Excel/PowerPoint, text/plain — todo cae en document.
    if MIME_DOCUMENT.iter().any(|m| *m == mime) {
        return Some("document");
    }
    None
}

/// POST /v1/auth-user/whatsapp/media (multipart/form-data)
///
/// Sube un binario a Meta Cloud API y devuelve el `media_id` para usarlo en un
/// `POST /conversations/:id/messages` posterior. Flujo replicado 1:1 del de
/// Meta (dos pasos) para: validar tamaño/mime server-side, separar errores de
/// upload vs. envío, y ocultar el access_token cifrado al browser.
///
/// Campos del multipart:
/// - `file` (binary, requerido): el archivo a subir.
/// - `type` (text, requerido): `"image" | "video" | "document" | "audio" | "sticker"`.
/// - `conversation_id` (text, requerido): ID de la conversación; define qué
///   `WaSettings` (phone_number_id + token) usar para subir. Meta asocia el
///   `media_id` al phone_number_id que lo creó.
///
/// Autorización: cualquier usuario con `bCanChat == true` puede subir media
/// para luego enviarlo en la conversación. Los `SUPERADMIN` también pasan
/// aunque tengan `bCanChat=false`. Usuarios sin acceso al módulo no pueden
/// subir media ni enviar mensajes.
#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/media",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body(
        content = String,
        content_type = "multipart/form-data",
        description = "Campos: `file` (binario), `type` (image|video|document|audio|sticker), `conversation_id` (ObjectId hex)",
    ),
    responses(
        (status = 200, description = "Media subido", body = MediaUploadResponse),
        (status = 400, description = "Falta un campo o es inválido"),
        (status = 403, description = "El usuario no tiene acceso al módulo de chat"),
        (status = 404, description = "Conversación no encontrada"),
        (status = 422, description = "Validación falló: campo requerido vacío, tamaño excedido, o MIME no soportado"),
    )
)]
pub async fn upload_media_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    mut multipart: Multipart,
) -> Result<Json<MediaUploadResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_mime: Option<String> = None;
    let mut file_name: Option<String> = None;
    let mut type_str: Option<String> = None;
    let mut conv_id_str: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        tracing::error!("[upload_media] multipart error: {}", e);
        ApiError::BadRequest("error leyendo multipart".into())
    })? {
        match field.name().unwrap_or("") {
            "file" => {
                file_mime = field.content_type().map(|s| s.to_string());
                file_name = field.file_name().map(|s| s.to_string());
                let data = field
                    .bytes()
                    .await
                    .map_err(|_| ApiError::BadRequest("error leyendo file".into()))?;
                file_bytes = Some(data.to_vec());
            }
            "type" => {
                type_str = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| ApiError::BadRequest("type inválido".into()))?
                        .trim()
                        .to_lowercase(),
                );
            }
            "conversation_id" => {
                conv_id_str = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| ApiError::BadRequest("conversation_id inválido".into()))?
                        .trim()
                        .to_string(),
                );
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let bytes = file_bytes.ok_or_else(|| ApiError::ValidationError {
        code: "missing_field".into(),
        field: "file".into(),
        message: "Adjuntá el archivo para subir.".into(),
    })?;
    if bytes.is_empty() {
        return Err(ApiError::ValidationError {
            code: "file_empty".into(),
            field: "file".into(),
            message: "El archivo está vacío.".into(),
        });
    }
    let conv_id_str = conv_id_str.ok_or_else(|| ApiError::ValidationError {
        code: "missing_field".into(),
        field: "conversation_id".into(),
        message: "Falta identificar la conversación.".into(),
    })?;

    // Si el front no mandó `type`, lo inferimos del Content-Type del file part.
    // Útil para clientes simples que no quieren replicar la taxonomía de Meta.
    let mime_lower = file_mime
        .as_deref()
        .unwrap_or("application/octet-stream")
        .to_lowercase();
    let type_str = match type_str {
        Some(t) => t,
        None => infer_type_from_mime(&mime_lower).ok_or_else(|| ApiError::ValidationError {
            code: "unrecognized_mime".into(),
            field: "type".into(),
            message: format!("No reconocemos el tipo de archivo (`{}`). Revisá la extensión o adjuntalo con otro formato.", mime_lower),
        })?.to_string(),
    };

    let (max_bytes, allowed_mimes) =
        media_type_limits(&type_str).ok_or_else(|| ApiError::ValidationError {
            code: "invalid_media_type".into(),
            field: "type".into(),
            message: "El tipo debe ser image, video, document, audio o sticker.".into(),
        })?;

    if (bytes.len() as u64) > max_bytes {
        let (label, _) = media_type_label(&type_str);
        return Err(ApiError::ValidationError {
            code: "media_too_large".into(),
            field: "file".into(),
            message: format!(
                "El {} supera el límite de {} (recibido {}). Comprimilo o usá uno más liviano.",
                label,
                human_bytes(max_bytes),
                human_bytes(bytes.len() as u64)
            ),
        });
    }

    if !allowed_mimes.iter().any(|m| *m == mime_lower) {
        let (label, formats) = media_type_label(&type_str);
        return Err(ApiError::ValidationError {
            code: "mime_not_allowed".into(),
            field: "file".into(),
            message: format!(
                "Ese formato no se puede enviar como {}. Formatos aceptados: {}.",
                label, formats
            ),
        });
    }

    // Resolver conversación/número para decidir contra qué `WaSettings`
    // subimos el binario a Meta. La autorización ya quedó validada con
    // `require_can_chat`.
    let conv_oid = ObjectId::parse_str(&conv_id_str)
        .map_err(|_| ApiError::BadRequest("conversation_id inválido".into()))?;
    let conv = state
        .db
        .find_conversation_by_id(&conv_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // SHA-256 del binario — el front lo usa para deduplicar reenvíos idénticos.
    let sha256_hex = {
        let mut h = Sha256::new();
        h.update(&bytes);
        let out = h.finalize();
        out.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    };

    let size = bytes.len() as u64;

    // Subir a Meta (sin relay — el relay sólo aplica a downloads desde lookaside).
    let wa = resolve_service_for_phone(&state, &conv.business_phone).await?;
    let media_id = wa
        .upload_media(bytes, &mime_lower, file_name.as_deref())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!(
        "[upload_media] OK media_id={} size={}B type={} mime={} conv={}",
        media_id,
        size,
        type_str,
        mime_lower,
        conv_id_str
    );

    Ok(Json(MediaUploadResponse {
        ok: true,
        data: crate::models::whatsapp::MediaUploadData {
            media_id,
            mime_type: mime_lower,
            size,
            sha256: sha256_hex,
        },
    }))
}

/// GET /v1/auth-user/whatsapp/media/limits
///
/// Devuelve los límites de tamaño y los MIME types aceptados por cada tipo de
/// media. El front lo consulta para validar client-side antes de llamar al
/// upload, y para mostrar mensajes de error coherentes con el backend.
#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/media/limits",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Límites vigentes", body = MediaLimitsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_media_limits_handler() -> Json<MediaLimitsResponse> {
    let as_vec = |slice: &[&str]| slice.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    Json(MediaLimitsResponse {
        ok: true,
        image: MediaTypeLimit {
            max_bytes: MAX_IMAGE_BYTES,
            mime_types: as_vec(MIME_IMAGE),
        },
        video: MediaTypeLimit {
            max_bytes: MAX_VIDEO_BYTES,
            mime_types: as_vec(MIME_VIDEO),
        },
        audio: MediaTypeLimit {
            max_bytes: MAX_AUDIO_BYTES,
            mime_types: as_vec(MIME_AUDIO),
        },
        document: MediaTypeLimit {
            max_bytes: MAX_DOCUMENT_BYTES,
            mime_types: as_vec(MIME_DOCUMENT),
        },
        sticker: MediaTypeLimit {
            max_bytes: MAX_STICKER_BYTES,
            mime_types: as_vec(MIME_STICKER),
        },
    })
}

/// Arma la respuesta HTTP con el binario y headers compartidos entre hit y miss.
/// `Cache-Control: immutable` + 30 días porque los `media_id` de Meta no cambian:
/// el browser no vuelve a pedirlo hasta un mes después.
fn build_media_response(bytes: Vec<u8>, mime: &str, filename: &str) -> axum::response::Response {
    let content_length = bytes.len();
    let mut resp = axum::response::Response::new(axum::body::Body::from(bytes));
    let headers = resp.headers_mut();
    if let Ok(v) = axum::http::HeaderValue::from_str(mime) {
        headers.insert(axum::http::header::CONTENT_TYPE, v);
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!(
        "inline; filename=\"{}\"",
        filename.replace('"', "'")
    )) {
        headers.insert(axum::http::header::CONTENT_DISPOSITION, v);
    }
    headers.insert(
        axum::http::header::CONTENT_LENGTH,
        axum::http::HeaderValue::from(content_length),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, max-age=2592000, immutable"),
    );
    resp
}

// ============================================
// QUICK REPLIES (mensajes rápidos)
// ============================================

#[derive(serde::Deserialize)]
pub struct QuickRepliesQuery {
    /// Hex de `WaSettings._id`. Si viene, filtra a ese workspace puntual
    /// (el agente debe pertenecer a él o devuelve lista vacía).
    pub workspace_id: Option<String>,
    /// Si viene, filtra por `active = bool`. Omitir para traer todos.
    pub active: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/quick-replies",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("workspace_id" = Option<String>, Query, description = "Filtrar por workspace puntual (hex de WaSettings._id)"),
        ("active" = Option<bool>, Query, description = "Filtrar por estado activo (true/false)"),
    ),
    responses(
        (status = 200, description = "Lista completa de quick replies. Con `?workspace_id=X` filtra a items que tengan X en `workspace_ids`. Cada item incluye `can_edit` calculado para el caller (flag de delete).", body = QuickRepliesListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sin `bCanChat=true`"),
    )
)]
pub async fn list_quick_replies_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<QuickRepliesQuery>,
) -> Result<Json<QuickRepliesListResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let filter_oid = match q.workspace_id.as_deref() {
        Some(hex) => Some(
            ObjectId::parse_str(hex)
                .map_err(|_| ApiError::BadRequest("workspace_id inválido".into()))?,
        ),
        None => None,
    };

    let docs = state
        .db
        .list_quick_replies(filter_oid.as_ref(), q.active)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(QuickRepliesListResponse {
        ok: true,
        data: docs
            .into_iter()
            .map(|q| quick_reply_to_item(q, &caller, &caller_workspaces))
            .collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/quick-replies",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = CreateQuickReplyRequest,
    responses(
        (status = 200, description = "Snippet creado", body = QuickReplyResponse),
        (status = 400, description = "Validación falló, `workspace_ids` vacío, o algún id no existe"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sin `bCanChat=true`, o no es agente en todos los workspaces indicados (y no es superadmin)"),
    )
)]
pub async fn create_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(payload): Json<CreateQuickReplyRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    use super::quick_reply_validation::{validate_quick_reply, ValidatedQuickReply};

    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let title = payload.title.trim().to_string();
    let content = payload.content.trim().to_string();
    let workspace_oids = parse_and_validate_workspaces(&state, &payload.workspace_ids).await?;
    require_create_permission(caller.role, &caller_workspaces, &workspace_oids)?;
    let footer = payload.footer.as_ref().map(|s| s.trim().to_string());

    validate_quick_reply(&ValidatedQuickReply {
        title: &title,
        content: &content,
        workspace_ids_len: workspace_oids.len(),
        header: payload.header.as_ref(),
        footer: footer.as_deref(),
        buttons: payload.buttons.as_deref(),
        list: payload.list.as_ref(),
        cta_url: payload.cta_url.as_ref(),
    })?;

    let now = DateTime::now();
    let doc = WaQuickReply {
        id: None,
        title,
        content,
        workspace_ids: workspace_oids,
        created_by: claims.id.clone(),
        created_by_name: claims.name.clone(),
        created_at: now,
        updated_at: now,
        active: payload.active.unwrap_or(true),
        header: payload.header,
        footer,
        buttons: payload.buttons,
        list: payload.list,
        cta_url: payload.cta_url,
        use_count: 0,
        last_used_at: None,
    };

    let saved = state
        .db
        .create_quick_reply(doc)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(saved, &caller, &caller_workspaces),
    }))
}

#[utoipa::path(
    put,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet")),
    request_body = UpdateQuickReplyRequest,
    responses(
        (status = 200, description = "Snippet actualizado", body = QuickReplyResponse),
        (status = 400, description = "Validación falló"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene `bCanChat=true`"),
        (status = 404, description = "Snippet no encontrado"),
    )
)]
pub async fn update_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateQuickReplyRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    use super::quick_reply_validation::{validate_quick_reply, ValidatedQuickReply};
    use crate::db::UpdateQuickReplyPatch;

    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let existing = state
        .db
        .find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // Normalización + parse de campos planos
    let title_new = payload.title.as_ref().map(|t| t.trim().to_string());
    let content_new = payload.content.as_ref().map(|c| c.trim().to_string());
    let workspace_oids = match &payload.workspace_ids {
        Some(list) => Some(parse_and_validate_workspaces(&state, list).await?),
        None => None,
    };
    let footer_patch: Option<Option<String>> = payload
        .footer
        .as_ref()
        .map(|opt| opt.as_ref().map(|s| s.trim().to_string()));

    // Merge patch + existing → estado final, para validar el doc completo.
    let merged_title = title_new.clone().unwrap_or_else(|| existing.title.clone());
    let merged_content = content_new
        .clone()
        .unwrap_or_else(|| existing.content.clone());
    let merged_ws_len = workspace_oids
        .as_ref()
        .map(|v| v.len())
        .unwrap_or(existing.workspace_ids.len());

    // Campos nullable: Some(Some) → nuevo valor, Some(None) → clear, None → mantener existente.
    let merged_header: Option<QuickReplyHeader> = match &payload.header {
        Some(Some(h)) => Some(h.clone()),
        Some(None) => None,
        None => existing.header.clone(),
    };
    let merged_footer: Option<String> = match &footer_patch {
        Some(Some(f)) => Some(f.clone()),
        Some(None) => None,
        None => existing.footer.clone(),
    };
    let merged_buttons: Option<Vec<QuickReplyButton>> = match &payload.buttons {
        Some(Some(b)) => Some(b.clone()),
        Some(None) => None,
        None => existing.buttons.clone(),
    };
    let merged_list: Option<QuickReplyList> = match &payload.list {
        Some(Some(l)) => Some(l.clone()),
        Some(None) => None,
        None => existing.list.clone(),
    };
    let merged_cta: Option<QuickReplyCtaUrl> = match &payload.cta_url {
        Some(Some(c)) => Some(c.clone()),
        Some(None) => None,
        None => existing.cta_url.clone(),
    };

    validate_quick_reply(&ValidatedQuickReply {
        title: &merged_title,
        content: &merged_content,
        workspace_ids_len: merged_ws_len,
        header: merged_header.as_ref(),
        footer: merged_footer.as_deref(),
        buttons: merged_buttons.as_deref(),
        list: merged_list.as_ref(),
        cta_url: merged_cta.as_ref(),
    })?;

    let patch = UpdateQuickReplyPatch {
        title: title_new,
        content: content_new,
        workspace_ids: workspace_oids,
        active: payload.active,
        header: payload.header,
        footer: footer_patch,
        buttons: payload.buttons,
        list: payload.list,
        cta_url: payload.cta_url,
    };

    let updated = state
        .db
        .update_quick_reply(&oid, patch)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(updated, &caller, &caller_workspaces),
    }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet")),
    responses(
        (status = 200, description = "Snippet eliminado", body = UpdateResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sin `bCanChat=true`, o sin overlap entre workspaces del caller y del item (y no es superadmin)"),
        (status = 404, description = "Snippet no encontrado"),
    )
)]
pub async fn delete_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let existing = state
        .db
        .find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;
    if !compute_can_edit(caller.role, &caller_workspaces, &existing.workspace_ids) {
        return Err(ApiError::Forbidden);
    }

    let deleted = state
        .db
        .delete_quick_reply(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !deleted {
        return Err(ApiError::NotFound);
    }
    Ok(Json(UpdateResponse { ok: true }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}/active",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet")),
    request_body = ToggleActiveRequest,
    responses(
        (status = 200, description = "Estado actualizado", body = QuickReplyResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene `bCanChat=true`"),
        (status = 404, description = "Snippet no encontrado"),
    )
)]
pub async fn set_quick_reply_active_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<ToggleActiveRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let updated = state
        .db
        .set_quick_reply_active(&oid, payload.active)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(updated, &caller, &caller_workspaces),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}/duplicate",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet original")),
    request_body = DuplicateQuickReplyRequest,
    responses(
        (status = 200, description = "Snippet duplicado. Se aplica la misma regla que crear sobre los workspaces del item resultante (los del payload si vienen, los del original si no).", body = QuickReplyResponse),
        (status = 400, description = "Validación falló o algún `workspace_id` no existe"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sin `bCanChat=true`, o no es agente en todos los workspaces del item resultante (y no es superadmin)"),
        (status = 404, description = "Snippet original no encontrado"),
    )
)]
pub async fn duplicate_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<DuplicateQuickReplyRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let original = state
        .db
        .find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let title = match payload.title.as_deref() {
        Some(t) => {
            let trimmed = t.trim().to_string();
            if trimmed.is_empty() || trimmed.chars().count() > 100 {
                return Err(ApiError::ValidationError {
                    code: "quick_reply_title_length".into(),
                    field: "title".into(),
                    message: "El título debe tener entre 1 y 100 caracteres.".into(),
                });
            }
            trimmed
        }
        None => {
            let proposed = format!("{} (copia)", original.title);
            // Truncar si supera 100 chars — nunca falla por el suffix.
            proposed.chars().take(100).collect::<String>()
        }
    };
    let workspace_oids = match payload.workspace_ids {
        Some(list) => parse_and_validate_workspaces(&state, &list).await?,
        None => original.workspace_ids.clone(),
    };
    // Duplicate es "create con campos heredados" — misma regla de autorización.
    require_create_permission(caller.role, &caller_workspaces, &workspace_oids)?;

    let now = DateTime::now();
    let doc = WaQuickReply {
        id: None,
        title,
        content: original.content.clone(),
        workspace_ids: workspace_oids,
        created_by: claims.id.clone(),
        created_by_name: claims.name.clone(),
        created_at: now,
        updated_at: now,
        // La copia nace activa, con use_count en 0. El resto de campos
        // interactivos se heredan tal cual del original.
        active: true,
        header: original.header.clone(),
        footer: original.footer.clone(),
        buttons: original.buttons.clone(),
        list: original.list.clone(),
        cta_url: original.cta_url.clone(),
        use_count: 0,
        last_used_at: None,
    };

    let saved = state
        .db
        .create_quick_reply(doc)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(saved, &caller, &caller_workspaces),
    }))
}

// ============================================
// HELPERS INTERNOS
// ============================================

/// Verifica la firma `X-Hub-Signature-256` de Meta: `sha256=<hex>` sobre el body crudo.
fn verify_meta_signature(app_secret: &[u8], body: &[u8], header_val: &str) -> bool {
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

/// Secreto AES para cifrar `WaSettings.access_token` en reposo.
/// Reutilizamos `JWT_SECRET` — alta entropía y estrictamente privado del backend.
fn settings_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

/// Valida un access_token de Meta. Un token legítimo es un string continuo
/// base64url-ish sin espacios ni comillas. Cualquier carácter extraño suele
/// indicar copy-paste con varias variables (ej: pegar una línea de `.env`).
fn validate_access_token(raw: &str) -> Result<&str, ApiError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(ApiError::BadRequest("access_token requerido".into()));
    }
    if t.chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    {
        return Err(ApiError::BadRequest(
            "access_token inválido: contiene espacios o comillas".into(),
        ));
    }
    Ok(t)
}

/// Resuelve el `WhatsAppService` para el `business_phone` de una conversación:
/// busca `WaSettings`, descifra el `access_token` y construye el cliente.
async fn resolve_service_for_phone(
    state: &Arc<AppState>,
    business_phone: &str,
) -> Result<WhatsAppService, ApiError> {
    let settings = state
        .db
        .find_wa_settings_by_phone(business_phone)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "wa_settings inactivo o no encontrado para {}",
                business_phone
            ))
        })?;

    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        return Err(ApiError::Internal(
            "wa_settings sin phone_number_id o access_token configurados".into(),
        ));
    }

    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;

    let svc = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id,
        token,
    );
    Ok(apply_media_relay(&state, svc))
}

/// Avisa al cliente que su archivo no llegó cuando Meta reporta un fallo de
/// media inbound (131052/131053/131056). Mejor un mensaje pidiendo reenvío
/// que dejar al cliente esperando respuesta sobre un comprobante que nunca
/// existió en nuestro sistema.
///
/// Best-effort: si falla cualquier paso (settings, decrypt, send) sólo
/// loguea WARN y retorna. No re-intenta — un mensaje fallido de este tipo
/// no justifica complejidad de retry.
async fn notify_inbound_media_failure(
    state: &Arc<AppState>,
    recipient_phone: &str,
    business_phone: &str,
) {
    let settings = match state.db.find_wa_settings_by_phone(business_phone).await {
        Ok(Some(s)) => s,
        _ => {
            tracing::warn!(
                "[webhook] inbound_media_failure: WaSettings no encontrado para business='{}'",
                business_phone
            );
            return;
        }
    };
    let token = match decrypt_payload(&settings_secret(), &settings.access_token) {
        Some(t) => t,
        None => {
            tracing::warn!(
                "[webhook] inbound_media_failure: decrypt_payload falló (business='{}')",
                business_phone
            );
            return;
        }
    };
    let svc = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id.clone(),
        token,
    );
    let svc = apply_media_relay(state, svc);
    let body = "No pude leer el archivo que enviaste. ¿Podrías reenviarlo como *Foto* \
                (no como Documento)? Si preferís, también podés escribirme los datos \
                del pago: monto, banco origen, referencia y fecha.";
    match svc.send_text(recipient_phone, body, None, false).await {
        Ok(wamid) => tracing::info!(
            "[webhook] inbound_media_failure: fallback enviado a '{}' (wamid={})",
            recipient_phone,
            wamid
        ),
        Err(e) => tracing::warn!(
            "[webhook] inbound_media_failure: send_text falló para '{}': {}",
            recipient_phone,
            e
        ),
    }
}

/// Aplica el relay de Cloudflare al service si ambas env vars están seteadas
/// en el Config. No-op cuando no hay relay configurado (dev, o red arreglada).
fn apply_media_relay(state: &Arc<AppState>, svc: WhatsAppService) -> WhatsAppService {
    match (
        state.config.relay_url.as_ref(),
        state.config.relay_secret.as_ref(),
    ) {
        (Some(url), Some(secret)) => svc.with_media_relay(super::service::MediaRelay {
            url: url.clone(),
            secret: secret.clone(),
        }),
        _ => svc,
    }
}

/// Tipos de mensaje que se prefetchean al llegar por webhook.
/// Todos los tipos con media están incluidos — documentos también, pero el
/// límite de 5 MB (`MEDIA_CACHE_MAX_BYTES`) deja fuera los PDFs pesados.
fn should_prefetch_media(msg_type: &str) -> bool {
    matches!(
        msg_type,
        "audio" | "image" | "sticker" | "video" | "document"
    )
}

/// Guard que libera el lock de prefetch al salir de `prefetch_media`,
/// haya terminado bien o mal. Evita que un panic o un early-return deje
/// un lock huérfano en Redis (el TTL de 60s lo limpiaría igual, pero
/// así lo liberamos apenas se puede).
struct MediaPrefetchGuard {
    redis: crate::cache::RedisClient,
    media_id: String,
}

impl Drop for MediaPrefetchGuard {
    fn drop(&mut self) {
        let redis = self.redis.clone();
        let media_id = self.media_id.clone();
        tokio::spawn(async move {
            redis.release_media_prefetch_lock(&media_id).await;
        });
    }
}

/// Descarga un media de Meta y lo guarda en Redis si pesa poco.
/// Fire-and-forget: se spawnea desde el webhook apenas llega el mensaje,
/// para que cuando el agente abra el chat el `GET /media/:id` encuentre
/// hit en Redis y responda en milisegundos.
pub(crate) async fn prefetch_media(state: Arc<AppState>, business_phone: String, media_id: String) {
    // Skip si ya está cacheado (puede pasar si el mismo media llega dos veces).
    if state.redis.get_media_cache(&media_id).await.is_some() {
        return;
    }

    // Lock para evitar descarga duplicada: si el endpoint ya está bajando
    // este media (race con el agente que abre el chat al instante), lo dejamos.
    if !state.redis.try_lock_media_prefetch(&media_id).await {
        tracing::debug!(
            "prefetch_media({}): ya hay otra tarea descargándolo",
            media_id
        );
        return;
    }
    // RAII manual: liberamos el lock al final.
    let _guard = MediaPrefetchGuard {
        redis: state.redis.clone(),
        media_id: media_id.clone(),
    };

    let wa = match resolve_service_for_phone(&state, &business_phone).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                "prefetch_media({}): no pude resolver service: {:?}",
                media_id,
                e
            );
            return;
        }
    };

    let info = match wa.download_media_info(&media_id).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("prefetch_media({}): info falló: {:?}", media_id, e);
            return;
        }
    };

    // Si Meta reporta tamaño y supera el límite, no cacheamos — lo bajará el
    // endpoint si el agente abre el media.
    if let Some(size) = info.file_size {
        if size > MEDIA_CACHE_MAX_BYTES as u64 {
            tracing::debug!(
                "prefetch_media({}): skip ({} bytes > {} max)",
                media_id,
                size,
                MEDIA_CACHE_MAX_BYTES
            );
            return;
        }
    }

    let bytes = match wa.download_media_body(&info.url).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("prefetch_media({}): body falló: {:?}", media_id, e);
            return;
        }
    };

    // Guard tardío: si Meta no reportó `file_size` y el binario terminó siendo
    // grande, igual respetamos el límite.
    if bytes.len() > MEDIA_CACHE_MAX_BYTES {
        return;
    }

    state
        .redis
        .set_media_cache(&media_id, &bytes, &info.mime, info.file_name.as_deref())
        .await;
    tracing::info!(
        "prefetch_media({}): cacheado {} bytes ({})",
        media_id,
        bytes.len(),
        info.mime
    );
}

/// Normaliza cualquier formato de número venezolano a E.164 sin "+" (ej: "584141234567")
fn normalize_to_e164(phone: &str) -> String {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.starts_with("58") {
        digits
    } else if let Some(rest) = digits.strip_prefix('0') {
        format!("58{}", rest)
    } else {
        format!("58{}", digits)
    }
}

// ============================================
// HELPERS DE MAPEO
// ============================================

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
    let (can_send_freeform, expires_iso) = compute_freeform_state(c.last_inbound_at);
    let (meta_throttled, meta_throttle_until_iso) =
        compute_meta_throttle_state(c.meta_throttle_until);
    // Prioridad: DB (Clients.sName) → WhatsApp profile (c.name) → null
    let customer_name = resolved_name.filter(|s| !s.trim().is_empty()).or(c.name);
    ConversationItem {
        id: c.id.map(|o| o.to_hex()).unwrap_or_default(),
        customer_phone: c.phone,
        customer_name,
        business_phone: c.business_phone,
        workspace_name,
        status: c.status,
        assigned_to: c.assigned_to,
        assigned_to_name,
        last_message_at: iso8601(c.last_message_at),
        last_message_preview: c.last_message_preview,
        last_message_type: c.last_message_type,
        last_message_direction: c.last_message_direction,
        last_message_status: c.last_message_status,
        last_message_media_filename: c.last_message_media_filename,
        last_message_from_user_id: c.last_message_from_user_id,
        last_message_from_user_name,
        unread_count: c.unread_count,
        created_at: iso8601(c.created_at),
        client_id: if include_client_id {
            c.client_id.map(|o| o.to_hex())
        } else {
            None
        },
        last_opened_at: last_opened_at.map(iso8601),
        last_inbound_at: c.last_inbound_at.map(iso8601),
        can_send_freeform,
        freeform_expires_at: expires_iso,
        meta_throttled,
        meta_throttle_until: meta_throttle_until_iso,
        ai_active_agent_id: c.ai_active_agent_id.map(|o| o.to_hex()),
        ai_disabled: c.ai_disabled,
        ai_last_processed_at: c.ai_last_processed_at.map(iso8601),
        ai_conv_state: c.ai_conv_state,
    }
}

/// Devuelve `(meta_throttled, meta_throttle_until_iso)`. Si el cooldown ya
/// expiró, devuelve `(false, None)` — un campo seteado en el pasado no debe
/// confundir al front.
fn compute_meta_throttle_state(until: Option<DateTime>) -> (bool, Option<String>) {
    match until {
        Some(t) => {
            let now_ms = DateTime::now().timestamp_millis();
            if t.timestamp_millis() > now_ms {
                (true, Some(iso8601(t)))
            } else {
                (false, None)
            }
        }
        None => (false, None),
    }
}

/// Resuelve el nombre del contacto para una conversación contra `Clients`:
/// si tiene `client_id` linkeado lo usa; si no, intenta por teléfono. Devuelve
/// `None` cuando no matchea en DB — el caller cae a `WaConversation.name`.
async fn resolve_customer_name(state: &Arc<AppState>, conv: &WaConversation) -> Option<String> {
    use crate::db::ProfileRepository;
    if let Some(cid) = conv.client_id {
        let map = state.db.get_client_names_by_ids(&[cid]).await.ok()?;
        if let Some(n) = map.get(&cid).cloned() {
            return Some(n);
        }
    }
    let map = state
        .db
        .get_client_names_by_phones(&[conv.phone.clone()])
        .await
        .ok()?;
    map.get(&conv.phone).cloned()
}

/// Ventana de 24h desde `last_inbound_at`. Usado por el gate de envío freeform,
/// por `conv_to_item` y por el WS event `CONVERSACION_ESTADO`.
pub(super) fn is_within_24h(last_inbound_at: Option<DateTime>) -> bool {
    match last_inbound_at {
        Some(t) => {
            let now = DateTime::now().timestamp_millis();
            let then = t.timestamp_millis();
            (now - then) <= 24 * 60 * 60 * 1000
        }
        None => false,
    }
}

/// Devuelve `(can_send_freeform, freeform_expires_at_iso)`.
fn compute_freeform_state(last_inbound_at: Option<DateTime>) -> (bool, Option<String>) {
    match last_inbound_at {
        Some(t) => {
            let expires = DateTime::from_millis(t.timestamp_millis() + 24 * 60 * 60 * 1000);
            (is_within_24h(Some(t)), Some(iso8601(expires)))
        }
        None => (false, None),
    }
}

/// Atajo para handlers que tocan una sola conversación: resuelve `workspace_name`
/// por su `business_phone` vía `WaSettings`.
async fn resolve_workspace_name(state: &Arc<AppState>, business_phone: &str) -> Option<String> {
    if business_phone.is_empty() {
        return None;
    }
    state
        .db
        .get_workspace_names(&[business_phone.to_string()])
        .await
        .ok()
        .and_then(|m| m.get(business_phone).cloned())
}

fn settings_to_item(s: WaSettings) -> SettingsItem {
    SettingsItem {
        id: s.id.map(|o| o.to_hex()).unwrap_or_default(),
        phone: s.phone,
        workspace_name: s.workspace_name,
        phone_number_id: s.phone_number_id,
        whatsapp_business_account_id: s.whatsapp_business_account_id,
        has_access_token: !s.access_token.is_empty(),
        agents: s.agents,
        active: s.active,
        purposes: s.purposes,
        enable_guardrails: s.enable_guardrails,
        enable_conversation_state: s.enable_conversation_state,
        pre_classifier_enabled: s.pre_classifier_enabled,
        trivial_responses: s.trivial_responses,
        templates_synced_at: s.templates_synced_at.map(iso8601),
        created_at: iso8601(s.created_at),
        updated_at: iso8601(s.updated_at),
    }
}

fn msg_to_item(
    m: WaMessage,
    from_user_name: Option<String>,
    reply_to: Option<ReplyToItem>,
) -> MessageItem {
    MessageItem {
        id: m.id.map(|o| o.to_hex()).unwrap_or_default(),
        conversation_id: m.conversation_id.to_hex(),
        wa_message_id: m.wa_message_id,
        direction: m.direction,
        msg_type: m.msg_type,
        content: m.body,
        media_id: m.media_id,
        media_mime_type: m.media_mime_type,
        media_filename: m.media_filename,
        status: m.status,
        from_user_id: m.sent_by,
        from_user_name,
        idempotency_key: m.idempotency_key,
        reply_to,
        url_preview: m.url_preview,
        voice: m.voice,
        template_name: m.template_name,
        template_language: m.template_language,
        template_components: m.template_components,
        interactive_payload: m.interactive_payload,
        contacts_payload: m.contacts_payload,
        location: m.location,
        reactions: m.reactions,
        ai_processed_at: m.ai_processed_at.map(iso8601),
        created_at: iso8601(m.timestamp),
    }
}

/// Atajo usado por jobs async (`url_preview`, `ai_agent::dispatch`) para armar
/// un `MessageItem` completo a partir de un `WaMessage` recién releído:
/// resuelve `sent_by_name` y `reply_to` en un solo call. Costo: 1-2 queries
/// a `Users` / `WaMessages`.
pub async fn build_message_item(state: &Arc<AppState>, m: WaMessage) -> MessageItem {
    use crate::db::UserRepository;
    let name = match m.sent_by.as_deref() {
        Some(id) => state
            .db
            .find_user_by_id(id)
            .await
            .ok()
            .flatten()
            .map(|u| u.name),
        None => None,
    };
    let reply_to = resolve_reply_to_for_one(state, &m).await;
    msg_to_item(m, name, reply_to)
}

/// Trunca el cuerpo del mensaje citado a ~80 chars (seguro en UTF-8).
/// Se usa sólo para preview en la UI; el mensaje original completo sigue
/// disponible por su `wa_message_id`.
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
    let wid = m.reply_to_wa_message_id.as_ref()?;
    let items = resolve_reply_to_items(state, std::slice::from_ref(m)).await;
    items.get(wid).cloned()
}

/// Batch-resuelve los `reply_to` de un conjunto de mensajes en un solo query a
/// `WaMessages` (+ uno a `Users` para los nombres de agentes).
///
/// Devuelve un mapa `wa_message_id citado → ReplyToItem` listo para armar el
/// `MessageItem`. Mensajes cuyo `reply_to_wa_message_id` no existe en DB
/// (ej. mensajes anteriores al deploy del feature) quedan fuera del mapa y
/// el front recibirá `reply_to: null`.
async fn resolve_reply_to_items(
    state: &Arc<AppState>,
    messages: &[WaMessage],
) -> std::collections::HashMap<String, ReplyToItem> {
    use crate::db::UserRepository;

    // Recolecto los wamid citados, dedup.
    let mut wa_ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.reply_to_wa_message_id.clone())
        .collect();
    wa_ids.sort();
    wa_ids.dedup();
    if wa_ids.is_empty() {
        return std::collections::HashMap::new();
    }

    // Batch lookup del mensaje original.
    let originals = match state.db.find_messages_by_wa_ids(&wa_ids).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "resolve_reply_to_items find_messages_by_wa_ids error: {}",
                e
            );
            return std::collections::HashMap::new();
        }
    };

    // Nombres de agentes para los originales outbound — un batch sobre Users.
    let mut sender_ids: Vec<String> = originals
        .values()
        .filter_map(|m| m.sent_by.clone())
        .collect();
    sender_ids.sort();
    sender_ids.dedup();
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for id in sender_ids {
        if let Ok(Some(u)) = state.db.find_user_by_id(&id).await {
            names.insert(id, u.name);
        }
    }

    // Ensamblar ReplyToItems.
    originals
        .into_iter()
        .map(|(wa_id, m)| {
            let preview_content = m
                .body
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| preview_truncate(s, 80));
            let from_user_name = m.sent_by.as_deref().and_then(|id| names.get(id).cloned());
            let item = ReplyToItem {
                wa_message_id: wa_id.clone(),
                preview_content,
                preview_type: m.msg_type,
                direction: m.direction,
                from_user_name,
            };
            (wa_id, item)
        })
        .collect()
}

/// Convierte un timestamp de Meta (Unix seconds en string) a `bson::DateTime`.
fn parse_unix_seconds_to_bson(s: &str) -> Option<DateTime> {
    let secs: i64 = s.parse().ok()?;
    Some(DateTime::from_millis(secs.checked_mul(1000)?))
}

/// Resuelve nombres de agentes para un batch de mensajes, deduplicando UUIDs
/// y leyendo de `Users` en paralelo.
async fn resolve_sent_by_names(
    state: &Arc<AppState>,
    messages: &[WaMessage],
) -> std::collections::HashMap<String, String> {
    use crate::db::UserRepository;

    let mut ids: Vec<String> = messages.iter().filter_map(|m| m.sent_by.clone()).collect();
    ids.sort();
    ids.dedup();

    let mut out = std::collections::HashMap::new();
    for id in ids {
        if let Ok(Some(u)) = state.db.find_user_by_id(&id).await {
            out.insert(id, u.name);
        }
    }
    out
}

/// Batch-resolución de nombres de agentes para listados de conversaciones,
/// a partir de `last_message_from_user_id`. Dedup + 1 lookup por UUID único.
async fn resolve_last_message_agent_names(
    state: &Arc<AppState>,
    convs: &[WaConversation],
) -> std::collections::HashMap<String, String> {
    use crate::db::UserRepository;

    let mut ids: Vec<String> = convs
        .iter()
        .filter_map(|c| c.last_message_from_user_id.clone())
        .collect();
    ids.sort();
    ids.dedup();

    let mut out = std::collections::HashMap::new();
    for id in ids {
        if let Ok(Some(u)) = state.db.find_user_by_id(&id).await {
            out.insert(id, u.name);
        }
    }
    out
}

/// Resuelve el nombre del agente del último mensaje de una conversación
/// puntual (detalle). Devuelve `None` si no hay autor o no se encuentra.
async fn resolve_last_message_agent_name_one(
    state: &Arc<AppState>,
    conv: &WaConversation,
) -> Option<String> {
    use crate::db::UserRepository;
    let id = conv.last_message_from_user_id.as_deref()?;
    state
        .db
        .find_user_by_id(id)
        .await
        .ok()
        .flatten()
        .map(|u| u.name)
}

/// Batch-resolución de nombres de agentes asignados (`assigned_to`) para
/// listados. Mismo patrón que `resolve_last_message_agent_names`.
async fn resolve_assigned_agent_names(
    state: &Arc<AppState>,
    convs: &[WaConversation],
) -> std::collections::HashMap<String, String> {
    use crate::db::UserRepository;

    let mut ids: Vec<String> = convs.iter().filter_map(|c| c.assigned_to.clone()).collect();
    ids.sort();
    ids.dedup();

    let mut out = std::collections::HashMap::new();
    for id in ids {
        if let Ok(Some(u)) = state.db.find_user_by_id(&id).await {
            out.insert(id, u.name);
        }
    }
    out
}

/// Resuelve el nombre del agente asignado de una conversación puntual.
/// Devuelve `None` si no hay asignado o el usuario no existe.
async fn resolve_assigned_agent_name_one(
    state: &Arc<AppState>,
    conv: &WaConversation,
) -> Option<String> {
    use crate::db::UserRepository;
    let id = conv.assigned_to.as_deref()?;
    state
        .db
        .find_user_by_id(id)
        .await
        .ok()
        .flatten()
        .map(|u| u.name)
}

/// Resuelve el nombre de un único user_id (UUID). Útil para los eventos WS
/// que necesitan inyectar el nombre del actor (CHAT_TOMADO -> taken_by_name).
async fn resolve_user_name_by_id(state: &Arc<AppState>, user_id: &str) -> Option<String> {
    use crate::db::UserRepository;
    if user_id.trim().is_empty() {
        return None;
    }
    state
        .db
        .find_user_by_id(user_id)
        .await
        .ok()
        .flatten()
        .map(|u| u.name)
}

// ============================================
// HELPERS — QUICK REPLIES
// ============================================

/// Exige `bCanChat == true` (o `nRole == 0`, super admin) y devuelve el
/// `User` completo (para que el caller tenga el rol sin re-consultar DB).
/// El `user_jwt_auth_middleware` solo valida que el token sea de staff; el
/// permiso de chat vive en `Users`.
///
/// Los super admins (`nRole == 0.0`) bypasean el gate de `bCanChat` —
/// "super admin = acceso a todo" es regla transversal del sistema, así que
/// no se les debe negar el módulo de WhatsApp aunque tengan `bCanChat=false`.
///
/// Los call sites que sólo necesitan el gate escriben
/// `require_can_chat(&state, &claims.id).await?;` y el valor se descarta. Los
/// que además necesitan el rol (`can_edit`, auditoría, etc.) lo capturan con
/// `let caller = require_can_chat(...).await?;`.
pub(super) async fn require_can_chat(
    state: &Arc<AppState>,
    user_id: &str,
) -> Result<crate::models::users::User, ApiError> {
    use crate::db::UserRepository;
    let user = state
        .db
        .find_user_by_id(user_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::Forbidden)?;
    if user.role != 0.0 && !user.can_chat {
        return Err(ApiError::Forbidden);
    }
    Ok(user)
}

/// Construye un `ConversationItem` completo desde un `WaConversation` resolviendo
/// workspace_name + nombres en una sola pasada. Reusable desde otros módulos
/// del feature (tickets) sin tener que reexportar todos los helpers internos.
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

pub(super) fn iso8601_pub(dt: DateTime) -> String {
    iso8601(dt)
}

/// Regla de `can_edit` (controla el botón de **eliminar** una quick reply):
///
/// - `true` si el caller es superadmin (`nRole == 0`).
/// - `true` si el caller es agente de **al menos uno** de los workspaces del
///   item (`overlap`).
/// - `false` en cualquier otro caso.
///
/// Cualquier `can_chat=true` puede ver/usar/editar/toggle — las únicas
/// operaciones con gate de workspace son crear y eliminar. Este helper cubre
/// la regla de eliminar; para crear se usa `require_create_permission`.
fn compute_can_edit(
    caller_role: f32,
    caller_workspaces: &[ObjectId],
    qr_workspace_ids: &[ObjectId],
) -> bool {
    if caller_role == 0.0 {
        return true;
    }
    qr_workspace_ids
        .iter()
        .any(|w| caller_workspaces.contains(w))
}

/// Gate para crear (y duplicate). El caller debe ser superadmin, o agente en
/// **todos** los `target_workspaces`. Devuelve `403 forbidden` si no cumple.
fn require_create_permission(
    caller_role: f32,
    caller_workspaces: &[ObjectId],
    target_workspaces: &[ObjectId],
) -> Result<(), ApiError> {
    if caller_role == 0.0 {
        return Ok(());
    }
    for w in target_workspaces {
        if !caller_workspaces.contains(w) {
            return Err(ApiError::Forbidden);
        }
    }
    Ok(())
}

/// Parsea `workspace_ids` de hex → ObjectId y valida mínimo 1 + existencia
/// en `WaSettings`. **No valida membresía del caller** — eso lo resuelve el
/// handler con `require_create_permission` cuando corresponda.
async fn parse_and_validate_workspaces(
    state: &Arc<AppState>,
    raw: &[String],
) -> Result<Vec<ObjectId>, ApiError> {
    if raw.is_empty() {
        return Err(ApiError::BadRequest(
            "workspace_ids requiere al menos 1".into(),
        ));
    }
    let mut oids = Vec::with_capacity(raw.len());
    for s in raw {
        let oid = ObjectId::parse_str(s)
            .map_err(|_| ApiError::BadRequest(format!("workspace_id inválido: {}", s)))?;
        oids.push(oid);
    }
    oids.sort();
    oids.dedup();

    if !state
        .db
        .wa_settings_exist(&oids)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        return Err(ApiError::BadRequest("algún workspace_id no existe".into()));
    }
    Ok(oids)
}

fn quick_reply_to_item(
    q: WaQuickReply,
    caller: &crate::models::users::User,
    caller_workspaces: &[ObjectId],
) -> QuickReplyItem {
    let can_edit = compute_can_edit(caller.role, caller_workspaces, &q.workspace_ids);
    QuickReplyItem {
        id: q.id.map(|o| o.to_hex()).unwrap_or_default(),
        title: q.title,
        content: q.content,
        workspace_ids: q.workspace_ids.into_iter().map(|o| o.to_hex()).collect(),
        created_by: q.created_by,
        created_by_name: q.created_by_name,
        created_at: iso8601(q.created_at),
        updated_at: iso8601(q.updated_at),
        active: q.active,
        can_edit,
        header: q.header,
        footer: q.footer,
        buttons: q.buttons,
        list: q.list,
        cta_url: q.cta_url,
        use_count: q.use_count,
        last_used_at: q.last_used_at.map(iso8601),
    }
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

// ============================================
// TEMPLATES CRUD (WaTemplates — DB local)
// ============================================

// ---------------------------------------------------------------------------
// Helpers compartidos
// ---------------------------------------------------------------------------

/// Convierte un `WaTemplate` de DB al shape de response `WaTemplateItem`.
fn to_template_item(t: WaTemplate) -> WaTemplateItem {
    WaTemplateItem {
        id: t.id.map(|o| o.to_hex()).unwrap_or_default(),
        phone_number_id: t.phone_number_id,
        name: t.name,
        display_name: t.display_name,
        name_input: t.name_input,
        language: t.language,
        category: t.category,
        components: t.components,
        body_placeholders: t.body_placeholders,
        status: t.status,
        rejection_reason: t.rejection_reason,
        meta_template_id: t.meta_template_id,
        is_system: t.is_system,
        submit_to_meta: t.submit_to_meta,
        created_by: t.created_by,
        created_by_name: t.created_by_name,
        created_at: iso8601(t.created_at),
        updated_at: iso8601(t.updated_at),
    }
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
fn generate_template_name(name_input: &str, is_system: bool) -> String {
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
fn flat_to_components(
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

fn validate_components(comps: &[serde_json::Value]) -> Result<u32, ApiError> {
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
fn map_meta_error(err: &anyhow::Error, default_msg: &str) -> ApiError {
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
    use crate::db::UserRepository;
    let user = state
        .db
        .find_user_by_id(user_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::Forbidden)?;
    if user.role != 0.0 {
        return Err(ApiError::Forbidden);
    }
    Ok(user)
}

/// Error canónico para plantilla no encontrada (404).
fn template_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "template_not_found",
        "Plantilla no encontrada",
    )
}

// ---------------------------------------------------------------------------
// POST /v1/auth-user/whatsapp/templates
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct TemplatesListQuery {
    pub phone_number_id: String,
    pub status: Option<String>,
    pub category: Option<String>,
    pub only_system: Option<bool>,
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/templates",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    request_body = CreateWaTemplateRequest,
    responses(
        (status = 200, description = "Plantilla creada", body = WaTemplateResponse),
        (status = 400, description = "Datos inválidos (name_required, name_invalid, invalid_component)"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "phone_number_not_found"),
        (status = 409, description = "name_already_exists"),
        (status = 502, description = "meta_rejected"),
    )
)]
pub async fn create_template_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(body): Json<CreateWaTemplateRequest>,
) -> Result<Json<WaTemplateResponse>, ApiError> {
    // Auth: bCanChat (superadmin bypass implícito en require_can_chat)
    let creator = require_can_chat(&state, &claims.id).await?;

    // 1. Validar name_input no vacío
    if body.name_input.trim().is_empty() {
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "name_required",
            "name_input",
            "El nombre es requerido",
        ));
    }

    // 2. Resolver WaSettings por phone_number_id
    let settings = state
        .db
        .find_wa_settings_by_phone_number_id(&body.phone_number_id)
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

    // 3. Generar `name`
    let name = generate_template_name(&body.name_input, body.is_system);

    // 4. Validar name contra regex Meta
    {
        let re = regex::Regex::new(r"^[a-z][a-z0-9_]{0,511}$").expect("regex válido");
        if !re.is_match(&name) {
            return Err(ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "name_invalid",
                "name_input",
                "Nombre inválido. Usa solo letras minúsculas, números y guión bajo (debe empezar con letra)",
            ));
        }
    }

    // 5. Construir components desde los flat fields y validar
    let components = flat_to_components(
        body.header.as_ref(),
        &body.body,
        body.body_samples.as_ref(),
        body.footer.as_deref(),
        body.buttons.as_ref(),
    );
    let body_placeholders = validate_components(&components)?;

    // 7. Resolver created_by_name (ya tenemos creator del paso de auth)
    let created_by_name = creator.name.clone();

    // 8. Verificar unicidad (phone_number_id, name, language)
    let existing = state
        .db
        .find_template_by_phone_name_lang(&body.phone_number_id, &name, &body.language)
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

    let now = DateTime::now();
    let mut status = WaTemplateStatus::Draft;
    let mut meta_template_id: Option<String> = None;

    // 9. Si submit_to_meta == true: crear en Meta
    if body.submit_to_meta {
        if settings.access_token.is_empty() {
            return Err(ApiError::Internal(
                "workspace sin access_token configurado".into(),
            ));
        }
        let token = decrypt_payload(&settings_secret(), &settings.access_token)
            .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;

        let waba_id = settings.whatsapp_business_account_id.trim().to_string();
        if waba_id.is_empty() {
            return Err(ApiError::Internal(
                "workspace sin whatsapp_business_account_id configurado".into(),
            ));
        }

        let category_str = match body.category {
            WaTemplateCategory::Marketing => "MARKETING",
            WaTemplateCategory::Utility => "UTILITY",
            WaTemplateCategory::Authentication => "AUTHENTICATION",
        };
        // Clonar components y swapear header media_id → handle Meta fresh.
        // El original (sin swap) se mantiene para persistir en DB, así que el
        // handle Meta (single-use, corto) nunca se guarda.
        let mut components_for_meta = components.clone();
        swap_header_handles_in_components(&state, &mut components_for_meta, &token).await?;
        let components_val = serde_json::Value::Array(components_for_meta);

        let wa = WhatsAppService::new(
            state.reqwest_client.clone(),
            settings.phone_number_id.clone(),
            token,
        );

        match wa
            .create_template_meta(
                &waba_id,
                &name,
                &body.language,
                category_str,
                &components_val,
            )
            .await
        {
            Ok(resp) => {
                status = WaTemplateStatus::Pending;
                meta_template_id = Some(resp.id);
            }
            Err(e) => {
                return Err(map_meta_error(&e, "Meta rechazó la plantilla"));
            }
        }
    }

    // 11. Insertar en DB
    let doc = WaTemplate {
        id: None,
        phone_number_id: body.phone_number_id.clone(),
        name: name.clone(),
        display_name: body.name_input.clone(),
        name_input: body.name_input.clone(),
        language: body.language.clone(),
        category: body.category,
        components,
        body_placeholders,
        status,
        rejection_reason: None,
        meta_template_id,
        is_system: body.is_system,
        submit_to_meta: body.submit_to_meta,
        created_by: claims.id.clone(),
        created_by_name,
        created_at: now,
        updated_at: now,
    };

    let saved = state.db.create_template(doc).await.map_err(|e| {
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
    })?;

    // 12. Construir item
    let item = to_template_item(saved);

    // 13. Emit WS
    let ws_payload = build_template_created_event(&item);
    emit_to_phone_number_agents(&state, &body.phone_number_id, ws_payload).await;

    Ok(Json(WaTemplateResponse {
        ok: true,
        data: item,
    }))
}

// ---------------------------------------------------------------------------
// GET /v1/auth-user/whatsapp/templates (reemplaza el anterior)
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/templates",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(
        ("phone_number_id" = String, Query, description = "phone_number_id del workspace (requerido)"),
        ("status" = Option<String>, Query, description = "Filtrar por status(es) separados por coma"),
        ("category" = Option<String>, Query, description = "MARKETING | UTILITY | AUTHENTICATION"),
        ("only_system" = Option<bool>, Query, description = "Si true, sólo plantillas del sistema"),
        ("search" = Option<String>, Query, description = "Búsqueda substring en display_name y name"),
        ("limit" = Option<i64>, Query, description = "Default 50, máx 100"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco de paginación"),
    ),
    responses(
        (status = 200, description = "Lista de plantillas", body = WaTemplatesListResponse),
        (status = 400, description = "Parámetros inválidos"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "phone_number_not_found"),
    )
)]
pub async fn list_templates_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<TemplatesListQuery>,
) -> Result<Json<WaTemplatesListResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let phone_number_id = q.phone_number_id.trim().to_string();
    if phone_number_id.is_empty() {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "phone_number_id es requerido",
        ));
    }

    // Verificar que el WaSettings existe
    state
        .db
        .find_wa_settings_by_phone_number_id(&phone_number_id)
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

    // Parsear status CSV
    let status_vec: Option<Vec<WaTemplateStatus>> = if let Some(s) = &q.status {
        let mut parsed = Vec::new();
        for part in s.split(',') {
            let trimmed = part.trim();
            let st = match trimmed.to_uppercase().as_str() {
                "DRAFT" => WaTemplateStatus::Draft,
                "PENDING" => WaTemplateStatus::Pending,
                "APPROVED" => WaTemplateStatus::Approved,
                "REJECTED" => WaTemplateStatus::Rejected,
                "PAUSED" => WaTemplateStatus::Paused,
                "DISABLED" => WaTemplateStatus::Disabled,
                _ => {
                    return Err(ApiError::domain_simple(
                        StatusCode::BAD_REQUEST,
                        "invalid_query",
                        "Status inválido",
                    ));
                }
            };
            parsed.push(st);
        }
        if parsed.is_empty() {
            None
        } else {
            Some(parsed)
        }
    } else {
        None
    };

    // Parsear category
    let category_filter: Option<WaTemplateCategory> = if let Some(c) = &q.category {
        Some(match c.trim().to_uppercase().as_str() {
            "MARKETING" => WaTemplateCategory::Marketing,
            "UTILITY" => WaTemplateCategory::Utility,
            "AUTHENTICATION" => WaTemplateCategory::Authentication,
            _ => {
                return Err(ApiError::domain_simple(
                    StatusCode::BAD_REQUEST,
                    "invalid_query",
                    "Categoría inválida",
                ));
            }
        })
    } else {
        None
    };

    let limit = q.limit.unwrap_or(50).clamp(1, 100);

    let filter = WaTemplateListFilter {
        phone_number_id: &phone_number_id,
        status: status_vec.as_deref(),
        category: category_filter,
        only_system: q.only_system.unwrap_or(false),
        search: q.search.as_deref(),
        limit,
        cursor: q.cursor.as_deref(),
    };

    let templates = state
        .db
        .list_templates_filtered(filter)
        .await
        .map_err(ApiError::DatabaseError)?;

    let next_cursor = if (templates.len() as i64) < limit {
        None
    } else {
        templates.last().and_then(|t| {
            t.id.map(|id| format!("{}_{}", t.created_at.timestamp_millis(), id.to_hex()))
        })
    };

    let data: Vec<WaTemplateItem> = templates.into_iter().map(to_template_item).collect();

    Ok(Json(WaTemplatesListResponse {
        ok: true,
        data,
        next_cursor,
    }))
}

// ---------------------------------------------------------------------------
// GET /v1/auth-user/whatsapp/templates/:id
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/templates/{id}",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la plantilla")),
    responses(
        (status = 200, description = "Detalle de plantilla", body = WaTemplateResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sólo SUPERADMIN"),
        (status = 404, description = "template_not_found"),
    )
)]
pub async fn get_template_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<WaTemplateResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| template_not_found())?;

    let doc = state
        .db
        .find_template_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(template_not_found)?;

    Ok(Json(WaTemplateResponse {
        ok: true,
        data: to_template_item(doc),
    }))
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
// DELETE /v1/auth-user/whatsapp/templates/:id
// ---------------------------------------------------------------------------

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/templates/{id}",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la plantilla")),
    responses(
        (status = 200, description = "Plantilla eliminada", body = DeleteWaTemplateResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sólo SUPERADMIN"),
        (status = 404, description = "template_not_found"),
        (status = 409, description = "template_in_use_cannot_delete"),
        (status = 502, description = "meta_rejected"),
    )
)]
pub async fn delete_template_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<DeleteWaTemplateResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| template_not_found())?;

    // 1. Cargar doc
    let doc = state
        .db
        .find_template_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(template_not_found)?;

    // 2. Verificar si está en uso en propósitos
    let in_use = state
        .db
        .count_templates_in_purposes(&doc.phone_number_id, &doc.name)
        .await
        .map_err(ApiError::DatabaseError)?;

    if !in_use.is_empty() {
        return Err(ApiError::domain_with_details(
            StatusCode::CONFLICT,
            "template_in_use_cannot_delete",
            "La plantilla está en uso en propósitos del sistema",
            serde_json::json!({ "purposes": in_use }),
        ));
    }

    // 3/4. Si tiene meta_template_id: borrar en Meta
    if let Some(ref meta_id) = doc.meta_template_id {
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

        let wa = WhatsAppService::new(
            state.reqwest_client.clone(),
            settings.phone_number_id.clone(),
            token,
        );

        // 404 se loggea como warn y continúa (ya manejado en service)
        if let Err(e) = wa.delete_template_meta(&waba_id, meta_id, &doc.name).await {
            return Err(map_meta_error(&e, "Meta rechazó el borrado del template"));
        }
    }

    // 5. Borrar en DB
    state
        .db
        .delete_template(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;

    // 6. Emit WS
    let ws_payload = build_template_deleted_event(
        &oid.to_hex(),
        &doc.name,
        &doc.language,
        &doc.phone_number_id,
    );
    emit_to_phone_number_agents(&state, &doc.phone_number_id, ws_payload).await;

    Ok(Json(DeleteWaTemplateResponse {
        ok: true,
        data: DeleteWaTemplateData { id: oid.to_hex() },
    }))
}

// ---------------------------------------------------------------------------
// POST /v1/auth-user/whatsapp/templates/:id/resync
// ---------------------------------------------------------------------------

/// Resync manual del estado de un template desde Meta. Útil cuando se perdió
/// un webhook de status update (subscription apagada, fallo transitorio,
/// payload mal-deserializado, etc). Hace `GET /{meta_template_id}` a Meta,
/// lee el `status` real, actualiza la DB y emite `WA_TEMPLATE_UPDATED` por WS.
#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/templates/{id}/resync",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la plantilla")),
    responses(
        (status = 200, description = "Estado sincronizado desde Meta", body = WaTemplateResponse),
        (status = 400, description = "draft_cannot_resync (la plantilla está en DRAFT)"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sólo SUPERADMIN"),
        (status = 404, description = "template_not_found"),
        (status = 502, description = "meta_rejected (Meta no devolvió el template)"),
    )
)]
pub async fn resync_template_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<WaTemplateResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| template_not_found())?;

    let doc = state
        .db
        .find_template_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(template_not_found)?;

    let meta_id = doc.meta_template_id.as_deref().ok_or_else(|| {
        ApiError::domain_simple(
        StatusCode::BAD_REQUEST,
        "draft_cannot_resync",
        "La plantilla está en DRAFT — todavía no fue enviada a Meta, no hay nada que sincronizar",
    )
    })?;

    // Resolver WaSettings + token
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

    let wa = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id.clone(),
        token,
    );

    // Leer estado real de Meta
    let info = wa
        .get_template_meta(meta_id)
        .await
        .map_err(|e| map_meta_error(&e, "Meta no devolvió el template"))?;

    // Mapear status Meta → WaTemplateStatus (mismo mapping que el webhook)
    let (new_status, rejection_reason): (WaTemplateStatus, Option<String>) =
        match info.status.to_uppercase().as_str() {
            "APPROVED" => (WaTemplateStatus::Approved, None),
            "REJECTED" => (WaTemplateStatus::Rejected, info.rejected_reason),
            "FLAGGED" => (
                WaTemplateStatus::Rejected,
                Some("flagged_by_meta_quality".to_string()),
            ),
            "PAUSED" => (WaTemplateStatus::Paused, info.rejected_reason),
            "DISABLED" => (WaTemplateStatus::Disabled, info.rejected_reason),
            "PENDING" | "IN_REVIEW" | "" => (WaTemplateStatus::Pending, None),
            other => {
                return Err(ApiError::Internal(format!(
                    "Meta devolvió un status desconocido: '{}'",
                    other
                )));
            }
        };

    // Update DB y capturar prev_status
    let result = state
        .db
        .update_template_status(meta_id, new_status, rejection_reason)
        .await
        .map_err(ApiError::DatabaseError)?;

    let (updated_doc, prev_status) = match result {
        Some(t) => t,
        None => {
            // No debería pasar: existe el doc en DB y tiene meta_template_id
            return Err(ApiError::Internal(
                "update_template_status retornó None pese a tener doc en DB".into(),
            ));
        }
    };

    let item = to_template_item(updated_doc);

    // Emit WS sólo si el status efectivamente cambió
    if item.status != prev_status {
        let payload = build_template_updated_event(&item, Some(prev_status));
        emit_to_phone_number_agents(&state, &item.phone_number_id, payload).await;
    }

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

// ============================================
// TEMPLATE HEADER MEDIA (GridFS + Resumable Upload)
// ============================================

/// Límites de mime + tamaño por `format` impuestos por Meta para headers de
/// template. Cualquier cosa fuera de esto rebota client-side antes de llegar
/// a la Resumable Upload API.
fn header_media_limits(format: &str) -> Option<(&'static [&'static str], u64)> {
    match format.to_uppercase().as_str() {
        "IMAGE" => Some((&["image/jpeg", "image/png"], 5 * 1024 * 1024)),
        "VIDEO" => Some((&["video/mp4", "video/3gpp"], 16 * 1024 * 1024)),
        "DOCUMENT" => Some((&["application/pdf"], 100 * 1024 * 1024)),
        _ => None,
    }
}

/// SHA-256 en hex minúsculas. Usado para dedup (`wa_template_media.files` tiene
/// índice único por `(metadata.phone_number_id, metadata.sha256)`).
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Recorre `components` in-place. Para cada HEADER con `format` != TEXT que
/// contenga un `example.header_handle[0]` que sea un ObjectId válido (nuestro
/// `media_id`), fetchea el binario de GridFS, lo sube a Meta via Resumable
/// Upload y reemplaza el ID por el handle `h` que devuelve Meta.
///
/// Si `header_handle[0]` NO parsea como ObjectId, asumimos que es ya un handle
/// Meta (caso de re-uso o test manual) y lo dejamos intacto.
async fn swap_header_handles_in_components(
    state: &Arc<AppState>,
    components: &mut [serde_json::Value],
    access_token: &str,
) -> Result<(), ApiError> {
    let mut needs_swap = false;
    // Primer pase: detectar si hay algo que swapear (ObjectIds nuestros)
    for c in components.iter() {
        let is_header = c
            .get("type")
            .and_then(|v| v.as_str())
            .map(|t| t.eq_ignore_ascii_case("HEADER"))
            .unwrap_or(false);
        if !is_header {
            continue;
        }
        let format = c.get("format").and_then(|v| v.as_str()).unwrap_or("");
        if format.eq_ignore_ascii_case("TEXT") || format.is_empty() {
            continue;
        }
        if let Some(id_str) = c
            .pointer("/example/header_handle/0")
            .and_then(|v| v.as_str())
        {
            if ObjectId::parse_str(id_str).is_ok() {
                needs_swap = true;
                break;
            }
        }
    }
    if !needs_swap {
        return Ok(());
    }

    // Requerido sólo cuando hay algo que subir
    let app_id = state.config.whatsapp_app_id.as_deref().ok_or_else(|| {
        ApiError::domain_simple(
            StatusCode::SERVICE_UNAVAILABLE,
            "app_id_not_configured",
            "El servidor no tiene configurado WHATSAPP_APP_ID; no se puede subir media de header",
        )
    })?;

    let wa = WhatsAppService::new(
        state.reqwest_client.clone(),
        String::new(), // phone_number_id no se usa en upload_to_meta_resumable
        access_token.to_string(),
    );

    for (idx, c) in components.iter_mut().enumerate() {
        let is_header = c
            .get("type")
            .and_then(|v| v.as_str())
            .map(|t| t.eq_ignore_ascii_case("HEADER"))
            .unwrap_or(false);
        if !is_header {
            continue;
        }
        let format = c
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if format.eq_ignore_ascii_case("TEXT") || format.is_empty() {
            continue;
        }

        let id_str = match c
            .pointer("/example/header_handle/0")
            .and_then(|v| v.as_str())
        {
            Some(s) => s.to_string(),
            None => continue,
        };
        let oid = match ObjectId::parse_str(&id_str) {
            Ok(o) => o,
            Err(_) => continue, // no es nuestro media_id — probablemente un handle Meta ya
        };

        // Fetch del binario
        let (bytes, mime) = state.db
            .read_template_media_bytes(&oid)
            .await
            .map_err(ApiError::DatabaseError)?
            .ok_or_else(|| ApiError::domain_with_details(
                StatusCode::BAD_REQUEST,
                "invalid_component",
                "Media de header no encontrada",
                serde_json::json!({ "component_index": idx, "reason": "header_media_not_found" }),
            ))?;

        // Upload-resumable a Meta
        let handle = wa
            .upload_to_meta_resumable(app_id, &mime, &bytes)
            .await
            .map_err(|e| map_meta_error(&e, "Meta rechazó el upload del header media"))?;

        // Swap: example.header_handle[0] = handle
        if let Some(example) = c.get_mut("example") {
            if let Some(arr) = example
                .get_mut("header_handle")
                .and_then(|v| v.as_array_mut())
            {
                if let Some(first) = arr.get_mut(0) {
                    *first = serde_json::Value::String(handle);
                }
            }
        }
    }

    Ok(())
}

/// `POST /v1/auth-user/whatsapp/templates/header-media` — multipart upload.
/// Persiste el binario en GridFS con dedup por SHA-256. El front usa el
/// `media_id` devuelto como `example.header_handle[0]` al crear/editar un
/// template; el swap real a handle Meta ocurre en `create_template_handler` /
/// `update_template_handler` cuando llaman a la Resumable Upload API.
#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/templates/header-media",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    request_body(
        content = String,
        content_type = "multipart/form-data",
        description = "Campos: `file` (binario), `phone_number_id` (string), `format` (IMAGE|VIDEO|DOCUMENT)",
    ),
    responses(
        (status = 200, description = "Media persistida en GridFS", body = HeaderMediaUploadResponse),
        (status = 400, description = "invalid_file_type | invalid_format | file_required | file_empty"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "phone_number_not_found"),
        (status = 413, description = "file_too_large"),
        (status = 503, description = "app_id_not_configured"),
    )
)]
pub async fn upload_template_header_media_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    mut multipart: Multipart,
) -> Result<Json<HeaderMediaUploadResponse>, ApiError> {
    // Auth
    let uploader = require_can_chat(&state, &claims.id).await?;

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_mime: Option<String> = None;
    let mut phone_number_id: Option<String> = None;
    let mut format: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        tracing::error!("[upload_template_header_media] multipart error: {}", e);
        ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "invalid_multipart",
            "Error leyendo el multipart",
        )
    })? {
        match field.name().unwrap_or("") {
            "file" => {
                file_mime = field.content_type().map(|s| s.to_string());
                let data = field.bytes().await.map_err(|_| {
                    ApiError::domain_with_field(
                        StatusCode::BAD_REQUEST,
                        "file_required",
                        "file",
                        "No se pudo leer el archivo adjunto",
                    )
                })?;
                file_bytes = Some(data.to_vec());
            }
            "phone_number_id" => {
                phone_number_id = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| {
                            ApiError::domain_with_field(
                                StatusCode::BAD_REQUEST,
                                "invalid_field",
                                "phone_number_id",
                                "phone_number_id inválido",
                            )
                        })?
                        .trim()
                        .to_string(),
                );
            }
            "format" => {
                format = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| {
                            ApiError::domain_with_field(
                                StatusCode::BAD_REQUEST,
                                "invalid_field",
                                "format",
                                "format inválido",
                            )
                        })?
                        .trim()
                        .to_uppercase(),
                );
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    // Validar fields requeridos
    let bytes = file_bytes.ok_or_else(|| {
        ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "file_required",
            "file",
            "Adjuntá el archivo a subir",
        )
    })?;
    if bytes.is_empty() {
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "file_empty",
            "file",
            "El archivo está vacío",
        ));
    }
    let phone_number_id = phone_number_id.ok_or_else(|| {
        ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "missing_field",
            "phone_number_id",
            "phone_number_id es requerido",
        )
    })?;
    let format = format.ok_or_else(|| {
        ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "missing_field",
            "format",
            "format es requerido",
        )
    })?;

    // Validar format + mime + size
    let (allowed_mimes, max_size) = header_media_limits(&format).ok_or_else(|| {
        ApiError::domain_with_details(
            StatusCode::BAD_REQUEST,
            "invalid_format",
            "Formato no soportado. Usa IMAGE, VIDEO o DOCUMENT",
            serde_json::json!({ "field": "format", "received": format }),
        )
    })?;

    let mime = file_mime
        .as_deref()
        .unwrap_or("application/octet-stream")
        .to_lowercase();
    if !allowed_mimes.iter().any(|m| *m == mime.as_str()) {
        return Err(ApiError::domain_with_details(
            StatusCode::BAD_REQUEST,
            "invalid_file_type",
            "Tipo MIME no permitido para este formato",
            serde_json::json!({ "allowed_mime_types": allowed_mimes, "received": mime }),
        ));
    }
    let size = bytes.len() as u64;
    if size > max_size {
        return Err(ApiError::domain_with_details(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            "El archivo supera el tamaño máximo permitido",
            serde_json::json!({ "max_size": max_size, "actual_size": size }),
        ));
    }

    // Validar que phone_number_id existe
    let _settings = state
        .db
        .find_wa_settings_by_phone_number_id(&phone_number_id)
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

    // SHA-256 + persistencia (con dedup)
    let sha = sha256_hex(&bytes);
    let stored = state
        .db
        .store_template_media(StoreTemplateMediaInput {
            phone_number_id: &phone_number_id,
            format: &format,
            mime_type: &mime,
            sha256: &sha,
            bytes: &bytes,
            uploaded_by: &claims.id,
            uploaded_by_name: &uploader.name,
        })
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(HeaderMediaUploadResponse {
        ok: true,
        data: HeaderMediaUploadData {
            media_id: stored.id.to_hex(),
            mime_type: stored.mime_type,
            file_size: stored.file_size,
            sha256: stored.sha256,
        },
    }))
}

// ============================================
// RESET AI CONVERSATION STATE
// ============================================

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct ResetAiStateResponse {
    pub ok: bool,
    pub conversation_id: String,
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/agent-state/reset",
    tag = "WhatsApp — Conversaciones",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la conversación")),
    responses(
        (status = 200, description = "Estado IA reseteado", body = ResetAiStateResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat y rol supervisor"),
        (status = 404, description = "Conversación no encontrada"),
        (status = 409, description = "dispatch_in_progress — el dispatch está corriendo, reintentá en segundos"),
    )
)]
pub async fn reset_ai_conv_state_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ResetAiStateResponse>, ApiError> {
    // Requiere bCanChat Y rol supervisor (superadmin / operador / contador).
    let caller = require_can_chat(&state, &claims.id).await?;
    if caller.role != 0.0 && caller.role != 0.5 && caller.role != 1.0 {
        return Err(ApiError::Forbidden);
    }

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    // Verificar que la conv existe.
    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // Acquire dispatch lock — DEBE mantenerse durante todo el reset (DB write + audit + WS)
    // para evitar que un dispatch concurrente sobrescriba el estado que estamos limpiando.
    if !state.redis.try_lock_ai_dispatch(&id).await {
        return Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "dispatch_in_progress",
            "El agente IA está procesando esta conversación. Reintentá en unos segundos.",
        ));
    }

    // Borrar el estado IA INSIDE the lock window.
    let write_result = state.db.update_conversation_ai_conv_state(&oid, None).await;

    // Auditoría también dentro del lock (mejor consistencia: si el write falló, no auditamos
    // un reset que no ocurrió).
    if write_result.is_ok() {
        record_conv_event(
            &state,
            WaConversationEventInput {
                conversation_id: &oid,
                business_phone: &conv.business_phone,
                event_type: "ai_state_reset",
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(claims.name.as_str()),
                target_id: None,
                target_name: None,
                note: Some("Reset manual del estado IA por supervisor"),
            },
        )
        .await;
    }

    // Liberar el lock antes del broadcast (broadcast es best-effort, no necesita exclusión).
    state.redis.release_ai_dispatch_lock(&id).await;

    write_result.map_err(ApiError::DatabaseError)?;

    tracing::info!(
        "[ai_agent] ai_conv_state reset manual (conv={}, by={})",
        id,
        claims.id
    );

    // Broadcast WS: ai_conv_state = null (limpiado).
    let ev = WsServerEvent::ConversacionEstadoIa {
        conversation_id: id.clone(),
        ai_conv_state: None,
    };
    broadcast_all(&state.ws_registry, &ev).await;

    Ok(Json(ResetAiStateResponse {
        ok: true,
        conversation_id: id,
    }))
}

// ============================================
// INTERVENIR (take-over manual de IA → humano)
// ============================================

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct InterveneData {
    pub conversation_id: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct InterveneResponse {
    pub ok: bool,
    pub data: InterveneData,
}

/// Take-over manual: el agente humano interrumpe a la IA y se queda con la
/// conversación. En un solo shot: asigna al caller, pasa a `in_progress` y
/// setea `ai_disabled=true`. CONSERVA `ai_active_agent_id` (pausa reversible).
/// Emite `CHAT_TOMADO` (assigned_to + status) + `IA_PAUSADA{reason:"manual"}`.
#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/intervene",
    tag = "WhatsApp — Conversaciones",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la conversación")),
    responses(
        (status = 200, description = "Take-over OK: conv asignada al caller, status in_progress, IA pausada", body = InterveneResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "conversation_not_found"),
        (status = 409, description = "ai_not_active (la IA no atiende esta conv) o dispatch_in_progress (turno IA en vuelo)"),
    )
)]
pub async fn intervene_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<InterveneResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let existing = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "conversation_not_found",
                "Conversación no encontrada.",
            )
        })?;

    // Gate: la IA debe estar atendiendo (status=pending && !ai_disabled). Si ya
    // está pausada o un humano la tomó (in_progress) o está cerrada → ai_not_active.
    if existing.ai_disabled || existing.status != "pending" {
        return Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "ai_not_active",
            "La IA no está atendiendo esta conversación (ya pausada, tomada por un humano o cerrada).",
        ));
    }

    // Lock — evita pisarse con un dispatch en vuelo. Si está tomado, el turno IA
    // está corriendo: que el front reintente en unos segundos.
    if !state.redis.try_lock_ai_dispatch(&id).await {
        return Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "dispatch_in_progress",
            "El agente IA está procesando esta conversación. Reintentá en unos segundos.",
        ));
    }

    // Take-over atómico dentro del lock. ai_active_agent_id y aiConvState quedan intactos.
    let taken = state.db.intervene_conversation(&oid, &claims.id).await;

    state.redis.release_ai_dispatch_lock(&id).await;

    let conv = match taken {
        Ok(Some(c)) => c,
        // El filtro atómico no matcheó: otro actor cambió el estado entre el
        // pre-check y el lock. Para el caller es lo mismo que ai_not_active.
        Ok(None) => {
            return Err(ApiError::domain_simple(
                StatusCode::CONFLICT,
                "ai_not_active",
                "La IA dejó de atender esta conversación antes de la intervención.",
            ));
        }
        Err(e) => return Err(ApiError::DatabaseError(e)),
    };

    // La conv pasa a manos del caller → sube su carga (espejo de /take).
    state.redis.incr_agent_load(&claims.id).await;

    // Auditoría.
    record_conv_event(
        &state,
        WaConversationEventInput {
            conversation_id: &oid,
            business_phone: &conv.business_phone,
            event_type: "ai_intervened",
            actor_id: Some(claims.id.as_str()),
            actor_name: Some(claims.name.as_str()),
            target_id: Some(claims.id.as_str()),
            target_name: Some(claims.name.as_str()),
            note: Some("Take-over manual de conversación atendida por IA"),
        },
    )
    .await;

    // WS — dos eventos, ambos broadcast_all (los handlers ya existen en el front):
    // 1) CHAT_TOMADO — assigned_to + status (patchea sidebar/cache).
    let ev_tomado = WsServerEvent::ChatTomado {
        conversation_id: id.clone(),
        taken_by: claims.id.clone(),
        taken_by_name: Some(claims.name.clone()),
        status: conv.status.clone(),
        previous_status: "pending".to_string(),
    };
    broadcast_all(&state.ws_registry, &ev_tomado).await;

    // 2) IA_PAUSADA — ai_disabled (actualiza el indicador IA del chat).
    let ev_pausada = WsServerEvent::IaPausada {
        conversation_id: id.clone(),
        reason: "manual".to_string(),
        by: claims.id.clone(),
    };
    broadcast_all(&state.ws_registry, &ev_pausada).await;

    tracing::info!(
        "[whatsapp] intervene manual (conv={}, by={})",
        id,
        claims.id
    );

    Ok(Json(InterveneResponse {
        ok: true,
        data: InterveneData {
            conversation_id: id,
        },
    }))
}

// ============================================
// REACCIONES A MENSAJES
// ============================================

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct ReactMessageRequest {
    /// Emoji crudo (ej: "👍", "❤️"). Cadena vacía `""` significa "remover mi reacción".
    pub emoji: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct ReactMessageResponse {
    pub ok: bool,
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/messages/{id}/react",
    tag = "WhatsApp — Messages",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId del WaMessage al que se reacciona")),
    request_body = ReactMessageRequest,
    responses(
        (status = 200, description = "Reacción aplicada", body = ReactMessageResponse),
        (status = 400, description = "id inválido o payload malformado"),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "Mensaje no encontrado"),
        (status = 409, description = "reaction_window_expired — ventana de 24h expirada"),
        (status = 502, description = "meta_upstream_error — Meta rechazó la reacción"),
    )
)]
pub async fn react_message_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<crate::auth::user_jwt::UserProfileClaims>,
    Path(id): Path<String>,
    Json(body): Json<ReactMessageRequest>,
) -> Result<Json<ReactMessageResponse>, ApiError> {
    use crate::db::WhatsAppRepository;
    use axum::http::StatusCode;

    // 1. Parsear ObjectId
    let oid = ObjectId::parse_str(&id).map_err(|_| {
        ApiError::domain_simple(StatusCode::BAD_REQUEST, "invalid_id", "id inválido")
    })?;

    // 2. Cargar el WaMessage target
    let message = state
        .db
        .find_message_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "message_not_found",
                "Mensaje no encontrado",
            )
        })?;

    // 3. Cargar la conversación para obtener customer_phone + business_phone
    let conv = state
        .db
        .find_conversation_by_id(&message.conversation_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "conversation_not_found",
                "Conversación no encontrada",
            )
        })?;

    // 4. Resolver WhatsAppService para el business_phone
    let wa = resolve_service_for_phone(&state, &conv.business_phone).await?;

    // 5. Llamar a Meta (Meta acepta emoji vacío para remover)
    wa.send_reaction(&conv.phone, &message.wa_message_id, &body.emoji)
        .await?;

    // 6. Aplicar update en DB (sólo si Meta aceptó)
    let updated = state
        .db
        .update_message_reactions(
            &message.wa_message_id,
            "agent",
            &body.emoji,
            Some(&claims.name),
        )
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "message_not_found",
                "Mensaje desapareció entre lookup y update",
            )
        })?;

    // 7. Broadcast WS
    let event = WsServerEvent::ReaccionMensaje {
        conversation_id: updated.conversation_id.to_hex(),
        message_id: updated.id.map(|o| o.to_hex()).unwrap_or_default(),
        wa_message_id: updated.wa_message_id.clone(),
        emoji: body.emoji.clone(),
        from: "agent".to_string(),
        sender_name: Some(claims.name.clone()),
    };
    broadcast_all(&state.ws_registry, &event).await;

    Ok(Json(ReactMessageResponse { ok: true }))
}
