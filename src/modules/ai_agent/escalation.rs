//! Auto-escalación de la IA → humano.
//!
//! Centraliza la transición "el agente IA deja la conv para que un humano
//! la atienda". Lo dispara la tool `request_human` (cuando la IA decide) y
//! varios gates del dispatch (limit reached, keyword matched, critical
//! failure, etc).
//!
//! Lo que hace en una sola llamada:
//! 1. Actualiza la conv: `ai_disabled=true`, limpia `ai_active_agent_id` y
//!    `ai_transfer_context`.
//! 2. Libera la asignación (`assigned_to=null`).
//! 3. Persiste un `WaConversationEvent` con `event_type=ai_handoff` + nota.
//! 4. Limpia counters per-conv en Redis.
//! 5. (En `live`) envía `farewell_to_human` al cliente como último mensaje
//!    de la IA antes de pausarla. Si `farewell_to_human` está vacío, usa un
//!    fallback genérico.
//! 6. Broadcastea `IA_PAUSADA` por WS para que el front actualice el header
//!    del chat.

use std::sync::Arc;

use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};

use crate::{
    crypto::aes::decrypt_payload,
    db::{ConversationAiPatch, ConversationTouch, WhatsAppRepository},
    models::{
        ai_agent::{AiAgent, AiAgentMode},
        whatsapp::{WaConversationEventInput, WaMessage},
    },
    state::AppState,
};

use crate::modules::whatsapp::assignment;

use super::ai_agent_secret;

// Reasons que llegan al evento WS y al timeline. Stable strings — el front
// los usa para decidir copy / icono.
pub const REASON_REQUEST_HUMAN: &str = "request_human";
pub const REASON_CONVERSATION_TURN_LIMIT: &str = "conversation_turn_limit";
pub const REASON_DAILY_TURN_LIMIT: &str = "daily_turn_limit";
pub const REASON_DAILY_TOKEN_LIMIT: &str = "daily_token_limit";
pub const REASON_KEYWORD_MATCHED: &str = "keyword_matched";
pub const REASON_NO_RESOLUTION: &str = "no_resolution";
pub const REASON_MAX_ID_ATTEMPTS: &str = "max_identification_attempts";
pub const REASON_CRITICAL_TOOL_FAILURE: &str = "critical_tool_failure";

const FALLBACK_FAREWELL: &str = "Te derivo con un compañero del equipo para que te atienda.";

/// Resultado del helper. Útil para que el caller decida si seguir procesando
/// o cortar (auto-escalación corta el flujo del dispatch).
#[allow(dead_code)]
pub struct AutoEscalateResult {
    pub farewell_sent: bool,
}

/// Marca la conv como `ai_disabled`, persiste evento y emite WS.
///
/// Si `send_farewell=true` y el agente está en `mode=live`, envía
/// `personality.farewell_to_human` al cliente vía Meta antes de pausar.
/// Si `farewell_to_human` está vacío usa un fallback genérico.
///
/// Best-effort: si alguna sub-operación falla (DB, WS, Meta), loggeamos y
/// continuamos. La conv queda igualmente en `ai_disabled=true` — el humano
/// puede tomarla.
pub async fn auto_escalate(
    state: &Arc<AppState>,
    conversation_id: &ObjectId,
    agent: &AiAgent,
    reason: &str,
    note: Option<&str>,
    send_farewell: bool,
) -> AutoEscalateResult {
    let conv_hex = conversation_id.to_hex();

    // 1. Actualizar estado IA en la conv.
    let patch = ConversationAiPatch {
        ai_active_agent_id: Some(None),
        ai_disabled: Some(true),
        ai_transfer_context: Some(None),
    };
    if let Err(e) = state
        .db
        .update_conversation_ai_state(conversation_id, patch)
        .await
    {
        tracing::warn!("[ai_agent.escalate] update_conversation_ai_state: {}", e);
    }

    // 2. Liberar asignación.
    if let Err(e) = state.db.assign_conversation(conversation_id, None).await {
        tracing::warn!("[ai_agent.escalate] release assignment: {}", e);
    }

    // 3. Cargar la conv para el evento (necesita business_phone) y el envío
    // del farewell (necesita customer phone + access_token).
    let conv = match state.db.find_conversation_by_id(conversation_id).await {
        Ok(Some(c)) => Some(c),
        _ => None,
    };

    if let Some(c) = conv.as_ref() {
        let event_input = WaConversationEventInput {
            conversation_id,
            business_phone: &c.business_phone,
            event_type: "ai_handoff",
            actor_id: Some(agent.ai_user_id.as_str()),
            actor_name: Some(agent.personality.assistant_name.as_str()),
            target_id: None,
            target_name: None,
            note,
        };
        if let Err(e) = state.db.record_conversation_event(event_input).await {
            tracing::warn!("[ai_agent.escalate] record_conversation_event: {}", e);
        }
    }

    // 4. Limpiar counters per-conv.
    state.redis.clear_ai_conv_counters(&conv_hex).await;

    // 5. Enviar farewell si corresponde (live mode + conv conocida).
    let mut farewell_sent = false;
    if send_farewell && matches!(agent.mode, AiAgentMode::Live) {
        if let Some(c) = conv.as_ref() {
            let farewell_text = if agent.personality.farewell_to_human.trim().is_empty() {
                FALLBACK_FAREWELL.to_string()
            } else {
                agent.personality.farewell_to_human.trim().to_string()
            };
            match send_farewell_message(state, c, agent, &farewell_text).await {
                Ok(()) => farewell_sent = true,
                Err(e) => tracing::warn!("[ai_agent.escalate] farewell envío falló: {}", e),
            }
        }
    }

    // 5b. Disparar asignación a humano.
    // Al escalar a humano, la conv queda ai_disabled=true pero sin asignado. Si no
    // disparamos la asignación explícitamente, el cliente espera indefinidamente.
    // Cargamos wa_settings para obtener la lista de agentes humanos habilitados.
    if let Some(c) = conv.as_ref() {
        match state.db.find_wa_settings_by_phone(&c.business_phone).await {
            Ok(Some(settings)) if !settings.agents.is_empty() => {
                tracing::info!(
                    "[ai_agent.tools] request_human → triggering human assignment for conv {}",
                    conv_hex
                );
                let state_clone = Arc::clone(state);
                let conv_id_for_assignment = *conversation_id;
                tokio::spawn(async move {
                    assignment::assign_conversation(state_clone, conv_id_for_assignment).await;
                    tracing::info!(
                        "[ai_agent.tools] human assigned for conv {}",
                        conv_id_for_assignment.to_hex()
                    );
                });
            }
            Ok(Some(_)) => {
                tracing::warn!(
                    "[ai_agent.escalate] wa_settings.agents vacío para phone {} — conv {} no puede asignarse a humano",
                    c.business_phone, conv_hex
                );
            }
            Ok(None) => {
                tracing::warn!(
                    "[ai_agent.escalate] wa_settings no encontrados para phone {} — conv {} no puede asignarse",
                    c.business_phone, conv_hex
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[ai_agent.escalate] error cargando wa_settings para phone {}: {}",
                    c.business_phone,
                    e
                );
            }
        }
    }

    // 6. Broadcast WS — siempre, para actualizar UI.
    let event = crate::modules::whatsapp::ws::WsServerEvent::IaPausada {
        conversation_id: conv_hex,
        reason: reason.to_string(),
        by: "ai_agent".to_string(),
    };
    crate::modules::whatsapp::ws::broadcast_all(&state.ws_registry, &event).await;

    AutoEscalateResult { farewell_sent }
}

async fn send_farewell_message(
    state: &Arc<AppState>,
    conv: &crate::models::whatsapp::WaConversation,
    agent: &AiAgent,
    text: &str,
) -> Result<(), String> {
    let conv_id = conv.id.ok_or_else(|| "conv sin _id".to_string())?;
    let wa_settings = state
        .db
        .find_wa_settings_by_phone(&conv.business_phone)
        .await
        .map_err(|e| format!("wa_settings: {}", e))?
        .ok_or_else(|| "wa_settings no encontrados".to_string())?;
    let token = decrypt_payload(&ai_agent_secret(), &wa_settings.access_token)
        .ok_or_else(|| "no se pudo descifrar access_token".to_string())?;

    let mut svc = crate::modules::whatsapp::service::WhatsAppService::new(
        state.reqwest_client.clone(),
        wa_settings.phone_number_id.clone(),
        token,
    );
    if let (Some(url), Some(secret)) = (
        state.config.relay_url.as_ref(),
        state.config.relay_secret.as_ref(),
    ) {
        svc = svc.with_media_relay(crate::modules::whatsapp::service::MediaRelay {
            url: url.clone(),
            secret: secret.clone(),
        });
    }

    let wa_id = svc
        .send_text(&conv.phone, text, None, false)
        .await
        .map_err(|e| format!("send_text: {}", e))?;

    let now = BsonDateTime::now();
    let outbound = WaMessage {
        id: None,
        conversation_id: conv_id,
        wa_message_id: wa_id,
        direction: "out".into(),
        msg_type: "text".into(),
        body: Some(text.to_string()),
        media_id: None,
        media_mime_type: None,
        media_filename: None,
        status: Some("sent".into()),
        meta_error_code: None,
        meta_error_title: None,
        meta_error_message: None,
        meta_error_details: None,
        failed_at: None,
        sent_by: Some(agent.ai_user_id.clone()),
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
        raw_payload: None,
        ai_processed_at: None,
        timestamp: now,
    };
    let saved = state
        .db
        .save_message(outbound)
        .await
        .map_err(|e| format!("save_message: {}", e))?;

    let touch = ConversationTouch {
        preview: text,
        msg_type: &saved.msg_type,
        direction: "out",
        wa_message_id: &saved.wa_message_id,
        from_user_id: Some(agent.ai_user_id.as_str()),
        media_filename: None,
        status: Some("sent"),
        increment_unread: false,
        last_message_at: Some(now),
    };
    state
        .db
        .touch_conversation(&conv_id, touch)
        .await
        .map_err(|e| format!("touch_conversation: {}", e))?;

    let item = crate::modules::whatsapp::handler::build_message_item(state, saved).await;
    let ev = crate::modules::whatsapp::ws::WsServerEvent::MensajeNuevo {
        conversation_id: conv_id.to_hex(),
        message: item,
    };
    crate::modules::whatsapp::ws::broadcast_all(&state.ws_registry, &ev).await;
    Ok(())
}
