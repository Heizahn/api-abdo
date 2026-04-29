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
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};

use crate::{
    crypto::aes::decrypt_payload,
    db::{
        AiAgentRepository, ConversationAiPatch, ConversationTouch, ProfileRepository,
        WhatsAppRepository,
    },
    models::{
        ai_agent::{AiAgent, AiAgentMode},
        whatsapp::WaMessage,
    },
    state::AppState,
};

use super::{
    escalation,
    gemini::AiRelay,
    runner::{decrypt_api_key, run_turn, ConvRole, ConvTurn, MediaInput, PromptVariables},
    tools::{extract_allowed_transfer_targets, ToolContext},
};

/// Cuántos mensajes leemos hacia atrás para armar history + ráfaga.
const RECENT_WINDOW: i64 = 40;

/// Fallback cuando el agente no tiene `debounce_seconds` (compat con docs
/// viejos antes del campo). El default real es 10s y vive en el agente.
const DEBOUNCE_FALLBACK_SECONDS: u64 = 10;

fn ai_agent_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

/// Spawnea el dispatch en background. Llamada desde el webhook tras
/// persistir el inbound. No bloquea — el webhook ya respondió 200 al
/// momento que esta función retorna.
///
/// **Debounce**: cuando llega un inbound, marca timestamp en Redis y
/// duerme N segundos. Al despertar, si su timestamp sigue siendo el
/// último → procesa toda la ráfaga ("Hola", "como estas?", "tengo duda")
/// como un único turno. Si llegó otro mensaje después → ese spawn lo
/// procesará, este sale.
///
/// **Lock anti-concurrencia**: red de seguridad además del debounce
/// (TTL 60s). Releído al final.
pub fn dispatch_inbound_async(
    state: Arc<AppState>,
    inbound: WaMessage,
    workspace_id: ObjectId,
) {
    let conv_hex = inbound.conversation_id.to_hex();
    let now_ms = chrono::Utc::now().timestamp_millis();

    tokio::spawn(async move {
        // Marcar como "actividad reciente". El próximo inbound sobreescribe.
        state.redis.set_ai_debounce_ts(&conv_hex, now_ms).await;

        // Cargar la conv y resolver agente según su estado IA. Si la conv
        // tiene `ai_disabled=true`, NO procesamos (un humano la atiende).
        let conv = match state.db.find_conversation_by_id(&inbound.conversation_id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                tracing::warn!(
                    "[ai_agent.dispatch] conv no encontrada (id={})",
                    inbound.conversation_id.to_hex()
                );
                return;
            }
            Err(e) => {
                tracing::warn!("[ai_agent.dispatch] error buscando conv: {}", e);
                return;
            }
        };
        if conv.ai_disabled {
            tracing::debug!(
                "[ai_agent.dispatch] conv {} con ai_disabled=true, skip",
                conv_hex
            );
            return;
        }

        let agent = match select_agent(&state, &conv, &workspace_id).await {
            Some(a) => a,
            None => {
                tracing::debug!(
                    "[ai_agent.dispatch] sin agente activo para workspace={}",
                    workspace_id.to_hex()
                );
                return;
            }
        };

        // ── Schedule enforcement ───────────────────────────────────────────
        if !is_within_schedule(&agent.schedule) {
            tracing::info!(
                "[ai_agent.dispatch] fuera de horario (agent={}, tz={}), skip",
                agent.id.map(|o| o.to_hex()).unwrap_or_default(),
                agent.schedule.timezone
            );
            return;
        }

        // ── Daily limits per-agent (turns / tokens) ────────────────────────
        // Si el agente alcanzó su cap diario, esta conv se auto-escala
        // (cliente recibe farewell_to_human + humano puede tomar). Cada conv
        // que llegue después del cap escala individualmente — la IA no
        // responde más hasta el rollover de medianoche en TZ Caracas.
        if let Some(agent_oid) = agent.id {
            let agent_hex = agent_oid.to_hex();
            let used_turns = state.redis.get_ai_turns_agent_daily(&agent_hex).await;
            if agent.limits.max_turns_per_day > 0
                && used_turns >= agent.limits.max_turns_per_day as i64
            {
                tracing::warn!(
                    "[ai_agent.dispatch] daily turn limit reached (agent={}, used={}, cap={}); escalate conv={}",
                    agent_hex, used_turns, agent.limits.max_turns_per_day, conv_hex
                );
                escalation::auto_escalate(
                    &state,
                    &inbound.conversation_id,
                    &agent,
                    escalation::REASON_DAILY_TURN_LIMIT,
                    Some("Límite diario de turnos del agente alcanzado"),
                    true,
                )
                .await;
                return;
            }
            let used_tokens = state.redis.get_ai_tokens_agent_daily(&agent_hex).await;
            if agent.limits.max_tokens_per_day > 0
                && used_tokens >= agent.limits.max_tokens_per_day as i64
            {
                tracing::warn!(
                    "[ai_agent.dispatch] daily token limit reached (agent={}, used={}, cap={}); escalate conv={}",
                    agent_hex, used_tokens, agent.limits.max_tokens_per_day, conv_hex
                );
                escalation::auto_escalate(
                    &state,
                    &inbound.conversation_id,
                    &agent,
                    escalation::REASON_DAILY_TOKEN_LIMIT,
                    Some("Límite diario de tokens del agente alcanzado"),
                    true,
                )
                .await;
                return;
            }
        }

        let debounce_secs = if agent.debounce_seconds == 0 {
            DEBOUNCE_FALLBACK_SECONDS
        } else {
            agent.debounce_seconds as u64
        };

        // Esperar el debounce.
        tokio::time::sleep(Duration::from_secs(debounce_secs)).await;

        // ¿Sigo siendo el último? Si no, otro spawn procesará la ráfaga.
        match state.redis.get_ai_debounce_ts(&conv_hex).await {
            Some(latest) if latest == now_ms => {}
            _ => {
                tracing::debug!(
                    "[ai_agent.dispatch] debounce: llegó otro mensaje, skip (conv={})",
                    conv_hex
                );
                return;
            }
        }

        // Red de seguridad: nadie más procesando esta conv ahora.
        if !state.redis.try_lock_ai_dispatch(&conv_hex).await {
            tracing::info!(
                "[ai_agent.dispatch] otro dispatch en curso para conv={}; skip",
                conv_hex
            );
            return;
        }

        let result = run_dispatch(state.clone(), agent, inbound, workspace_id).await;
        state.redis.release_ai_dispatch_lock(&conv_hex).await;
        if let Err(e) = result {
            tracing::warn!("[ai_agent.dispatch] error: {}", e);
        }
    });
}

async fn run_dispatch(
    state: Arc<AppState>,
    agent: AiAgent,
    inbound: WaMessage,
    workspace_id: ObjectId,
) -> Result<(), String> {
    // Re-leemos la conv después del debounce: el estado IA pudo cambiar
    // (humano tomó, transfer entre agents) durante la espera.
    let conv = state
        .db
        .find_conversation_by_id(&inbound.conversation_id)
        .await?
        .ok_or_else(|| "conv no encontrada".to_string())?;

    if conv.ai_disabled {
        tracing::debug!(
            "[ai_agent.dispatch] conv {} pasó a ai_disabled durante debounce, skip",
            inbound.conversation_id.to_hex()
        );
        return Ok(());
    }

    // Si el agente activo registrado en la conv difiere del que pasó por el
    // debounce (porque hubo transfer durante la espera), re-resolvemos.
    let agent = match conv.ai_active_agent_id {
        Some(active) if Some(active) != agent.id => {
            match state.db.find_ai_agent_by_id(&active).await? {
                Some(a) if a.enabled => a,
                _ => {
                    tracing::debug!(
                        "[ai_agent.dispatch] ai_active_agent_id obsoleto/deshabilitado, skip"
                    );
                    return Ok(());
                }
            }
        }
        _ => agent,
    };

    let agent_id = agent.id.ok_or_else(|| "agent sin _id".to_string())?;

    tracing::info!(
        "[ai_agent.dispatch] agent={} (label={}, mode={:?}) procesando conv={}",
        agent_id.to_hex(),
        agent.label,
        agent.mode,
        inbound.conversation_id.to_hex()
    );

    let wa_settings = state
        .db
        .find_wa_settings_by_id(&workspace_id)
        .await?
        .ok_or_else(|| "wa_settings no encontradas".to_string())?;

    // Para shadow: usamos `is_sandbox=true` para que tools de escritura
    // (request_human, create_ticket) no toquen DB. En live usamos `false`.
    let is_live = matches!(agent.mode, AiAgentMode::Live);
    let is_sandbox = !is_live;

    // ── Releer mensajes recientes y armar la "ráfaga" ───────────────────
    // `recent` viene ordenado por `_id` ascendente (orden de inserción real
    // en el back, no `timestamp` de Meta). Ráfaga = todos los inbounds con
    // `_id >= inbound.id` — el inbound original que disparó este dispatch
    // y cualquiera que llegó después (incluso si su timestamp es anterior
    // al último outbound persistido).
    let inbound_oid = match inbound.id {
        Some(o) => o,
        None => return Err("inbound sin _id".into()),
    };
    let recent = state
        .db
        .list_recent_messages_for_conversation(&inbound.conversation_id, RECENT_WINDOW)
        .await?;

    // History: todo lo que NO está en la ráfaga. Esto incluye outbounds
    // posteriores al inbound_oid (porque el bot pudo haber respondido a
    // un turno previo mientras este mensaje quedaba pendiente). Los
    // mantenemos para que la IA tenga el contexto cronológico correcto.
    let full_history: Vec<ConvTurn> = recent
        .iter()
        .filter(|m| {
            // Excluir los que van a la ráfaga (inbounds con _id >= inbound_oid).
            !(m.direction == "in"
                && m.id.map(|i| i >= inbound_oid).unwrap_or(false))
        })
        .filter_map(|m| {
            let text = m.body.as_deref()?.trim().to_string();
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

    // ── Fresh-start detection ──────────────────────────────────────────────
    // Si la IA NUNCA respondió en esta conv y hay history previo (mensajes
    // de humanos / canales anteriores), recortamos el history para que el
    // modelo no copie el patrón conversacional que venía. Inyectamos un
    // bloque etiquetado `[ai_first_turn]` con el conteo previo — el
    // SUPERADMIN decide desde el system_prompt cómo reaccionar a ese flag.
    let prior_ai_turns = state
        .db
        .count_ai_interactions_for_conversation(&inbound.conversation_id)
        .await
        .unwrap_or(0);
    let prior_history_count = full_history.len();
    let is_ai_first_turn_with_prior_history =
        prior_ai_turns == 0 && prior_history_count > 0;

    let history: Vec<ConvTurn> = if is_ai_first_turn_with_prior_history {
        tracing::info!(
            "[ai_agent.dispatch] fresh-start detectado (conv={}, prior_messages={}); history recortado + counters reseteados",
            inbound.conversation_id.to_hex(),
            prior_history_count
        );
        // Reset counters per-conv: si la IA no había respondido todavía,
        // cualquier contador per-conv proviene de tests previos o estado
        // residual. La IA arranca limpia.
        state.redis.clear_ai_conv_counters(&inbound.conversation_id.to_hex()).await;
        Vec::new()
    } else {
        full_history
    };

    let first_turn_note_owned: Option<String> = if is_ai_first_turn_with_prior_history {
        Some(format!(
            "prior_messages_count: {}\nprior_handlers: humans_or_other_channels\nnote: Esta es la primera vez que un agente IA responde en esta conversación. Los mensajes previos no se incluyen en el history de este turno.",
            prior_history_count
        ))
    } else {
        None
    };

    // Burst: inbounds con `_id >= inbound_oid` en orden de _id ascendente.
    let burst: Vec<&WaMessage> = recent
        .iter()
        .filter(|m| {
            m.direction == "in" && m.id.map(|i| i >= inbound_oid).unwrap_or(false)
        })
        .collect();

    // Texto unificado de la ráfaga (4 mensajes seguidos del cliente se ven
    // como un solo input multilínea para la IA).
    let burst_texts: Vec<String> = burst
        .iter()
        .filter_map(|m| {
            let t = m.body.as_deref()?.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        })
        .collect();

    // Multimedia inline: descargamos el media de cada mensaje de la ráfaga
    // que tenga uno (Gemini soporta múltiples partes inline en un turno).
    let mut user_media: Vec<MediaInput> = Vec::new();
    for m in &burst {
        let mut chunks = build_media_inputs(&state, &wa_settings, m).await;
        user_media.append(&mut chunks);
    }

    if burst_texts.is_empty() && user_media.is_empty() {
        tracing::info!(
            "[ai_agent.dispatch] ráfaga sin contenido procesable (último msg_type={}); skip",
            inbound.msg_type
        );
        return Ok(());
    }
    let user_text = burst_texts.join("\n");

    // ── Per-conv: chequeo de turn limit ANTES del LLM ──────────────────────
    let conv_hex = inbound.conversation_id.to_hex();
    if agent.limits.max_turns_per_conversation > 0 {
        let turns = state.redis.get_ai_turns_conv(&conv_hex).await;
        if turns >= agent.limits.max_turns_per_conversation as i64 {
            tracing::warn!(
                "[ai_agent.dispatch] turn limit per conv reached (conv={}, turns={}, cap={}); auto-escalate",
                conv_hex, turns, agent.limits.max_turns_per_conversation
            );
            escalation::auto_escalate(
                &state,
                &inbound.conversation_id,
                &agent,
                escalation::REASON_CONVERSATION_TURN_LIMIT,
                Some("Límite de turnos por conversación alcanzado"),
                true,
            )
            .await;
            return Ok(());
        }
    }

    // ── Pre-LLM keyword escalation ─────────────────────────────────────────
    // Si `always_escalate_when_asked=true` y alguna keyword matchea con el
    // texto del cliente, escalamos sin gastar tokens.
    if agent.escalation.always_escalate_when_asked
        && matches_escalation_keyword(&user_text, &agent.escalation.keywords)
    {
        tracing::info!(
            "[ai_agent.dispatch] keyword match (conv={}); auto-escalate sin LLM",
            conv_hex
        );
        escalation::auto_escalate(
            &state,
            &inbound.conversation_id,
            &agent,
            escalation::REASON_KEYWORD_MATCHED,
            Some("Cliente pidió hablar con humano (keyword match)"),
            true,
        )
        .await;
        return Ok(());
    }

    // ── Pre-lookup del cliente por su número de teléfono ────────────────
    // Si el `conv.phone` matchea con un Cliente, inyectamos sus datos al
    // system_instruction. Así la IA sabe quién es sin pedir cédula y solo
    // la pide si NO se pudo identificar.
    let (customer_context, customer_first_name) =
        build_customer_context(&state, &conv.phone).await;

    let prompt_vars = build_prompt_variables(
        &agent,
        &wa_settings,
        &conv,
        customer_first_name.as_deref(),
    );

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

    let allowed_transfer_targets = extract_allowed_transfer_targets(&agent.tools);
    let agent_snapshot = Arc::new(agent.clone());
    let tool_ctx = ToolContext {
        state: state.clone(),
        workspace_id,
        business_phone: conv.business_phone.clone(),
        agent_id,
        conversation_id: Some(inbound.conversation_id),
        ai_user_id: agent.ai_user_id.clone(),
        ai_user_name: agent.personality.assistant_name.clone(),
        is_sandbox,
        allowed_transfer_targets,
        agent_snapshot: agent_snapshot.clone(),
        default_ticket_category_id: agent.escalation.default_ticket_category_id.clone(),
    };

    // Si no hay texto y solo hay media, mandamos un placeholder MUY neutro
    // (solo el tipo) para que la IA tenga un pivot. El comportamiento ante
    // adjuntos lo decide el SUPERADMIN en el system_prompt.
    let effective_user_message = if user_text.trim().is_empty() {
        format!("[attachment type={}]", inbound.msg_type)
    } else {
        user_text
    };

    let transfer_context_owned = conv.ai_transfer_context.clone();
    let output = match run_turn(
        &state.reqwest_client,
        &agent,
        &api_key,
        relay,
        &history,
        &effective_user_message,
        &user_media,
        faqs_inline.as_deref(),
        customer_context.as_deref(),
        transfer_context_owned.as_deref(),
        first_turn_note_owned.as_deref(),
        Some(&prompt_vars),
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

    // ── Post-turn counters: tokens + turnos diarios y per-conv ────────────
    let agent_hex = agent_id.to_hex();
    let total_tokens = output.total_tokens as u64;
    state.redis.add_ai_tokens_agent_daily(&agent_hex, total_tokens).await;
    state.redis.incr_ai_turns_agent_daily(&agent_hex).await;
    state.redis.incr_ai_turns_conv(&conv_hex).await;

    // ── Cost alert threshold (post-turn) ───────────────────────────────────
    if agent.limits.max_tokens_per_day > 0 && agent.limits.cost_alert_threshold_pct > 0 {
        let used = state.redis.get_ai_tokens_agent_daily(&agent_hex).await as u64;
        let cap = agent.limits.max_tokens_per_day;
        let pct_used = (used as f64 / cap as f64) * 100.0;
        if pct_used >= agent.limits.cost_alert_threshold_pct as f64 {
            if state.redis.try_mark_cost_alert_today(&agent_hex).await {
                tracing::warn!(
                    "[ai_agent.dispatch] cost alert: agent={} used {}/{} tokens ({:.1}% — threshold {}%)",
                    agent_hex, used, cap, pct_used, agent.limits.cost_alert_threshold_pct
                );
            }
        }
    }

    // ── max_identification_attempts: si el LLM llamó lookup_customer y volvió
    //    sin matches, contamos. Después de N attempts sin éxito, escalamos. ─
    if agent.escalation.max_identification_attempts > 0 {
        let had_failed_lookup = output.tool_calls.iter().any(|t| {
            t.tool_name == "lookup_customer"
                && t.success
                && t.result_summary.contains("\"items\":[]")
        });
        if had_failed_lookup {
            let attempts = state.redis.incr_ai_id_attempts(&conv_hex).await;
            if attempts >= agent.escalation.max_identification_attempts as i64 {
                tracing::info!(
                    "[ai_agent.dispatch] max_identification_attempts ({}) reached (conv={})",
                    attempts, conv_hex
                );
                escalation::auto_escalate(
                    &state,
                    &inbound.conversation_id,
                    &agent,
                    escalation::REASON_MAX_ID_ATTEMPTS,
                    Some("No fue posible identificar al cliente automáticamente"),
                    true,
                )
                .await;
                return Ok(());
            }
        }
    }

    // ── Resolución del turno ──────────────────────────────────────────────
    // Tools que cuentan como "resolución" del caso desde la perspectiva del
    // agente actual:
    // - request_human / create_ticket: la IA escala a humano (cierra ese path).
    // - transfer_to_agent: la IA pasa la conv a OTRO agente IA — desde la
    //   perspectiva del agente origen, este caso ya está resuelto. Además
    //   reseteamos counters per-conv para que el agente destino arranque
    //   limpio, no heredando los turns sin resolver del origen.
    let transfer_succeeded = output
        .tool_calls
        .iter()
        .any(|t| t.tool_name == "transfer_to_agent" && t.success);
    if transfer_succeeded {
        state.redis.clear_ai_conv_counters(&conv_hex).await;
        tracing::info!(
            "[ai_agent.dispatch] transfer_to_agent OK; counters per-conv reseteados (conv={})",
            conv_hex
        );
    }

    let resolved_now = transfer_succeeded
        || output
            .tool_calls
            .iter()
            .any(|t| {
                (t.tool_name == "request_human" || t.tool_name == "create_ticket") && t.success
            });

    // ── max_turns_without_resolution: cuenta turnos donde NO se llamó tool
    //    de cierre. Las tools de cierre (request_human / create_ticket /
    //    transfer_to_agent) cuentan como resolución. ─────────────────────
    if agent.escalation.max_turns_without_resolution > 0 && !resolved_now {
        let nr = state.redis.incr_ai_no_resolution(&conv_hex).await;
        let cap = agent.escalation.max_turns_without_resolution as i64;
        tracing::info!(
            "[ai_agent.dispatch] no_resolution counter (conv={}, count={}/{}, resolved_now=false)",
            conv_hex, nr, cap
        );
        if nr >= cap {
            tracing::info!(
                "[ai_agent.dispatch] max_turns_without_resolution ({}) reached (conv={})",
                nr, conv_hex
            );
            escalation::auto_escalate(
                &state,
                &inbound.conversation_id,
                &agent,
                escalation::REASON_NO_RESOLUTION,
                Some("Caso sin resolver tras varios turnos"),
                true,
            )
            .await;
            return Ok(());
        }
    }

    // ── escalate_on_critical_tool_failure: si alguna tool falló con error
    //    crítico (db_error, timeout, upstream), escalamos para no dejar al
    //    cliente sin respuesta calificada. ────────────────────────────────
    if agent.escalation.escalate_on_critical_tool_failure {
        let critical_failed = output.tool_calls.iter().any(|t| {
            !t.success
                && t.error
                    .as_deref()
                    .map(is_critical_tool_error)
                    .unwrap_or(false)
        });
        if critical_failed {
            tracing::warn!(
                "[ai_agent.dispatch] critical tool failure (conv={}); auto-escalate",
                conv_hex
            );
            escalation::auto_escalate(
                &state,
                &inbound.conversation_id,
                &agent,
                escalation::REASON_CRITICAL_TOOL_FAILURE,
                Some("Falla crítica de herramienta — IA no pudo continuar"),
                true,
            )
            .await;
            return Ok(());
        }
    }

    // Si consumimos transfer_context en este turno, limpiarlo para que no se
    // arrastre a turnos siguientes del mismo agente destino.
    if transfer_context_owned.is_some() {
        let clear = ConversationAiPatch {
            ai_active_agent_id: None,
            ai_disabled: None,
            ai_transfer_context: Some(None),
        };
        if let Err(e) = state
            .db
            .update_conversation_ai_state(&inbound.conversation_id, clear)
            .await
        {
            tracing::warn!("[ai_agent.dispatch] limpiar transfer_context: {}", e);
        }
    }

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

/// Pre-lookup automático del cliente por su número de WhatsApp. Devuelve
/// `(bloque_etiquetado, first_match_name)` — el bloque va al system_instruction;
/// el name lo usa la sustitución de placeholders (`{customer_name}`).
///
/// El back NO le dice a la IA qué hacer con los datos — el SUPERADMIN
/// configura el comportamiento desde `system_prompt` en el front.
async fn build_customer_context(
    state: &Arc<AppState>,
    customer_phone: &str,
) -> (Option<String>, Option<String>) {
    if customer_phone.trim().is_empty() {
        return (None, None);
    }
    let matches = match state
        .db
        .find_clients_for_ai_lookup(Some(customer_phone), None)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("[ai_agent.dispatch] lookup por phone falló: {}", e);
            return (None, None);
        }
    };

    let mut buf = String::new();
    buf.push_str(&format!("[customer_lookup_by_phone]\nphone: {}\n", customer_phone));
    if matches.is_empty() {
        buf.push_str("matches: 0\n");
        return (Some(buf), None);
    }
    buf.push_str(&format!("matches: {}\n", matches.len()));
    for (i, m) in matches.iter().enumerate() {
        buf.push_str(&format!(
            "  - [{}] client_id: {} | name: {} | identification: {} | status: {} | balance: {:.2}\n",
            i + 1,
            m.client_id,
            m.name.as_deref().unwrap_or(""),
            m.identification.as_deref().unwrap_or(""),
            m.status,
            m.balance,
        ));
    }
    let first_name = matches
        .first()
        .and_then(|m| m.name.as_ref())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    (Some(buf), first_name)
}

/// Construye los valores que sustituyen `{placeholders}` del `system_prompt`.
fn build_prompt_variables(
    agent: &AiAgent,
    wa_settings: &crate::models::whatsapp::WaSettings,
    conv: &crate::models::whatsapp::WaConversation,
    customer_first_name: Option<&str>,
) -> PromptVariables {
    use chrono::Datelike;
    let now = crate::utils::timezone::VenezuelaDateTime::now();
    let in_vz = now.in_venezuela();
    let weekday = match in_vz.weekday() {
        chrono::Weekday::Mon => "lunes",
        chrono::Weekday::Tue => "martes",
        chrono::Weekday::Wed => "miércoles",
        chrono::Weekday::Thu => "jueves",
        chrono::Weekday::Fri => "viernes",
        chrono::Weekday::Sat => "sábado",
        chrono::Weekday::Sun => "domingo",
    };
    PromptVariables {
        assistant_name: agent.personality.assistant_name.clone(),
        workspace_name: wa_settings.workspace_name.clone(),
        customer_name: customer_first_name.unwrap_or("").to_string(),
        customer_phone: conv.phone.clone(),
        business_phone: conv.business_phone.clone(),
        today: now.date_string_venezuela(),
        weekday: weekday.to_string(),
    }
}

/// Resuelve el agente IA que debería procesar este turno:
///
/// 1. Si la conv ya tiene `ai_active_agent_id` y ese agente sigue habilitado → ese.
/// 2. Sino, el `is_receptionist=true` enabled del workspace (si existe).
/// 3. Sino, el más viejo `enabled` del workspace (fallback compat).
async fn select_agent(
    state: &Arc<AppState>,
    conv: &crate::models::whatsapp::WaConversation,
    workspace_id: &ObjectId,
) -> Option<AiAgent> {
    if let Some(active) = conv.ai_active_agent_id {
        match state.db.find_ai_agent_by_id(&active).await {
            Ok(Some(a)) if a.enabled => return Some(a),
            Ok(_) => {
                tracing::debug!(
                    "[ai_agent.dispatch] ai_active_agent_id={} deshabilitado/borrado, fallback",
                    active.to_hex()
                );
            }
            Err(e) => {
                tracing::warn!("[ai_agent.dispatch] lookup active agent: {}", e);
            }
        }
    }

    if let Ok(Some(a)) = state.db.find_receptionist_for_workspace(workspace_id).await {
        return Some(a);
    }

    state
        .db
        .find_active_agent_for_workspace(workspace_id)
        .await
        .ok()
        .flatten()
}

/// Check de horario configurado en `agent.schedule`. Si `always_on=true`,
/// pasa siempre. Sino: weekday actual debe estar en `weekdays[]` y hora actual
/// en `[from_hour, to_hour]` inclusive.
///
/// `weekdays` usa convención ISO: 1=lunes, 7=domingo (igual que el front).
fn is_within_schedule(schedule: &crate::models::ai_agent::AiSchedule) -> bool {
    if schedule.always_on {
        return true;
    }
    use chrono::{Datelike, Timelike};
    let tz: chrono_tz::Tz = schedule
        .timezone
        .parse()
        .unwrap_or(chrono_tz::America::Caracas);
    let now = chrono::Utc::now().with_timezone(&tz);
    let weekday_iso = now.weekday().number_from_monday() as u8; // 1..=7
    if !schedule.weekdays.iter().any(|&d| d == weekday_iso) {
        return false;
    }
    let hour = now.hour() as u8;
    if schedule.from_hour <= schedule.to_hour {
        hour >= schedule.from_hour && hour <= schedule.to_hour
    } else {
        // Caso "horario que cruza medianoche" (ej: 22..=6).
        hour >= schedule.from_hour || hour <= schedule.to_hour
    }
}

/// Normaliza tildes/case y chequea si alguna keyword aparece como substring.
/// Devuelve `false` cuando `keywords` está vacío.
fn matches_escalation_keyword(text: &str, keywords: &[String]) -> bool {
    if keywords.is_empty() {
        return false;
    }
    let normalized = super::tools::normalize_zone(text);
    keywords.iter().any(|kw| {
        let nk = super::tools::normalize_zone(kw);
        !nk.is_empty() && normalized.contains(&nk)
    })
}

/// Errores de tool considerados "críticos" para el flag
/// `escalate_on_critical_tool_failure`. Los errores de validación
/// (`invalid_args:*`, `missing_*`) no escalan — la IA puede re-prompt y
/// reintentar.
fn is_critical_tool_error(err: &str) -> bool {
    err.starts_with("db_error:")
        || err.starts_with("timeout")
        || err.contains("upstream")
        || err.contains("connection")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{}…", cut)
    }
}
