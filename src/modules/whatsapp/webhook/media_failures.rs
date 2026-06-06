use std::sync::Arc;

use mongodb::bson::DateTime;

use crate::{
    crypto::aes::decrypt_payload,
    db::{ConversationTouch, WhatsAppRepository},
    models::whatsapp::{WaConversationEventInput, WaMessage},
    state::AppState,
};

use super::status::InboundMediaFailureDetails;
use crate::modules::whatsapp::{
    service::WhatsAppService,
    shared::{apply_media_relay, mappers, response, settings_secret},
    ws::{broadcast_all, broadcast_to_chat_users, ConversacionNoLeidaData, WsServerEvent},
};

/// Antes de avisar "reenvía la imagen", re-chequeamos si el mensaje apareció
/// en DB unos segundos después (desfase webhook status vs message).
/// Si aparece, evitamos fallback prematuro.
const INBOUND_MEDIA_FAILURE_RECHECK_DELAYS_MS: &[u64] = &[0, 10_000, 30_000];

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
    wa_id: &str,
    recipient_phone: &str,
    business_phone: &str,
    failure: Option<InboundMediaFailureDetails>,
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

    persist_inbound_media_failure_placeholder(
        state,
        wa_id,
        recipient_phone,
        business_phone,
        Some(settings.workspace_name.as_str()).filter(|w| !w.is_empty()),
        failure.as_ref(),
    )
    .await;

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

/// Hace visible en el inbox un media inbound que Meta reportó como fallido
/// antes de que existiera un `WaMessage` persistido. Esto evita el peor caso
/// operativo: el cliente sí envió la imagen/documento en WhatsApp, pero el
/// panel queda vacío y el agente cree que nunca llegó nada.
async fn persist_inbound_media_failure_placeholder(
    state: &Arc<AppState>,
    wa_id: &str,
    recipient_phone: &str,
    business_phone: &str,
    workspace_name: Option<&str>,
    failure: Option<&InboundMediaFailureDetails>,
) {
    let (conv, conv_created) = match state
        .db
        .upsert_conversation(recipient_phone, business_phone, None)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "[webhook] inbound_media_failure: no se pudo crear/ubicar conv customer='{}' business='{}': {}",
                recipient_phone,
                business_phone,
                e
            );
            return;
        }
    };
    let conv_id = match conv.id {
        Some(id) => id,
        None => return,
    };

    if conv_created {
        record_conv_event(
            state,
            WaConversationEventInput {
                conversation_id: &conv_id,
                business_phone: &conv.business_phone,
                event_type: "created",
                actor_id: None,
                actor_name: None,
                target_id: None,
                target_name: None,
                note: Some("inbound_media_failure"),
            },
        )
        .await;
    }

    let was_reopened = if conv.status == "closed" {
        match state.db.reopen_conversation(&conv_id).await {
            Ok(changed) => changed,
            Err(e) => {
                tracing::warn!("[webhook] inbound_media_failure reopen error: {}", e);
                false
            }
        }
    } else {
        false
    };

    if was_reopened {
        record_conv_event(
            state,
            WaConversationEventInput {
                conversation_id: &conv_id,
                business_phone: &conv.business_phone,
                event_type: "reopened",
                actor_id: None,
                actor_name: None,
                target_id: None,
                target_name: None,
                note: Some("inbound_media_failure"),
            },
        )
        .await;
    }

    let now = DateTime::now();
    let body = "⚠️ WhatsApp informó que el archivo enviado por el cliente no pudo ser procesado por Meta. Pedí que lo reenvíe como foto/documento o que escriba los datos manualmente.".to_string();
    let raw_payload = Some(serde_json::json!({
        "source": "meta_status_media_failure",
        "wa_message_id": wa_id,
        "customer_phone": recipient_phone,
        "business_phone": business_phone,
        "error": failure.map(|f| serde_json::json!({
            "code": f.code,
            "title": f.title,
            "message": f.message,
            "error_data": f.error_data,
        })),
    }));

    let msg = WaMessage {
        id: None,
        conversation_id: conv_id,
        wa_message_id: wa_id.to_string(),
        direction: "in".to_string(),
        msg_type: "unsupported".to_string(),
        body: Some(body.clone()),
        media_id: None,
        media_mime_type: None,
        media_filename: None,
        status: Some("failed".to_string()),
        meta_error_code: failure.and_then(|f| f.code),
        meta_error_title: failure.and_then(|f| f.title.clone()),
        meta_error_message: failure.and_then(|f| f.message.clone()),
        meta_error_details: failure.and_then(|f| f.error_data.clone()),
        failed_at: Some(now),
        sent_by: None,
        read_by_user_id: None,
        read_at: None,
        idempotency_key: None,
        reply_to_wa_message_id: None,
        url_preview: None,
        voice: false,
        template_name: None,
        template_language: None,
        template_components: None,
        interactive_payload: None,
        contacts_payload: None,
        location: None,
        reactions: vec![],
        raw_payload,
        ai_processed_at: None,
        timestamp: now,
    };

    let saved = match state.db.save_message(msg).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "[webhook] inbound_media_failure: no se pudo persistir placeholder wa_id={}: {}",
                wa_id,
                e
            );
            return;
        }
    };

    let touch = ConversationTouch {
        preview: &body,
        msg_type: "unsupported",
        direction: "in",
        wa_message_id: wa_id,
        from_user_id: None,
        media_filename: None,
        status: Some("failed"),
        increment_unread: true,
        last_message_at: Some(now),
    };
    if let Err(e) = state.db.touch_conversation(&conv_id, touch).await {
        tracing::warn!("[webhook] inbound_media_failure touch error: {}", e);
    }

    let conv_now = state
        .db
        .find_conversation_by_id(&conv_id)
        .await
        .ok()
        .flatten()
        .unwrap_or(conv);

    if conv_created {
        let resolved = mappers::resolve_customer_name(state, &conv_now).await;
        let new_ev = WsServerEvent::ConversacionNueva {
            conversation: response::conv_to_item(
                conv_now.clone(),
                false,
                None,
                workspace_name.map(str::to_string),
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

    let unread_pending = state.db.count_unread_conversations().await.unwrap_or(0);
    let unread_ev = WsServerEvent::ConversacionNoLeida {
        data: ConversacionNoLeidaData {
            pending_total: unread_pending,
            conversation_id: conv_id.to_hex(),
            delta: 1,
        },
    };
    if let Ok(badge_payload) = serde_json::to_string(&unread_ev) {
        let _ = broadcast_to_chat_users(state, badge_payload).await;
    }

    let item = mappers::msg_to_item(saved, None, None);
    let msg_ev = WsServerEvent::MensajeNuevo {
        conversation_id: conv_id.to_hex(),
        message: item,
    };
    broadcast_all(&state.ws_registry, &msg_ev).await;
}

/// Evita fallback prematuro en `131052/131056`: algunos eventos de status
/// pueden adelantarse al guardado del mensaje. Re-chequeamos por `wa_id` y
/// sólo avisamos al cliente si después de varios intentos sigue sin doc.
pub(crate) async fn schedule_inbound_media_failure_fallback(
    state: &Arc<AppState>,
    wa_id: &str,
    recipient_phone: &str,
    business_phone: &str,
    failure: Option<InboundMediaFailureDetails>,
) {
    for (attempt, delay_ms) in INBOUND_MEDIA_FAILURE_RECHECK_DELAYS_MS.iter().enumerate() {
        if *delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
        }
        let probe = vec![wa_id.to_string()];
        match state.db.find_messages_by_wa_ids(&probe).await {
            Ok(map) if map.contains_key(wa_id) => {
                tracing::debug!(
                    "[webhook] inbound_media_failure: wa_id={} apareció en DB en recheck #{}, no se envía fallback",
                    wa_id,
                    attempt + 1
                );
                return;
            }
            Ok(_) => {
                tracing::debug!(
                    "[webhook] inbound_media_failure: wa_id={} aún ausente en recheck #{}",
                    wa_id,
                    attempt + 1
                );
            }
            Err(e) => tracing::warn!(
                "[webhook] inbound_media_failure: recheck DB error wa_id={} (#{}) {}",
                wa_id,
                attempt + 1,
                e
            ),
        }
    }
    notify_inbound_media_failure(state, wa_id, recipient_phone, business_phone, failure).await;
}

/// Persiste un evento de ciclo de vida de conversación. Best-effort:
/// si la inserción falla se loggea pero NO se propaga el error — la
/// auditoría no debe bloquear la respuesta HTTP del agente.
async fn record_conv_event(state: &AppState, input: WaConversationEventInput<'_>) {
    if let Err(e) = state.db.record_conversation_event(input).await {
        tracing::warn!("record_conversation_event failed: {}", e);
    }
}
