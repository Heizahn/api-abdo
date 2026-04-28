//! Dispatch real del AI Agent: hook que dispara la IA cuando llega un
//! mensaje inbound de WhatsApp.
//!
//! Flujo:
//! 1. Resolver agente activo del workspace (opción A: el más viejo `enabled`).
//! 2. Construir history desde `WaMessages` (últimos 20 turnos en texto).
//! 3. Cargar FAQs del agente.
//! 4. Llamar `run_turn` (con `is_sandbox=true` cuando `mode=shadow`, así los
//!    tools de escritura no persisten side-effects reales).
//! 5. Persistir `AiInteraction` siempre (log del turno).
//! 6. Si `mode=live`: enviar la respuesta por Meta + persistir outbound +
//!    broadcast WS (TODO — pendiente; hoy solo loguea que NO envió).
//!
//! El dispatch corre en `tokio::spawn` para que el webhook de Meta responda
//! 200 al instante y no quede colgado esperando a Gemini.

use std::sync::Arc;

use mongodb::bson::oid::ObjectId;

use crate::{
    db::AiAgentRepository,
    models::ai_agent::AiAgentMode,
    state::AppState,
};

use super::{
    gemini::AiRelay,
    runner::{decrypt_api_key, run_turn, ConvRole, ConvTurn},
    tools::ToolContext,
};

const HISTORY_MAX_TURNS: i64 = 20;

fn ai_agent_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

/// Spawnea el dispatch en background. Llamada desde el webhook de WA tras
/// persistir el inbound. No bloquea — el webhook ya respondió 200 al
/// momento que esta función retorna.
pub fn dispatch_inbound_async(
    state: Arc<AppState>,
    conversation_id: ObjectId,
    inbound_message_id: Option<ObjectId>,
    inbound_text: Option<String>,
    workspace_id: ObjectId,
    business_phone: String,
) {
    // Si el mensaje no es texto (o no se pudo extraer), por ahora no
    // disparamos la IA. Soporte multimedia llega después.
    let Some(text) = inbound_text.as_ref().and_then(|t| {
        let trimmed = t.trim();
        if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
    }) else {
        return;
    };

    tokio::spawn(async move {
        if let Err(e) = run_dispatch(
            state,
            conversation_id,
            inbound_message_id,
            text,
            workspace_id,
            business_phone,
        )
        .await
        {
            tracing::warn!("[ai_agent.dispatch] error: {}", e);
        }
    });
}

async fn run_dispatch(
    state: Arc<AppState>,
    conversation_id: ObjectId,
    inbound_message_id: Option<ObjectId>,
    user_message: String,
    workspace_id: ObjectId,
    _business_phone: String,
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
    let agent_id = match agent.id {
        Some(id) => id,
        None => return Err("agent sin _id".into()),
    };

    tracing::info!(
        "[ai_agent.dispatch] agent={} (label={}, mode={:?}) procesando conv={}",
        agent_id.to_hex(),
        agent.label,
        agent.mode,
        conversation_id.to_hex()
    );

    // Para shadow: usamos `is_sandbox=true` para que tools de escritura
    // (request_human, create_ticket) no toquen DB. En live usamos `false`.
    let is_sandbox = matches!(agent.mode, AiAgentMode::Shadow);

    // History: últimos N mensajes de la conv en orden cronológico (excluido
    // el inbound recién insertado, que va por separado en `user_message`).
    let raw_history = state
        .db
        .list_recent_messages_for_conversation(&conversation_id, HISTORY_MAX_TURNS)
        .await?;
    let history: Vec<ConvTurn> = raw_history
        .into_iter()
        .filter(|m| {
            // Excluir el inbound actual (si lo encontramos por _id).
            match (inbound_message_id, m.id) {
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
        business_phone: _business_phone,
        agent_id,
        conversation_id: Some(conversation_id),
        ai_user_id: agent.ai_user_id.clone(),
        ai_user_name: agent.personality.assistant_name.clone(),
        is_sandbox,
    };

    let output = match run_turn(
        &state.reqwest_client,
        &agent,
        &api_key,
        relay,
        &history,
        &user_message,
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
                conversation_id.to_hex(),
                e
            );
            return Ok(());
        }
    };

    tracing::info!(
        "[ai_agent.dispatch] turno OK (agent={}, conv={}, mode={:?}, escalated={}, in_tokens={}, out_tokens={}, latency={}ms)",
        agent_id.to_hex(),
        conversation_id.to_hex(),
        agent.mode,
        output.escalated,
        output.input_tokens,
        output.output_tokens,
        output.latency_ms
    );

    // Persistimos el turno como AiInteraction. El message_id es el inbound
    // que disparó el turno (puede ser None si no se pasó — usamos un OID
    // dummy en ese caso, no debería pasar en el flujo del webhook).
    let interaction = output.to_interaction(
        conversation_id,
        inbound_message_id.unwrap_or_else(ObjectId::new),
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

    // En shadow, NO enviamos nada al cliente. Solo logueamos qué habría
    // contestado para que el SUPERADMIN compare.
    if matches!(agent.mode, AiAgentMode::Shadow) {
        if let Some(text) = output.response_text.as_deref() {
            tracing::info!(
                "[ai_agent.dispatch] shadow → habría respondido: {}",
                truncate(text, 300)
            );
        }
        return Ok(());
    }

    // Live: pendiente — enviar por Meta + persistir outbound + WS.
    // TODO PR siguiente: implementar envío real. Por ahora avisamos.
    tracing::warn!(
        "[ai_agent.dispatch] LIVE mode pero envío real aún no implementado. Texto generado: {}",
        output
            .response_text
            .as_deref()
            .map(|t| truncate(t, 300))
            .unwrap_or_else(|| "(sin texto)".into())
    );
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
