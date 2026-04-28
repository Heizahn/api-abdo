//! Dispatch real del AI Agent: hook que dispara la IA cuando llega un
//! mensaje inbound de WhatsApp.
//!
//! Flujo:
//! 1. Resolver agente activo del workspace (opción A: el más viejo `enabled`).
//! 2. Cargar conv + wa_settings (descifrar access_token).
//! 3. Descargar multimedia si el inbound es image/audio/video/document.
//! 4. Construir history desde `WaMessages` (últimos 20 textos).
//! 5. Cargar FAQs del agente.
//! 6. Llamar `run_turn` con texto + media (Gemini 1.5+ es multimodal).
//! 7. Persistir `AiInteraction` (siempre, independientemente del modo).
//! 8. Si `mode=live`: enviar la respuesta por Meta + persistir el WaMessage
//!    outbound + tocar la conv + broadcast WS.
//!
//! Corre en `tokio::spawn` para no bloquear el webhook.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};

use crate::{
    crypto::aes::decrypt_payload,
    db::{AiAgentRepository, ConversationTouch, WhatsAppRepository},
    models::{
        ai_agent::AiAgentMode,
        whatsapp::WaMessage,
    },
    state::AppState,
};

use super::{
    gemini::AiRelay,
    runner::{decrypt_api_key, run_turn, ConvRole, ConvTurn, MediaInput},
    tools::ToolContext,
};

const HISTORY_MAX_TURNS: i64 = 20;

fn ai_agent_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

/// Spawnea el dispatch en background. Llamada desde el webhook tras
/// persistir el inbound. No bloquea — el webhook ya respondió 200 al
/// momento que esta función retorna.
pub fn dispatch_inbound_async(
    state: Arc<AppState>,
    inbound: WaMessage,
    workspace_id: ObjectId,
) {
    tokio::spawn(async move {
        if let Err(e) = run_dispatch(state, inbound, workspace_id).await {
            tracing::warn!("[ai_agent.dispatch] error: {}", e);
        }
    });
}

async fn run_dispatch(
    state: Arc<AppState>,
    inbound: WaMessage,
    workspace_id: ObjectId,
) -> Result<(), String> {
    let agent = match state
        .db
        .find_active_agent_for_workspace(&workspace_id)
        .await?
    {
        Some(a) => a,
        None => {
            tracing::debug!(
                "[ai_agent.dispatch] sin agente activo para workspace={}",
                workspace_id.to_hex()
            );
            return Ok(());
        }
    };
    let agent_id = agent.id.ok_or_else(|| "agent sin _id".to_string())?;

    tracing::info!(
        "[ai_agent.dispatch] agent={} (label={}, mode={:?}) procesando conv={}",
        agent_id.to_hex(),
        agent.label,
        agent.mode,
        inbound.conversation_id.to_hex()
    );

    let conv = state
        .db
        .find_conversation_by_id(&inbound.conversation_id)
        .await?
        .ok_or_else(|| "conv no encontrada".to_string())?;

    let wa_settings = state
        .db
        .find_wa_settings_by_id(&workspace_id)
        .await?
        .ok_or_else(|| "wa_settings no encontradas".to_string())?;

    // Para shadow: usamos `is_sandbox=true` para que tools de escritura
    // (request_human, create_ticket) no toquen DB. En live usamos `false`.
    let is_live = matches!(agent.mode, AiAgentMode::Live);
    let is_sandbox = !is_live;

    // Texto que va al modelo: body del inbound (caption en media, body en texto).
    let user_text = inbound.body.clone().unwrap_or_default();

    // Multimedia inline para Gemini. Solo procesamos lo que Gemini sabe leer
    // (image/audio/video/pdf). Sticker/document genéricos van como "el cliente
    // envió un archivo: <filename>" en texto.
    let user_media = build_media_inputs(&state, &wa_settings, &inbound).await;

    // Si no hay nada que mandar, salimos. Evita roundtrip a Gemini para
    // tipos no soportados (location, contacts, button reply legacy, etc).
    if user_text.trim().is_empty() && user_media.is_empty() {
        tracing::info!(
            "[ai_agent.dispatch] inbound sin contenido procesable (msg_type={}); skip",
            inbound.msg_type
        );
        return Ok(());
    }

    // History: últimos N textos en orden cronológico (excluido el inbound
    // recién insertado, que va por separado).
    let raw_history = state
        .db
        .list_recent_messages_for_conversation(&inbound.conversation_id, HISTORY_MAX_TURNS)
        .await?;
    let history: Vec<ConvTurn> = raw_history
        .into_iter()
        .filter(|m| {
            // Excluir el inbound actual.
            match (inbound.id, m.id) {
                (Some(target), Some(this)) => this != target,
                _ => true,
            }
        })
        .filter_map(|m| {
            let text = m.body?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let role = match m.direction.as_str() {
                "in" => ConvRole::User,
                "out" => ConvRole::Assistant,
                _ => return None,
            };
            Some(ConvTurn { role, text })
        })
        .collect();

    let api_key = match decrypt_api_key(&agent, &ai_agent_secret()) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(
                "[ai_agent.dispatch] api_key indisponible (agent={}): {:?}",
                agent_id.to_hex(),
                e
            );
            return Ok(());
        }
    };

    let faqs = state.db.list_ai_agent_faqs(&agent_id).await?;
    let faqs_inline = if faqs.is_empty() {
        None
    } else {
        let mut buf = String::new();
        for f in &faqs {
            buf.push_str("Q: ");
            buf.push_str(&f.question);
            buf.push_str("\nA: ");
            buf.push_str(&f.answer);
            buf.push_str("\n\n");
        }
        Some(buf)
    };

    let relay_owned = AiRelay::from_config(&state.config);
    let relay = relay_owned.as_ref();

    let tool_ctx = ToolContext {
        state: state.clone(),
        workspace_id,
        business_phone: conv.business_phone.clone(),
        agent_id,
        conversation_id: Some(inbound.conversation_id),
        ai_user_id: agent.ai_user_id.clone(),
        ai_user_name: agent.personality.assistant_name.clone(),
        is_sandbox,
    };

    // Si no hay texto y solo hay media, mandamos un placeholder breve para
    // que la IA tenga un pivot conversacional.
    let effective_user_message = if user_text.trim().is_empty() {
        match inbound.msg_type.as_str() {
            "audio" => "(el cliente envió un mensaje de audio — escuchalo y respondé)",
            "image" => "(el cliente envió una imagen — describila/analizala y respondé)",
            "video" => "(el cliente envió un video — analizalo y respondé)",
            "document" => "(el cliente envió un documento — leélo y respondé)",
            _ => "(adjunto sin texto)",
        }
        .to_string()
    } else {
        user_text
    };

    let output = match run_turn(
        &state.reqwest_client,
        &agent,
        &api_key,
        relay,
        &history,
        &effective_user_message,
        &user_media,
        faqs_inline.as_deref(),
        &tool_ctx,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                "[ai_agent.dispatch] runner error (agent={}, conv={}): {:?}",
                agent_id.to_hex(),
                inbound.conversation_id.to_hex(),
                e
            );
            return Ok(());
        }
    };

    tracing::info!(
        "[ai_agent.dispatch] turno OK (agent={}, conv={}, mode={:?}, escalated={}, in_tokens={}, out_tokens={}, latency={}ms)",
        agent_id.to_hex(),
        inbound.conversation_id.to_hex(),
        agent.mode,
        output.escalated,
        output.input_tokens,
        output.output_tokens,
        output.latency_ms
    );

    // Persistimos el turno como AiInteraction.
    let interaction = output.to_interaction(
        inbound.conversation_id,
        inbound.id.unwrap_or_else(ObjectId::new),
        workspace_id,
        agent_id,
        0,
        &agent.model.model_id,
    );
    if let Err(e) = state.db.create_ai_interaction(interaction).await {
        tracing::warn!(
            "[ai_agent.dispatch] persistir AiInteraction falló: {}",
            e
        );
    }

    let response_text = match output.response_text.as_deref() {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => {
            tracing::info!("[ai_agent.dispatch] runner no produjo texto, no envío");
            return Ok(());
        }
    };

    // En shadow: solo logueamos qué habría contestado.
    if !is_live {
        tracing::info!(
            "[ai_agent.dispatch] shadow → habría respondido: {}",
            truncate(&response_text, 300)
        );
        return Ok(());
    }

    // Live: descifrar access_token, construir WhatsAppService, enviar y
    // persistir el outbound + WS.
    if let Err(e) = send_live_response(
        &state,
        &wa_settings,
        &conv.phone,
        &agent.ai_user_id,
        inbound.conversation_id,
        &response_text,
    )
    .await
    {
        tracing::error!(
            "[ai_agent.dispatch] envío live falló (agent={}, conv={}): {}",
            agent_id.to_hex(),
            inbound.conversation_id.to_hex(),
            e
        );
    }

    Ok(())
}

/// Construye los `MediaInput` para Gemini bajando el binario via Meta. Solo
/// soporta tipos que Gemini procesa nativo. Si la descarga falla, devolvemos
/// vacío y la IA responderá solo al texto/caption.
async fn build_media_inputs(
    state: &Arc<AppState>,
    wa_settings: &crate::models::whatsapp::WaSettings,
    inbound: &WaMessage,
) -> Vec<MediaInput> {
    let media_id = match inbound.media_id.as_deref() {
        Some(m) if !m.is_empty() => m,
        _ => return Vec::new(),
    };

    // Gemini multimodal: image/audio/video/pdf van inline. El resto (sticker,
    // documents Office, etc) NO los pasamos como inline — la IA puede
    // responder al caption/contexto sin ver el binario.
    let supported = match inbound.msg_type.as_str() {
        "image" | "audio" | "video" => true,
        "document" => inbound
            .media_mime_type
            .as_deref()
            .map(|m| m == "application/pdf" || m.starts_with("text/"))
            .unwrap_or(false),
        _ => false,
    };
    if !supported {
        return Vec::new();
    }

    // Descifrar access_token y armar service para descargar el media.
    let token = match decrypt_payload(&ai_agent_secret(), &wa_settings.access_token) {
        Some(t) => t,
        None => {
            tracing::warn!("[ai_agent.dispatch] no se pudo descifrar access_token para media");
            return Vec::new();
        }
    };
    let mut svc = crate::modules::whatsapp::service::WhatsAppService::new(
        state.reqwest_client.clone(),
        wa_settings.phone_number_id.clone(),
        token,
    );
    if let (Some(url), Some(secret)) = (
        state.config.wa_media_relay_url.as_ref(),
        state.config.wa_media_relay_secret.as_ref(),
    ) {
        svc = svc.with_media_relay(crate::modules::whatsapp::service::MediaRelay {
            url: url.clone(),
            secret: secret.clone(),
        });
    }

    let (bytes, mime, _filename) = match svc.download_media(media_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                "[ai_agent.dispatch] descarga de media {} falló: {}",
                media_id, e
            );
            return Vec::new();
        }
    };

    let data_b64 = B64.encode(&bytes);
    vec![MediaInput {
        mime_type: mime,
        data_base64: data_b64,
    }]
}

/// Envío real en `live`: send_text vía Meta → persistir WaMessage outbound →
/// touch_conversation → broadcast `MENSAJE_NUEVO`.
async fn send_live_response(
    state: &Arc<AppState>,
    wa_settings: &crate::models::whatsapp::WaSettings,
    customer_phone: &str,
    ai_user_id: &str,
    conversation_id: ObjectId,
    text: &str,
) -> Result<(), String> {
    let token = decrypt_payload(&ai_agent_secret(), &wa_settings.access_token)
        .ok_or_else(|| "no se pudo descifrar access_token".to_string())?;

    let mut svc = crate::modules::whatsapp::service::WhatsAppService::new(
        state.reqwest_client.clone(),
        wa_settings.phone_number_id.clone(),
        token,
    );
    if let (Some(url), Some(secret)) = (
        state.config.wa_media_relay_url.as_ref(),
        state.config.wa_media_relay_secret.as_ref(),
    ) {
        svc = svc.with_media_relay(crate::modules::whatsapp::service::MediaRelay {
            url: url.clone(),
            secret: secret.clone(),
        });
    }

    let wa_id = svc
        .send_text(customer_phone, text, None, false)
        .await
        .map_err(|e| format!("send_text: {}", e))?;

    let now = BsonDateTime::now();
    let outbound = WaMessage {
        id: None,
        conversation_id,
        wa_message_id: wa_id.clone(),
        direction: "out".to_string(),
        msg_type: "text".to_string(),
        body: Some(text.to_string()),
        media_id: None,
        media_mime_type: None,
        media_filename: None,
        status: Some("sent".to_string()),
        sent_by: Some(ai_user_id.to_string()),
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
        timestamp: now,
    };
    let saved = state
        .db
        .save_message(outbound)
        .await
        .map_err(|e| format!("save_message: {}", e))?;

    let preview = text.to_string();
    let touch = ConversationTouch {
        preview: &preview,
        msg_type: &saved.msg_type,
        direction: "out",
        wa_message_id: &saved.wa_message_id,
        from_user_id: Some(ai_user_id),
        media_filename: None,
        status: Some("sent"),
        increment_unread: false,
        last_message_at: Some(now),
    };
    state
        .db
        .touch_conversation(&conversation_id, touch)
        .await
        .map_err(|e| format!("touch_conversation: {}", e))?;

    // Broadcast WS para que el front vea el mensaje saliente del bot.
    let item = crate::modules::whatsapp::handler::build_message_item(state, saved).await;
    let ev = crate::modules::whatsapp::ws::WsServerEvent::MensajeNuevo {
        conversation_id: conversation_id.to_hex(),
        message: item,
    };
    crate::modules::whatsapp::ws::broadcast_all(&state.ws_registry, &ev).await;
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{}…", cut)
    }
}
