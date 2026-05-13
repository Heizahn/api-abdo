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
        ai_agent::{AiAgent, AiAgentMode, AiAgentPurpose, AiInteraction},
        whatsapp::{StatePatch, WaConversationAiState, WaMessage},
    },
    state::AppState,
};

use super::{
    ai_agent_secret,
    config_resolver::resolve_ai_api_key,
    escalation, guardrails,
    openrouter::{resolve_base_url, AiRelay},
    pre_classifier::{self, PreClassResult, PreClassResultFull},
    runner::{run_turn, ConvRole, ConvTurn, MediaInput, PromptVariables},
    state::{apply_state_patches, format_conversation_state},
    tools::{extract_allowed_transfer_targets, ToolContext},
};

/// Cuántos mensajes leemos hacia atrás para armar history + ráfaga. Más bajo
/// = menos tokens de input por turno (menor costo) pero menos contexto. 20
/// turnos cubren bien una conversación típica de WhatsApp; conversaciones
/// largas ya pasan por handoff/escalación antes de llegar al cap.
const RECENT_WINDOW: i64 = 20;

/// Fallback cuando el agente no tiene `debounce_seconds` (compat con docs
/// viejos antes del campo). El default real es 10s y vive en el agente.
const DEBOUNCE_FALLBACK_SECONDS: u64 = 10;

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
pub fn dispatch_inbound_async(state: Arc<AppState>, inbound: WaMessage, workspace_id: ObjectId) {
    let conv_hex = inbound.conversation_id.to_hex();
    let now_ms = chrono::Utc::now().timestamp_millis();

    tokio::spawn(async move {
        // Marcar como "actividad reciente". El próximo inbound sobreescribe.
        state.redis.set_ai_debounce_ts(&conv_hex, now_ms).await;

        // Cargar la conv y resolver agente según su estado IA. Si la conv
        // tiene `ai_disabled=true`, NO procesamos (un humano la atiende).
        let conv = match state
            .db
            .find_conversation_by_id(&inbound.conversation_id)
            .await
        {
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
        if conv.status == "in_progress" {
            tracing::debug!(
                "[ai_agent.dispatch] conv {} con status=in_progress (humano atendiendo), skip",
                conv_hex
            );
            return;
        }

        let reopen_from_state_early = conv
            .ai_conv_state
            .as_ref()
            .map(|s| s.reopen_pending)
            .unwrap_or(false);
        let agent = match select_agent(&state, &conv, &workspace_id, reopen_from_state_early).await {
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
    if conv.status == "in_progress" {
        tracing::debug!(
            "[ai_agent.dispatch] conv {} pasó a in_progress durante debounce (humano la tomó), skip",
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
    // en el back, no `timestamp` de Meta).
    //
    // La ráfaga son TODOS los inbounds que llegaron después del último outbound
    // del bot (o todos si no hay outbound). Esto cubre el caso en que el cliente
    // manda imagen + texto en rápida sucesión: el debounce cancela el spawn de la
    // imagen y solo dispara el del texto, pero la imagen debe quedar en la ráfaga
    // para que el LLM la reciba como contenido visual.
    let inbound_oid = match inbound.id {
        Some(o) => o,
        None => return Err("inbound sin _id".into()),
    };
    let recent = state
        .db
        .list_recent_messages_for_conversation(&inbound.conversation_id, RECENT_WINDOW)
        .await?;

    // Último outbound del bot: define el inicio lógico de la ráfaga actual.
    let last_out_oid: Option<ObjectId> = recent
        .iter()
        .filter(|m| m.direction == "out")
        .filter_map(|m| m.id)
        .max();

    // ── Guardrail data + turn-state HUD (precomputado por turno) ────────────
    // Vive a este nivel porque NO cambia entre iteraciones del chain de
    // transfer (zones / media_ids / intents son función del cliente, no del
    // agente activo). turn_number también se computa una vez por turno.
    let customer_explicit_zones = guardrails::extract_customer_explicit_zones(&recent);
    // NOTA: media_ids se computan DESPUÉS del burst (ver abajo) para que solo
    // incluyan IDs del turno actual, no imágenes de sesiones anteriores.
    let customer_explicit_intents = guardrails::extract_customer_explicit_intents(&recent);

    // History: todo lo que NO está en la ráfaga. Incluye outbounds del bot para
    // que la IA tenga el contexto cronológico correcto.
    let full_history: Vec<ConvTurn> = recent
        .iter()
        .filter(|m| {
            // Excluir inbounds que forman la ráfaga (llegaron después del último outbound).
            if m.direction != "in" {
                return true;
            }
            match last_out_oid {
                Some(out_id) => m.id.map(|i| i <= out_id).unwrap_or(false),
                None => false, // sin outbound previo → todo es ráfaga
            }
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

    // turn_state_owned se computa más abajo, después del burst y recent_media_ids.

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
    let is_ai_first_turn_with_prior_history = prior_ai_turns == 0 && prior_history_count > 0;

    let history: Vec<ConvTurn> = if is_ai_first_turn_with_prior_history {
        tracing::info!(
            "[ai_agent.dispatch] fresh-start detectado (conv={}, prior_messages={}); history recortado + counters reseteados",
            inbound.conversation_id.to_hex(),
            prior_history_count
        );
        // Reset counters per-conv: si la IA no había respondido todavía,
        // cualquier contador per-conv proviene de tests previos o estado
        // residual. La IA arranca limpia.
        state
            .redis
            .clear_ai_conv_counters(&inbound.conversation_id.to_hex())
            .await;
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

    // Detecta reopen: la IA ya respondió antes (prior_ai_turns > 0) pero
    // ai_conv_state fue limpiado (reopen). El agente activo puede ser distinto
    // Reopen detection: si la conversación fue cerrada y reabierta con history
    // previo, truncamos el history a [] antes de pasarlo al LLM.
    //
    // Garantía dura: sin history no hay datos viejos que alucinar. El agente
    // arranca limpio con solo el mensaje actual del cliente.
    //
    // `is_reopen_with_prior_ai` = Turn 1 (ai_conv_state era None).
    // `reopen_from_state`       = Turn 2+ (flag persistido en WaConversationAiState).
    // Ambos aplican el truncado — el flag se limpia al finalizar Turn 2.
    let is_reopen_with_prior_ai =
        prior_ai_turns > 0 && conv.ai_conv_state.is_none() && !history.is_empty();
    let reopen_from_state = conv
        .ai_conv_state
        .as_ref()
        .map(|s| s.reopen_pending)
        .unwrap_or(false);
    let should_truncate_history = is_reopen_with_prior_ai || reopen_from_state;

    let history = if should_truncate_history {
        tracing::info!(
            "[ai_agent.dispatch] reopen detectado — history truncado (conv={}, prior_ai_turns={}, turns_descartados={}, from_state={})",
            inbound.conversation_id.to_hex(),
            prior_ai_turns,
            history.len(),
            reopen_from_state,
        );
        vec![]
    } else {
        history
    };

    // Burst: todos los inbounds que llegaron después del último outbound del bot,
    // ordenados por _id ascendente. Garantiza que imagen + texto enviados en
    // ráfaga rápida lleguen juntos al LLM aunque el debounce haya cancelado
    // el spawn de la imagen.
    // Fallback: si no hay outbound, incluir todos los inbounds de la ventana.
    // Siempre incluimos al menos el inbound que disparó este dispatch (inbound_oid).
    let burst: Vec<&WaMessage> = recent
        .iter()
        .filter(|m| {
            if m.direction != "in" {
                return false;
            }
            match last_out_oid {
                Some(out_id) => m.id.map(|i| i > out_id).unwrap_or(false),
                None => true,
            }
        })
        .collect();

    // Media IDs del burst actual — solo los del turno corriente para evitar que
    // el LLM elija imágenes de sesiones anteriores que siguen en la ventana de 20.
    let recent_media_ids: Vec<String> = {
        let mut seen = std::collections::LinkedList::new();
        let mut dedup = std::collections::HashSet::new();
        for m in &burst {
            if let Some(mid) = m.media_id.as_deref() {
                let mid = mid.trim();
                if !mid.is_empty() && dedup.insert(mid.to_string()) {
                    seen.push_back(mid.to_string());
                }
            }
        }
        seen.into_iter().collect()
    };

    // High water mark: el _id más alto que la IA "vio" en su prompt. Empieza
    // siendo el max del burst inicial; si hay chain reload, se actualiza con
    // el max del nuevo burst. Al final del dispatch lo usamos para detectar
    // mensajes que llegaron DURANTE el LLM call y no fueron procesados.
    let mut high_water_mark: ObjectId = burst
        .iter()
        .filter_map(|m| m.id)
        .max()
        .unwrap_or(inbound_oid);

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

    // turn_state_owned se computa con el `history` final (post fresh-start/reopen)
    // y los media_ids del burst actual — garantiza que turn_number y available_media_ids
    // sean consistentes con lo que realmente ve el LLM.
    let turn_state_owned: Option<String> = guardrails::build_turn_state(
        &history,
        &customer_explicit_zones,
        &customer_explicit_intents,
        &recent_media_ids,
    );

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
    let (customer_context, customer_first_name) = build_customer_context(&state, &conv.phone).await;

    // ── Phase 3a: Pre-classifier gate ─────────────────────────────────────
    // Solo si el workspace lo habilitó y hay texto del cliente.
    // Corre con la api_key del agente inicial (flash-lite es un modelo aparte).
    // Si no hay api_key disponible o el classify falla, se salta silenciosamente.
    //
    // `pre_class_result`   — para auditoría en AiInteraction (turno LLM normal).
    // `pre_class_specialist` — (AiAgent, api_key) cuando Clear* encuentra un
    //                          especialista distinto; reemplaza `agent` en el loop.
    let mut pre_class_result: Option<PreClassResultFull> = None;
    let mut pre_class_specialist: Option<(AiAgent, String)> = None;

    if wa_settings.pre_classifier_enabled && !user_text.trim().is_empty() {
        let pc_api_key_result = resolve_ai_api_key(&state).await;
        let pc_api_key_opt = match pc_api_key_result {
            Ok(k) => Some(k),
            Err(ref e) if matches!(e, crate::error::ApiError::Domain { code, .. } if code == "ai_global_config_missing") =>
            {
                tracing::debug!(
                    "[ai_agent.dispatch] pre-classifier skipped: global config missing"
                );
                None
            }
            Err(_) => None,
        };
        if let Some(pc_api_key) = pc_api_key_opt {
            let relay_for_pc = AiRelay::from_config(&state.config);
            let pc_ctx = pre_classifier::PreClassifierContext {
                api_key: &pc_api_key,
                relay: relay_for_pc.as_ref(),
                base_url: resolve_base_url(),
                http: &state.reqwest_client,
            };
            let summary = build_customer_summary_short(&customer_context);
            match pre_classifier::classify(&user_text, &summary, &pc_ctx).await {
                Ok(result) => {
                    tracing::info!(
                        "[ai_agent.dispatch] pre_class: variant={} gated={} confidence={:.2} latency={}ms (conv={})",
                        result.variant.as_str(),
                        result.gated_variant.as_str(),
                        result.confidence,
                        result.latency_ms,
                        conv_hex
                    );

                    let text_norm = super::tools::normalize_zone(&user_text);
                    match result.gated_variant {
                        // ── Spam: respuesta trivial (opcional) + silencio ─
                        PreClassResult::Spam => {
                            if let Some(trivial) =
                                pick_trivial(&wa_settings.trivial_responses, "spam", &text_norm)
                            {
                                if is_live {
                                    if let Err(e) = send_live_response(
                                        &state,
                                        &wa_settings,
                                        &conv.phone,
                                        &agent.ai_user_id,
                                        inbound.conversation_id,
                                        &trivial.response,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            "[ai_agent.dispatch] spam trivial send falló: {}",
                                            e
                                        );
                                    }
                                } else {
                                    tracing::info!(
                                        "[ai_agent.dispatch] shadow+spam → habría respondido: {}",
                                        truncate(&trivial.response, 200)
                                    );
                                }
                            }
                            persist_pre_class_only_interaction(
                                &state,
                                inbound.conversation_id,
                                inbound.id.unwrap_or_else(ObjectId::new),
                                workspace_id,
                                agent_id,
                                &agent.model.model_id,
                                &result,
                            )
                            .await;
                            return Ok(());
                        }

                        // ── GreetingOnly: respuesta trivial o fallthrough ──
                        PreClassResult::GreetingOnly => {
                            if let Some(trivial) =
                                pick_trivial(&wa_settings.trivial_responses, "greeting", &text_norm)
                            {
                                if is_live {
                                    if let Err(e) = send_live_response(
                                        &state,
                                        &wa_settings,
                                        &conv.phone,
                                        &agent.ai_user_id,
                                        inbound.conversation_id,
                                        &trivial.response,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            "[ai_agent.dispatch] greeting trivial send falló: {}",
                                            e
                                        );
                                    }
                                } else {
                                    tracing::info!(
                                        "[ai_agent.dispatch] shadow+greeting → habría respondido: {}",
                                        truncate(&trivial.response, 200)
                                    );
                                }
                                persist_pre_class_only_interaction(
                                    &state,
                                    inbound.conversation_id,
                                    inbound.id.unwrap_or_else(ObjectId::new),
                                    workspace_id,
                                    agent_id,
                                    &agent.model.model_id,
                                    &result,
                                )
                                .await;
                                return Ok(());
                            }
                            // Sin plantilla de saludo: cae al LLM con auditoría.
                            pre_class_result = Some(result);
                        }

                        // ── Clear*: buscar especialista y redirigir ────────
                        PreClassResult::ClearVentas
                        | PreClassResult::ClearPagos
                        | PreClassResult::ClearSoporte => {
                            let purpose = match result.gated_variant {
                                PreClassResult::ClearVentas => AiAgentPurpose::Ventas,
                                PreClassResult::ClearPagos => AiAgentPurpose::Pagos,
                                _ => AiAgentPurpose::Soporte,
                            };
                            match state
                                .db
                                .find_active_agent_by_workspace_and_purpose(&workspace_id, purpose)
                                .await
                            {
                                Ok(Some(specialist))
                                    if specialist.id.map(|id| id != agent_id).unwrap_or(false) =>
                                {
                                    let spec_id = specialist.id;
                                    tracing::info!(
                                        "[ai_agent.dispatch] pre_class Clear*: redirigiendo a specialist={} purpose={:?} (conv={})",
                                        spec_id.map(|o| o.to_hex()).unwrap_or_default(),
                                        purpose,
                                        conv_hex
                                    );
                                    // Persistir ai_active_agent_id para que el próximo
                                    // dispatch (si ocurre antes del loop actual) ya sepa
                                    // cuál es el agente activo.
                                    let patch = ConversationAiPatch {
                                        ai_active_agent_id: spec_id.as_ref().map(Some),
                                        ai_disabled: None,
                                        ai_transfer_context: None,
                                    };
                                    if let Err(e) = state
                                        .db
                                        .update_conversation_ai_state(
                                            &inbound.conversation_id,
                                            patch,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            "[ai_agent.dispatch] set specialist agent in conv falló: {}",
                                            e
                                        );
                                    }
                                    // Obtener api_key global para el specialist.
                                    match resolve_ai_api_key(&state).await {
                                        Ok(spec_key) => {
                                            pre_class_specialist = Some((specialist, spec_key));
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "[ai_agent.dispatch] global api_key indisponible para specialist: {:?}; usando agente original",
                                                e
                                            );
                                            // Config global no disponible: cae al agente original.
                                        }
                                    }
                                    pre_class_result = Some(result);
                                }
                                Ok(_) => {
                                    // Sin especialista distinto: cae al agente original.
                                    tracing::debug!(
                                        "[ai_agent.dispatch] pre_class Clear* sin specialist para {:?} en workspace={}, fallthrough",
                                        purpose,
                                        workspace_id.to_hex()
                                    );
                                    pre_class_result = Some(result);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "[ai_agent.dispatch] find specialist error: {}; fallthrough",
                                        e
                                    );
                                    pre_class_result = Some(result);
                                }
                            }
                        }

                        // ── Ambiguous: auditoría, flujo normal ────────────
                        PreClassResult::Ambiguous => {
                            pre_class_result = Some(result);
                        }
                    }
                }
                Err(e) => {
                    // Error de red/parse → skip silencioso.
                    tracing::warn!(
                        "[ai_agent.dispatch] pre_classifier error (skipping gate): {}",
                        e
                    );
                }
            }
        }
        // Sin config global → skip silencioso del pre-clasificador.
    }

    let prompt_vars =
        build_prompt_variables(&agent, &wa_settings, &conv, customer_first_name.as_deref());

    let api_key = match resolve_ai_api_key(&state).await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(
                "[ai_agent.dispatch] global api_key indisponible (agent={}): {:?}",
                agent_id.to_hex(),
                e
            );
            return Ok(());
        }
    };

    // FAQs y faqs_inline se cargan dentro del loop por iteración (cada agente
    // tiene sus FAQs propias). No las pre-computamos acá.

    let relay_owned = AiRelay::from_config(&state.config);
    let relay = relay_owned.as_ref();

    // Si no hay texto y solo hay media, mandamos un placeholder MUY neutro
    // (solo el tipo) para que la IA tenga un pivot. El comportamiento ante
    // adjuntos lo decide el SUPERADMIN en el system_prompt.
    //
    // `mut` porque al iniciar chain_count > 0 (handoff a otro agente) recargamos
    // mensajes del cliente que pudieron haber llegado durante la iteración
    // anterior. Sin esto, mensajes que arriban mientras Sofía está en su LLM
    // call quedan huérfanos y Carla solo ve el primer mensaje.
    let mut effective_user_message = if user_text.trim().is_empty() {
        format!("[attachment type={}]", inbound.msg_type)
    } else {
        user_text
    };
    let mut customer_explicit_zones = customer_explicit_zones;
    let mut recent_media_ids = recent_media_ids;

    // ── Loop de dispatch con chain de transfers ─────────────────────────────
    // Cuando un agente del MISMO workspace llama `transfer_to_agent`, el
    // handoff es silencioso: el dispatch re-corre `run_turn` con el target
    // sobre el mismo mensaje del cliente, y el cliente recibe SOLO la
    // respuesta del agente final. Cap a `MAX_TRANSFER_CHAIN` para evitar
    // loops (Sofía → Carla → Gabriel = 2 transfers = chain_count=2 al
    // hitting). Si se excede, escalamos a humano.
    //
    // Cuando el target es de OTRO workspace, NO podemos pasarle la conv
    // (cliente está chateando contra otro número). El tool genera un
    // `client_message` ("escribí al +58 YYY") que se envía al cliente como
    // respuesta visible y el chain termina.
    const MAX_TRANSFER_CHAIN: u32 = 2;
    let initial_transfer_context_owned = conv.ai_transfer_context.clone();

    // ── Phase 2: conversation state ────────────────────────────────────────
    // current_ai_conv_state: snapshot al inicio del dispatch (antes del chain).
    // Aplica solo si el kill switch del workspace está activo (per-workspace
    // toggle vía UI SUPERADMIN — los agentes acatan la política del workspace
    // al que pertenecen, NO la suya propia).
    let current_ai_conv_state: Option<WaConversationAiState> =
        if wa_settings.enable_conversation_state {
            conv.ai_conv_state.clone()
        } else {
            None
        };
    // conversation_state_owned: el bloque [conversation_state] formateado para
    // inyectar en el system_instruction del PRIMER agente del chain. En chain
    // steps > 0, no lo inyectamos (el target arranca sin estado del anterior).
    let conversation_state_owned: Option<String> = current_ai_conv_state
        .as_ref()
        .map(format_conversation_state);
    // Acumulador de todos los patches emitidos durante el chain completo.
    let mut all_state_patches: Vec<StatePatch> = Vec::new();

    // Si el pre-clasificador encontró un especialista, usar ese en vez del agente inicial.
    let (mut active_agent, mut active_api_key): (AiAgent, String) =
        if let Some((specialist, spec_key)) = pre_class_specialist {
            (specialist, spec_key)
        } else {
            (agent, api_key)
        };
    let mut active_transfer_context: Option<String> = initial_transfer_context_owned.clone();
    let mut chain_count: u32 = 0;
    let last_output: Option<crate::modules::ai_agent::runner::RunnerOutput>;
    let last_agent: Option<AiAgent>;
    let mut cross_workspace_message: Option<String> = None;

    loop {
        let active_agent_id = active_agent.id.ok_or_else(|| "agent sin _id".to_string())?;

        if chain_count > 0 {
            tracing::info!(
                "[ai_agent.dispatch] chain step {}: agent={} (label={}, mode={:?}) procesando conv={}",
                chain_count,
                active_agent_id.to_hex(),
                active_agent.label,
                active_agent.mode,
                inbound.conversation_id.to_hex()
            );

            // Re-fetch recent: durante la iteración anterior pudieron llegar
            // mensajes nuevos del cliente (debounce + lock activo descartó sus
            // dispatches individuales). El target del chain debe verlos para
            // responder a TODA la ráfaga, no solo al mensaje que originó el
            // dispatch. Si recent no cambió, no actualizamos nada.
            match state
                .db
                .list_recent_messages_for_conversation(&inbound.conversation_id, RECENT_WINDOW)
                .await
            {
                Ok(refreshed) => {
                    // Mismo criterio que el burst inicial: todos los inbounds
                    // desde el último outbound (no solo >= inbound_oid).
                    let refreshed_last_out_oid: Option<ObjectId> = refreshed
                        .iter()
                        .filter(|m| m.direction == "out")
                        .filter_map(|m| m.id)
                        .max();
                    let new_burst: Vec<&WaMessage> = refreshed
                        .iter()
                        .filter(|m| {
                            if m.direction != "in" { return false; }
                            match refreshed_last_out_oid {
                                Some(out_id) => m.id.map(|i| i > out_id).unwrap_or(false),
                                None => true,
                            }
                        })
                        .collect();
                    let new_burst_texts: Vec<String> = new_burst
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
                    if !new_burst_texts.is_empty() {
                        let new_user_text = new_burst_texts.join("\n");
                        if new_user_text != effective_user_message {
                            tracing::info!(
                                "[ai_agent.dispatch] burst recargado en chain step {} ({} mensajes en ráfaga, prev={} chars, new={} chars)",
                                chain_count,
                                new_burst_texts.len(),
                                effective_user_message.len(),
                                new_user_text.len()
                            );
                            effective_user_message = new_user_text;
                            customer_explicit_zones =
                                guardrails::extract_customer_explicit_zones(&refreshed);
                            // Media IDs: solo del burst recargado, misma lógica que burst inicial.
                            recent_media_ids = {
                                let mut seen = std::collections::LinkedList::new();
                                let mut dedup = std::collections::HashSet::new();
                                for m in &new_burst {
                                    if let Some(mid) = m.media_id.as_deref() {
                                        let mid = mid.trim();
                                        if !mid.is_empty() && dedup.insert(mid.to_string()) {
                                            seen.push_back(mid.to_string());
                                        }
                                    }
                                }
                                seen.into_iter().collect()
                            };
                        }
                    }
                    // Actualizar HWM al máximo _id del burst refrescado —
                    // así el follow-up post-dispatch sabe hasta dónde
                    // llegó la cadena de chain reloads.
                    if let Some(new_hwm) = new_burst.iter().filter_map(|m| m.id).max() {
                        if new_hwm > high_water_mark {
                            high_water_mark = new_hwm;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "[ai_agent.dispatch] no pude recargar recent en chain step {} (conv={}): {}",
                        chain_count,
                        inbound.conversation_id.to_hex(),
                        e
                    );
                }
            }
        }

        // Tools del agente activo (cada agente tiene su propia config).
        let allowed_transfer_targets = extract_allowed_transfer_targets(&active_agent.tools);
        let transfer_target_labels: Vec<(ObjectId, String)> = if allowed_transfer_targets.is_empty()
        {
            Vec::new()
        } else {
            match state
                .db
                .find_ai_agents_by_ids(&allowed_transfer_targets)
                .await
            {
                Ok(agents) => agents
                    .into_iter()
                    .filter_map(|a| a.id.map(|id| (id, a.label)))
                    .collect(),
                Err(e) => {
                    tracing::warn!(
                        "[ai_agent.dispatch] no pude resolver labels de transfer_targets: {} — la IA recibe enum sin mapeo",
                        e
                    );
                    Vec::new()
                }
            }
        };
        let agent_snapshot = Arc::new(active_agent.clone());
        let tool_ctx = ToolContext {
            state: state.clone(),
            workspace_id,
            business_phone: conv.business_phone.clone(),
            agent_id: active_agent_id,
            conversation_id: Some(inbound.conversation_id),
            ai_user_id: active_agent.ai_user_id.clone(),
            ai_user_name: active_agent.personality.assistant_name.clone(),
            is_sandbox,
            allowed_transfer_targets,
            transfer_target_labels,
            agent_snapshot: agent_snapshot.clone(),
            default_ticket_category_id: active_agent.escalation.default_ticket_category_id.clone(),
            customer_explicit_zones: customer_explicit_zones.clone(),
            recent_media_ids: recent_media_ids.clone(),
            workspace_enable_guardrails: wa_settings.enable_guardrails,
            customer_phone: conv.phone.clone(),
        };

        // FAQs y prompt_vars del agente activo (assistant_name cambia entre
        // Sofía y Carla, por ejemplo).
        let active_faqs = state
            .db
            .list_ai_agent_faqs(&active_agent_id)
            .await
            .unwrap_or_default();
        let active_faqs_inline = if active_faqs.is_empty() {
            None
        } else {
            let mut buf = String::new();
            for f in &active_faqs {
                buf.push_str("Q: ");
                buf.push_str(&f.question);
                buf.push_str("\nA: ");
                buf.push_str(&f.answer);
                buf.push_str("\n\n");
            }
            Some(buf)
        };
        let active_prompt_vars = if chain_count == 0 {
            prompt_vars.clone()
        } else {
            let mut p = prompt_vars.clone();
            p.assistant_name = active_agent.personality.assistant_name.clone();
            p
        };

        // first_turn_note y reopen_note solo en el primer turno del chain.
        let ftn_for_iter = if chain_count == 0 {
            first_turn_note_owned.as_deref()
        } else {
            None
        };
        // History ya fue truncado a [] si should_truncate_history — no hay nota
        // de reopen que inyectar: sin history viejo no hay qué advertir.

        // agent_state: chequeamos si el agente activo ya respondió antes en
        // esta conv. Si sí, le inyectamos un bloque para que sepa que ya
        // saludó y no repita "¡Hola! Soy Carla..." en cada turno.
        // En chain_count > 0 es siempre el primer turno del target → no
        // aplica (todavía no respondió). Solo computamos para chain_count=0.
        let agent_state_owned: Option<String> = if chain_count == 0 {
            match state
                .db
                .count_ai_interactions_for_agent_in_conv(
                    &inbound.conversation_id,
                    &active_agent_id,
                )
                .await
            {
                Ok(n) if n > 0 => Some(format!(
                    "already_greeted: true\nprior_turns_in_this_conv: {}\nnote: Ya respondiste antes en esta conversación. NO repitas el saludo \"¡Hola, soy {}...\" — retomá el hilo desde donde quedó. Si necesitás info que falta, preguntá directo sin presentarte de nuevo.",
                    n,
                    active_agent.personality.assistant_name,
                )),
                _ => None,
            }
        } else {
            None
        };

        // Resuelve base URL efectiva: agente override → default OpenRouter.
        let effective_base_url = resolve_base_url();

        let output = match run_turn(
            &state.reqwest_client,
            &active_agent,
            &active_api_key,
            relay,
            &effective_base_url,
            &history,
            &effective_user_message,
            &user_media,
            active_faqs_inline.as_deref(),
            customer_context.as_deref(),
            active_transfer_context.as_deref(),
            ftn_for_iter,
            None, // reopen_note: history truncado en reopen — nota redundante
            agent_state_owned.as_deref(),
            turn_state_owned.as_deref(),
            // Inyectamos el estado solo en el primer paso del chain (chain_count==0).
            // En pasos > 0 el target arranca sin estado heredado del anterior.
            if chain_count == 0 {
                conversation_state_owned.as_deref()
            } else {
                None
            },
            Some(&active_prompt_vars),
            &tool_ctx,
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    "[ai_agent.dispatch] runner error (agent={}, conv={}): {:?}",
                    active_agent_id.to_hex(),
                    inbound.conversation_id.to_hex(),
                    e
                );
                return Ok(());
            }
        };

        tracing::info!(
            "[ai_agent.dispatch] turno OK (agent={}, conv={}, mode={:?}, escalated={}, in_tokens={}, out_tokens={}, thinking_tokens={}, latency={}ms, chain_step={})",
            active_agent_id.to_hex(),
            inbound.conversation_id.to_hex(),
            active_agent.mode,
            output.escalated,
            output.input_tokens,
            output.output_tokens,
            output.thinking_tokens,
            output.latency_ms,
            chain_count
        );

        // Acumular patches de este turno.
        all_state_patches.extend(output.state_patches.iter().cloned());

        // Persistir AiInteraction de esta iteración (auditoría completa por
        // turno — cada agente queda con su propio registro).
        // Solo pasamos pre_class_result en el primer paso del chain (el pre-clasificador
        // corrió sobre el turno completo, no sobre cada agente individual del chain).
        let interaction = output.to_interaction(
            inbound.conversation_id,
            inbound.id.unwrap_or_else(ObjectId::new),
            workspace_id,
            active_agent_id,
            chain_count,
            &active_agent.model.model_id,
            if chain_count == 0 {
                pre_class_result.as_ref()
            } else {
                None
            },
        );
        if let Err(e) = state.db.create_ai_interaction(interaction).await {
            tracing::warn!("[ai_agent.dispatch] persistir AiInteraction falló: {}", e);
        }

        // Tokens daily + turns daily del agente activo.
        let active_agent_hex = active_agent_id.to_hex();
        let total_tokens = output.total_tokens as u64;
        state
            .redis
            .add_ai_tokens_agent_daily(&active_agent_hex, total_tokens)
            .await;
        state
            .redis
            .incr_ai_turns_agent_daily(&active_agent_hex)
            .await;

        // Cost alert por agente.
        if active_agent.limits.max_tokens_per_day > 0
            && active_agent.limits.cost_alert_threshold_pct > 0
        {
            let used = state
                .redis
                .get_ai_tokens_agent_daily(&active_agent_hex)
                .await as u64;
            let cap = active_agent.limits.max_tokens_per_day;
            let pct_used = (used as f64 / cap as f64) * 100.0;
            if pct_used >= active_agent.limits.cost_alert_threshold_pct as f64 {
                if state
                    .redis
                    .try_mark_cost_alert_today(&active_agent_hex)
                    .await
                {
                    tracing::warn!(
                        "[ai_agent.dispatch] cost alert: agent={} used {}/{} tokens ({:.1}% — threshold {}%)",
                        active_agent_hex, used, cap, pct_used,
                        active_agent.limits.cost_alert_threshold_pct
                    );
                }
            }
        }

        // ¿Hubo transfer? Decidir si seguimos en chain o salimos.
        if let Some(transfer) = output.transfer.clone() {
            if transfer.cross_workspace {
                tracing::info!(
                    "[ai_agent.dispatch] transfer cross-workspace (target={}); enviaré client_message al cliente",
                    transfer.target_agent_id.to_hex()
                );
                cross_workspace_message = transfer.client_message.clone();
                last_output = Some(output);
                last_agent = Some(active_agent);
                break;
            }

            // Same workspace: re-correr con el target.
            chain_count += 1;
            if chain_count >= MAX_TRANSFER_CHAIN {
                tracing::warn!(
                    "[ai_agent.dispatch] MAX_TRANSFER_CHAIN ({}) alcanzado (conv={}); escalando a humano",
                    MAX_TRANSFER_CHAIN, conv_hex
                );
                escalation::auto_escalate(
                    &state,
                    &inbound.conversation_id,
                    &active_agent,
                    escalation::REASON_NO_RESOLUTION,
                    Some("Cadena de transfers superó el cap — derivar a humano"),
                    true,
                )
                .await;
                return Ok(());
            }

            let target = match state
                .db
                .find_ai_agent_by_id(&transfer.target_agent_id)
                .await
            {
                Ok(Some(a)) if a.enabled => a,
                _ => {
                    tracing::warn!(
                        "[ai_agent.dispatch] target_agent {} no disponible (deshabilitado o no existe); escalando a humano",
                        transfer.target_agent_id.to_hex()
                    );
                    escalation::auto_escalate(
                        &state,
                        &inbound.conversation_id,
                        &active_agent,
                        escalation::REASON_CRITICAL_TOOL_FAILURE,
                        Some("Agente destino del transfer no está disponible"),
                        true,
                    )
                    .await;
                    return Ok(());
                }
            };

            // api_key global (compartida por todos los agentes). Si no está
            // configurada, escalamos a humano.
            let target_api_key = match resolve_ai_api_key(&state).await {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(
                        "[ai_agent.dispatch] global api_key indisponible para target {}: {:?}; escalando a humano",
                        transfer.target_agent_id.to_hex(),
                        e
                    );
                    escalation::auto_escalate(
                        &state,
                        &inbound.conversation_id,
                        &active_agent,
                        escalation::REASON_CRITICAL_TOOL_FAILURE,
                        Some("Agente destino del transfer sin api_key válida"),
                        true,
                    )
                    .await;
                    return Ok(());
                }
            };

            active_transfer_context = if transfer.reason.trim().is_empty() {
                None
            } else {
                Some(transfer.reason)
            };
            active_agent = target;
            active_api_key = target_api_key;
            // continue
        } else {
            last_output = Some(output);
            last_agent = Some(active_agent);
            break;
        }
    }

    let last_output = last_output.expect("loop debe setear last_output antes de break");
    let last_agent = last_agent.expect("loop debe setear last_agent antes de break");
    let last_agent_id = last_agent
        .id
        .ok_or_else(|| "last_agent sin _id".to_string())?;

    // ── Phase 2: fold state patches + persistir ai_conv_state ─────────────
    // Synthetic intent derivation (Spec 19.3): si el state aún no tiene
    // current_intent Y los guardrails detectaron al menos un intent en los
    // mensajes del cliente, inyectamos un SetIntent al frente de los patches.
    // Esto da `current_intent` determinístico desde el primer mensaje sin
    // pedirle al modelo que se auto-clasifique.
    let mut all_state_patches = all_state_patches; // shadow to allow prepend
                                                   // Kill switch vive per-workspace en `WaSettings.enable_conversation_state`
                                                   // (configurable desde UI SUPERADMIN). Los agentes del workspace acatan
                                                   // la política — un workspace de test puede correr sin state mientras prod
                                                   // lo mantiene activo.
    if wa_settings.enable_conversation_state {
        let base_intent = current_ai_conv_state
            .as_ref()
            .and_then(|s| s.current_intent.as_ref());
        if base_intent.is_none() && !customer_explicit_intents.is_empty() {
            all_state_patches.insert(
                0,
                crate::models::whatsapp::StatePatch::SetIntent {
                    intent: customer_explicit_intents[0].clone(),
                    confidence: 1.0,
                },
            );
        }
    }

    // `reopen_pending_next`: qué valor escribir en el flag para el próximo turno.
    // - Turn 1 (is_reopen_with_prior_ai): seteamos true → Turn 2 también trunca el history.
    // - Turn 2+ (reopen_from_state): seteamos false → la sesión queda normalizada.
    // - Sin reopen: None → no tocamos el flag.
    let reopen_pending_next: Option<bool> = if is_reopen_with_prior_ai {
        Some(true)
    } else if reopen_from_state {
        Some(false)
    } else {
        None
    };

    let needs_state_write = (wa_settings.enable_conversation_state && !all_state_patches.is_empty())
        || reopen_pending_next.is_some();

    if needs_state_write {
        let base = current_ai_conv_state.clone().unwrap_or_default();
        let mut new_state = if wa_settings.enable_conversation_state && !all_state_patches.is_empty() {
            apply_state_patches(base, &all_state_patches)
        } else {
            base
        };

        // Transfer-reset: si el último step es "transferred_to_*" limpiamos el
        // intent para que el próximo agente en la cadena (o en el próximo turno)
        // reclasifique. El step se mantiene para que el agente sepa de dónde
        // viene el cliente.
        if new_state
            .current_step
            .as_deref()
            .map(|s| s.starts_with("transferred_to_"))
            .unwrap_or(false)
        {
            new_state.current_intent = None;
            new_state.intent_confidence = None;
        }

        // Persistir el flag de reopen para que el próximo turno también
        // reciba history truncado aunque ai_conv_state ya no sea None.
        if let Some(pending) = reopen_pending_next {
            new_state.reopen_pending = pending;
            new_state.updated_at = chrono::Utc::now();
        }

        if Some(&new_state) != current_ai_conv_state.as_ref() {
            if let Err(e) = state
                .db
                .update_conversation_ai_conv_state(&inbound.conversation_id, Some(&new_state))
                .await
            {
                tracing::warn!(
                    "[ai_agent.dispatch] persistir ai_conv_state falló (conv={}): {}",
                    conv_hex,
                    e
                );
            } else {
                tracing::debug!(
                    "[ai_agent.dispatch] ai_conv_state actualizado (conv={})",
                    conv_hex
                );
                // Broadcast WS para que el front actualice el panel de estado IA.
                let state_json = serde_json::to_value(&new_state).ok();
                let ev = crate::modules::whatsapp::ws::WsServerEvent::ConversacionEstadoIa {
                    conversation_id: inbound.conversation_id.to_hex(),
                    ai_conv_state: state_json,
                };
                crate::modules::whatsapp::ws::broadcast_all(&state.ws_registry, &ev).await;
            }
        }
    }

    // ── Marcar inbounds como procesados por IA ────────────────────────────
    let burst_msg_ids: Vec<ObjectId> = burst.iter().filter_map(|m| m.id).collect();
    let now_for_ai_processed = BsonDateTime::now();
    if let Err(e) = state
        .db
        .mark_messages_ai_processed(
            &inbound.conversation_id,
            &burst_msg_ids,
            now_for_ai_processed,
        )
        .await
    {
        tracing::warn!("[ai_agent.dispatch] mark_messages_ai_processed: {}", e);
    } else {
        let iso = now_for_ai_processed
            .try_to_rfc3339_string()
            .unwrap_or_default();
        let ev = crate::modules::whatsapp::ws::WsServerEvent::IaProcesoMensaje {
            conversation_id: inbound.conversation_id.to_hex(),
            message_ids: burst_msg_ids.iter().map(|o| o.to_hex()).collect(),
            ai_processed_at: iso,
        };
        crate::modules::whatsapp::ws::broadcast_all(&state.ws_registry, &ev).await;
    }

    // ── Per-conv counter (1 por dispatch, no por iteración del chain) ─────
    // Nota: el reset de counters al transferir lo hace el tool
    // `transfer_to_agent` directamente cuando persiste el handoff. Acá solo
    // incrementamos el turn counter del último agente.
    state.redis.incr_ai_turns_conv(&conv_hex).await;
    let had_chain_transfer = chain_count > 0;

    // ── max_identification_attempts (sobre el último turno) ────────────────
    if last_agent.escalation.max_identification_attempts > 0 {
        let had_failed_lookup = last_output.tool_calls.iter().any(|t| {
            t.tool_name == "lookup_customer"
                && t.success
                && t.result_summary.contains("\"items\":[]")
        });
        if had_failed_lookup {
            let attempts = state.redis.incr_ai_id_attempts(&conv_hex).await;
            if attempts >= last_agent.escalation.max_identification_attempts as i64 {
                tracing::info!(
                    "[ai_agent.dispatch] max_identification_attempts ({}) reached (conv={})",
                    attempts,
                    conv_hex
                );
                escalation::auto_escalate(
                    &state,
                    &inbound.conversation_id,
                    &last_agent,
                    escalation::REASON_MAX_ID_ATTEMPTS,
                    Some("No fue posible identificar al cliente automáticamente"),
                    true,
                )
                .await;
                return Ok(());
            }
        }
    }

    // ── max_turns_without_resolution ───────────────────────────────────────
    // Lógica en 5 ramas explícitas (mutuamente exclusivas, fast-return dentro
    // del bloque `if cap > 0`):
    //
    //   B1  qualification_window  → prior_ai_turns < window → debug + skip
    //   B2  Action tool success   → reset_ai_no_resolution + debug
    //   B3  chain transfer        → had_chain_transfer || cross_workspace → debug + skip
    //   B4  InfoLookup success    → any tool succeeded (= InfoLookup en este punto) → debug + skip
    //   B5  no useful tool        → incr + info log + posible auto_escalate
    //
    // Orden importa: B1 antes que B2 (evitar reset innecesario en ventana),
    // B2 antes que B3 (log más específico gana si hay Action + chain_transfer),
    // B3 antes que B4 (tool fallido + chain transfer → skip, no incr),
    // B4 antes que B5 (InfoLookup exitoso → skip, no incr).
    let cap = last_agent.escalation.max_turns_without_resolution as i64;
    if cap > 0 {
        // B1: qualification window — skips counter entirely for initial turns
        let window = last_agent.escalation.qualification_window_turns as u64;
        if prior_ai_turns < window {
            tracing::debug!(
                "[ai_agent.dispatch] no_resolution skipped (conv={}, reason=qualification_window, prior_ai_turns={}/{})",
                conv_hex, prior_ai_turns, window
            );
        } else {
            // B2: Action tool con success → reset counter
            let action_success = last_output.tool_calls.iter().find(|t| {
                t.success
                    && crate::modules::ai_agent::tools::tool_category(&t.tool_name)
                        == crate::modules::ai_agent::tools::ToolCategory::Action
            });
            if let Some(t) = action_success {
                state.redis.reset_ai_no_resolution(&conv_hex).await;
                tracing::debug!(
                    "[ai_agent.dispatch] no_resolution reset (conv={}, tool={}, category=Action, count=0/{})",
                    conv_hex, t.tool_name, cap
                );
            } else if had_chain_transfer || cross_workspace_message.is_some() {
                // B3: transfer en chain (cross-workspace o same-workspace via chain)
                tracing::debug!(
                    "[ai_agent.dispatch] no_resolution skipped (conv={}, reason=chain_transfer)",
                    conv_hex
                );
            } else {
                // B4: InfoLookup con success → skip silencioso (sin reset)
                let any_success = last_output.tool_calls.iter().find(|t| t.success);
                if let Some(t) = any_success {
                    // No logueamos `count=N/MAX` aquí porque requeriría un Redis GET extra
                    // sólo para diagnóstico (el counter NO se modifica en este path).
                    tracing::debug!(
                        "[ai_agent.dispatch] no_resolution skipped (conv={}, tool={}, category=InfoLookup, max={})",
                        conv_hex, t.tool_name, cap
                    );
                } else {
                    // B5: nada útil pasó → incr + posible escalate (path original)
                    let nr = state.redis.incr_ai_no_resolution(&conv_hex).await;
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
                            &last_agent,
                            escalation::REASON_NO_RESOLUTION,
                            Some("Caso sin resolver tras varios turnos"),
                            true,
                        )
                        .await;
                        return Ok(());
                    }
                }
            }
        }
    }

    // ── escalate_on_critical_tool_failure (sobre el último turno) ──────────
    if last_agent.escalation.escalate_on_critical_tool_failure {
        let critical_failed = last_output.tool_calls.iter().any(|t| {
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
                &last_agent,
                escalation::REASON_CRITICAL_TOOL_FAILURE,
                Some("Falla crítica de herramienta — IA no pudo continuar"),
                true,
            )
            .await;
            return Ok(());
        }
    }

    // Limpieza del transfer_context. Dos caminos lo dejan poblado:
    //   (a) la conv venía con transfer_context al inicio de este dispatch
    //       (initial_transfer_context_owned). Lo consumió el primer agente.
    //   (b) durante el chain, el tool transfer_to_agent persistió uno nuevo
    //       (chain_count > 0). El target del chain ya lo consumió en su turno
    //       (active_transfer_context se le pasa fresh).
    // En ambos casos hay que limpiarlo en DB, sino el siguiente dispatch lo
    // re-inyecta a un agente que ya no tiene contexto compartido con el
    // mensaje nuevo del cliente, y la IA contesta sobre el tema viejo.
    if initial_transfer_context_owned.is_some() || had_chain_transfer {
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
        } else {
            tracing::debug!(
                "[ai_agent.dispatch] transfer_context limpiado post-dispatch (conv={})",
                conv_hex
            );
        }
    }

    // ── Decidir response_text ──────────────────────────────────────────────
    // Cross-workspace: usamos el `client_message` del tool ("escribí al
    // +58 YYY"). Caso normal: la response del último agente del chain
    // (si fue chain de mismo workspace, esa es la respuesta de Carla/Gabriel;
    // si no hubo chain, es la respuesta del agente original).
    let response_text_owned: Option<String> = if let Some(msg) = cross_workspace_message {
        if msg.trim().is_empty() {
            None
        } else {
            Some(msg)
        }
    } else {
        last_output.response_text.clone()
    };

    let response_text = match response_text_owned.as_deref() {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => {
            tracing::info!("[ai_agent.dispatch] runner no produjo texto, no envío");
            return Ok(());
        }
    };

    let last_is_live = matches!(last_agent.mode, AiAgentMode::Live);
    if !last_is_live {
        tracing::info!(
            "[ai_agent.dispatch] shadow → habría respondido (agent={}): {}",
            last_agent_id.to_hex(),
            truncate(&response_text, 300)
        );
        return Ok(());
    }

    if let Err(e) = send_live_response(
        &state,
        &wa_settings,
        &conv.phone,
        &last_agent.ai_user_id,
        inbound.conversation_id,
        &response_text,
    )
    .await
    {
        tracing::error!(
            "[ai_agent.dispatch] envío live falló (agent={}, conv={}): {}",
            last_agent_id.to_hex(),
            inbound.conversation_id.to_hex(),
            e
        );
    }

    // ── Follow-up check: mensajes que llegaron DURANTE este dispatch ───────
    // Si un cliente envió otro mensaje mientras corría el LLM, su scheduled
    // dispatch fue saltado por debounce/lock. Comparamos contra el
    // high_water_mark (mayor _id de inbound que la IA realmente "vio" en su
    // prompt — del burst inicial o del chain reload) y, si hay algo nuevo,
    // disparamos otro dispatch para no dejar mensajes huérfanos.
    let pending = state
        .db
        .list_recent_messages_for_conversation(&inbound.conversation_id, 5)
        .await
        .unwrap_or_default();
    if let Some(latest_pending) = pending
        .into_iter()
        .filter(|m| m.direction == "in")
        .filter(|m| m.id.map(|i| i > high_water_mark).unwrap_or(false))
        .last()
    {
        tracing::info!(
            "[ai_agent.dispatch] mensajes pendientes detectados post-dispatch (conv={}, latest_pending_id={}, hwm={}); spawn follow-up",
            inbound.conversation_id.to_hex(),
            latest_pending.id.map(|i| i.to_hex()).unwrap_or_default(),
            high_water_mark.to_hex()
        );
        dispatch_inbound_async(state.clone(), latest_pending, workspace_id);
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
        state.config.relay_url.as_ref(),
        state.config.relay_secret.as_ref(),
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
                media_id,
                e
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
        state.config.relay_url.as_ref(),
        state.config.relay_secret.as_ref(),
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
        ai_processed_at: None,
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
    buf.push_str(&format!(
        "[customer_lookup_by_phone]\nphone: {}\n",
        customer_phone
    ));
    if matches.is_empty() {
        buf.push_str("matches: 0\n");
        return (Some(buf), None);
    }
    buf.push_str(&format!("matches: {}\n", matches.len()));
    for (i, m) in matches.iter().enumerate() {
        buf.push_str(&format!(
            "  - [{}] client_id: {} | name: {} | identification: {} | status: {} | has_pending_debt: {} | address: {}\n",
            i + 1,
            m.client_id,
            m.name.as_deref().unwrap_or(""),
            m.identification.as_deref().unwrap_or(""),
            m.status,
            m.has_pending_debt,
            m.address.as_deref().unwrap_or(""),
        ));
    }
    let first_name = matches
        .first()
        .and_then(|m| m.name.as_ref())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    (Some(buf), first_name)
}

/// Extrae un resumen de una línea del output de `build_customer_context` para
/// el prompt del pre-clasificador. Mantiene el prompt pequeño y predictable.
/// Retorna `"sin match en DB"` cuando no hay matcheo o el contexto es `None`.
fn build_customer_summary_short(customer_context: &Option<String>) -> String {
    match customer_context {
        None => "sin match en DB".into(),
        Some(ctx) if ctx.contains("matches: 0") || !ctx.contains("matches:") => {
            "sin match en DB".into()
        }
        Some(ctx) => ctx
            .lines()
            .find(|l| l.trim_start().starts_with("- [1]"))
            .map(|l| l.trim().to_string())
            .unwrap_or_else(|| "sin match en DB".into()),
    }
}

/// Selecciona la plantilla de respuesta trivial que mejor matchea el texto del
/// cliente para el `kind` indicado (e.g. `"spam"`, `"greeting"`).
///
/// Reglas de selección:
/// 1. Filtra por `enabled == true` y `kind == requested_kind`.
/// 2. Filtra por triggers: si `triggers.is_empty()` (catch-all) o cualquier
///    trigger (normalizado) es substring del texto normalizado.
/// 3. Ordena por `priority` descendente (sort estable → empates preservan orden).
/// 4. Retorna la primera.
fn pick_trivial<'a>(
    responses: &'a [crate::models::whatsapp::TrivialResponse],
    kind: &str,
    text_normalized: &str,
) -> Option<&'a crate::models::whatsapp::TrivialResponse> {
    let mut candidates: Vec<&crate::models::whatsapp::TrivialResponse> = responses
        .iter()
        .filter(|t| t.enabled && t.kind == kind)
        .filter(|t| {
            t.triggers.is_empty()
                || t.triggers.iter().any(|tr| {
                    let tr_norm = super::tools::normalize_zone(tr);
                    !tr_norm.is_empty() && text_normalized.contains(&tr_norm)
                })
        })
        .collect();
    // Sort estable desc por priority (sort_by es estable en Rust).
    candidates.sort_by(|a, b| b.priority.cmp(&a.priority));
    candidates.first().copied()
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::models::whatsapp::TrivialResponse;

    fn make_trivial(
        kind: &str,
        triggers: &[&str],
        response: &str,
        priority: i32,
        enabled: bool,
    ) -> TrivialResponse {
        TrivialResponse {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.to_string(),
            triggers: triggers.iter().map(|s| s.to_string()).collect(),
            response: response.to_string(),
            enabled,
            priority,
        }
    }

    #[test]
    fn pick_trivial_empty_list() {
        let result = pick_trivial(&[], "spam", "hola");
        assert!(result.is_none());
    }

    #[test]
    fn pick_trivial_no_kind_match() {
        let items = vec![make_trivial("greeting", &["hola"], "ok", 0, true)];
        assert!(pick_trivial(&items, "spam", "hola").is_none());
    }

    #[test]
    fn pick_trivial_multi_match_priority() {
        let items = vec![
            make_trivial("spam", &["promo"], "resp_low", 0, true),
            make_trivial("spam", &["promo"], "resp_high", 5, true),
        ];
        let result = pick_trivial(&items, "spam", "promo oferta");
        assert_eq!(result.unwrap().response, "resp_high");
    }

    #[test]
    fn pick_trivial_empty_triggers_catchall() {
        let items = vec![make_trivial("greeting", &[], "hola a vos", 0, true)];
        let result = pick_trivial(&items, "greeting", "buenas tardes");
        assert!(result.is_some());
        assert_eq!(result.unwrap().response, "hola a vos");
    }

    #[test]
    fn pick_trivial_disabled_skipped() {
        let items = vec![
            make_trivial("spam", &["promo"], "resp_disabled", 10, false),
            make_trivial("spam", &["promo"], "resp_enabled", 0, true),
        ];
        let result = pick_trivial(&items, "spam", "promo");
        assert_eq!(result.unwrap().response, "resp_enabled");
    }

    #[test]
    fn pick_trivial_accent_normalization() {
        let items = vec![make_trivial("spam", &["promoción"], "resp", 0, true)];
        let normalized = super::super::tools::normalize_zone("PROMOCION gratis");
        let result = pick_trivial(&items, "spam", &normalized);
        assert!(result.is_some());
    }
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
    skip_active_agent: bool,
) -> Option<AiAgent> {
    if !skip_active_agent {
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
    } else {
        tracing::debug!(
            "[ai_agent.dispatch] reopen Turn 2 — ignorando ai_active_agent_id, forzando receptionist"
        );
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

/// Persiste un `AiInteraction` mínimo (sin tokens LLM) cuando el
/// pre-clasificador cortocircuitó el turno (Spam / GreetingOnly con plantilla).
/// Registra `pre_classified=true` y el variant crudo para auditoría.
/// Los tokens del pre-clasificador son insignificantes (< 200 tokens) y no se
/// rastrean individualmente por turno — solo se registra que el gate actuó.
async fn persist_pre_class_only_interaction(
    state: &Arc<AppState>,
    conversation_id: mongodb::bson::oid::ObjectId,
    message_id: mongodb::bson::oid::ObjectId,
    workspace_id: mongodb::bson::oid::ObjectId,
    agent_id: mongodb::bson::oid::ObjectId,
    model_id: &str,
    pre_class: &PreClassResultFull,
) {
    let now = BsonDateTime::now();
    let interaction = AiInteraction {
        id: None,
        conversation_id,
        message_id,
        workspace_id,
        agent_id,
        turn_index: 0,
        model_id: model_id.to_string(),
        input_tokens: pre_class.tokens.input,
        output_tokens: pre_class.tokens.output,
        cost_usd_estimate: 0.0, // flash-lite en gate: costo < $0.00001, no rastrear
        latency_ms: pre_class.latency_ms,
        tool_calls: Vec::new(),
        response_text: None,
        escalated: false,
        escalation_reason: None,
        thinking_tokens: 0,
        cached_tokens: 0,
        pre_classified: true,
        pre_class_result: Some(pre_class.variant.as_str().to_string()),
        created_at: now,
    };
    if let Err(e) = state.db.create_ai_interaction(interaction).await {
        tracing::warn!(
            "[ai_agent.dispatch] persist_pre_class_only_interaction falló: {}",
            e
        );
    }
}

// ── Dispatch outcome: pure helper for testability ──────────────────────────

/// Resultado de la evaluación de `max_turns_without_resolution` para un turno.
/// Mutuamente exclusivo: la evaluación de 5 ramas devuelve exactamente uno.
/// Extraído como función pura para testabilidad — la lógica real corre inline en
/// `run_dispatch` por acceso a `state.redis` (efectos con I/O).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// B0: max_turns_without_resolution == 0, feature disabled.
    Disabled,
    /// B1: prior_ai_turns < qualification_window_turns → skip counter.
    WindowSkip,
    /// B2: Action tool succeeded → reset counter.
    ActionReset { tool_name: String },
    /// B3: chain transfer (had_chain_transfer || cross_workspace) → skip.
    ChainSkip,
    /// B4: InfoLookup tool succeeded → skip counter (no reset).
    InfoLookupSkip { tool_name: String },
    /// B5: no useful tool → increment counter (may escalate if >= cap).
    Increment,
}

/// Determina el `DispatchOutcome` para el bloque `max_turns_without_resolution`,
/// a partir de inputs puramente de datos (sin I/O, sin Redis, sin DB).
///
/// **IMPORTANT**: This helper is mirrored inline in `run_dispatch` above.
/// If you change the branching logic here, update both places.
/// The helper exists for unit testing without Redis dependencies.
#[allow(dead_code)]
pub fn categorize_dispatch_outcome(
    prior_ai_turns: u64,
    max_turns_without_resolution: u32,
    qualification_window_turns: u32,
    tool_calls: &[crate::models::ai_agent::AiToolCallLog],
    had_chain_transfer: bool,
    has_cross_workspace: bool,
) -> DispatchOutcome {
    use super::tools::{tool_category, ToolCategory};

    // B0: feature disabled
    if max_turns_without_resolution == 0 {
        return DispatchOutcome::Disabled;
    }

    // B1: qualification window
    if prior_ai_turns < qualification_window_turns as u64 {
        return DispatchOutcome::WindowSkip;
    }

    // B2: Action tool success
    if let Some(t) = tool_calls
        .iter()
        .find(|t| t.success && tool_category(&t.tool_name) == ToolCategory::Action)
    {
        return DispatchOutcome::ActionReset {
            tool_name: t.tool_name.clone(),
        };
    }

    // B3: chain transfer
    if had_chain_transfer || has_cross_workspace {
        return DispatchOutcome::ChainSkip;
    }

    // B4: InfoLookup success (any successful tool at this point is InfoLookup)
    if let Some(t) = tool_calls.iter().find(|t| t.success) {
        return DispatchOutcome::InfoLookupSkip {
            tool_name: t.tool_name.clone(),
        };
    }

    // B5: no useful tool
    DispatchOutcome::Increment
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ai_agent::AiToolCallLog;
    use serde_json::json;

    fn make_tool_call(tool_name: &str, success: bool) -> AiToolCallLog {
        AiToolCallLog {
            tool_name: tool_name.to_string(),
            args: json!({}),
            result_summary: String::new(),
            success,
            error: if success {
                None
            } else {
                Some("err".to_string())
            },
            duration_ms: 0,
        }
    }

    // ── Scenario A: Carla qualification window (the original bug) ─────────
    // cap=4, window=4, text-only turns 1-4: counter stays 0; turn 5 increments.
    #[test]
    fn scenario_a_carla_qualification_window() {
        // Turns 1-4: prior_ai_turns = 0, 1, 2, 3 → all WindowSkip (< 4)
        for prior in 0u64..4 {
            let outcome = categorize_dispatch_outcome(
                prior,
                4,   // max_turns_without_resolution
                4,   // qualification_window_turns
                &[], // no tool calls
                false,
                false,
            );
            assert_eq!(
                outcome,
                DispatchOutcome::WindowSkip,
                "turn {} (prior_ai_turns={}) should be WindowSkip",
                prior + 1,
                prior
            );
        }

        // Turn 5: prior_ai_turns = 4 (>= window=4) → normal evaluation, no tools → Increment
        let outcome = categorize_dispatch_outcome(
            4,   // prior_ai_turns == window, window no longer applies
            4,   // max_turns_without_resolution
            4,   // qualification_window_turns
            &[], // no tool calls
            false,
            false,
        );
        assert_eq!(
            outcome,
            DispatchOutcome::Increment,
            "turn 5 (prior_ai_turns=4) should be Increment (window no longer applies)"
        );
    }

    // ── Scenario D: Sanity — no window, no tools, escalation path ────────
    // cap=3, window=0, 3 text-only turns → Increment each time.
    #[test]
    fn scenario_d_sanity_no_window_increment_path() {
        for prior in 0u64..3 {
            let outcome = categorize_dispatch_outcome(
                prior,
                3, // max_turns_without_resolution
                0, // qualification_window_turns = 0 (disabled)
                &[],
                false,
                false,
            );
            assert_eq!(
                outcome,
                DispatchOutcome::Increment,
                "turn {} should Increment (no window, no tools)",
                prior + 1
            );
        }
    }

    // ── Scenario C: InfoLookup does NOT reset ─────────────────────────────
    // cap=4, window=0, list_plans success → InfoLookupSkip (no reset).
    #[test]
    fn scenario_c_info_lookup_does_not_reset() {
        // Turns 1-2: no tools → Increment
        let outcome1 = categorize_dispatch_outcome(0, 4, 0, &[], false, false);
        assert_eq!(outcome1, DispatchOutcome::Increment);

        let outcome2 = categorize_dispatch_outcome(1, 4, 0, &[], false, false);
        assert_eq!(outcome2, DispatchOutcome::Increment);

        // Turn 3: list_plans success → InfoLookupSkip (counter stays at 2, not reset)
        let tools = vec![make_tool_call("list_plans", true)];
        let outcome3 = categorize_dispatch_outcome(2, 4, 0, &tools, false, false);
        assert_eq!(
            outcome3,
            DispatchOutcome::InfoLookupSkip {
                tool_name: "list_plans".to_string()
            },
            "list_plans success should be InfoLookupSkip, not ActionReset"
        );

        // Turns 4-5: no tools → Increment (counter reaches 3, then 4 → escalate)
        let outcome4 = categorize_dispatch_outcome(3, 4, 0, &[], false, false);
        assert_eq!(outcome4, DispatchOutcome::Increment);
        let outcome5 = categorize_dispatch_outcome(4, 4, 0, &[], false, false);
        assert_eq!(outcome5, DispatchOutcome::Increment);
    }

    // ── Scenario B: Action tool reset ────────────────────────────────────
    // cap=4, window=0, transfer_to_agent success → ActionReset.
    #[test]
    fn scenario_b_action_tool_reset() {
        // Turn 1: no tools → Increment (counter = 1)
        let outcome1 = categorize_dispatch_outcome(0, 4, 0, &[], false, false);
        assert_eq!(outcome1, DispatchOutcome::Increment);

        // Turn 2: no tools → Increment (counter = 2)
        let outcome2 = categorize_dispatch_outcome(1, 4, 0, &[], false, false);
        assert_eq!(outcome2, DispatchOutcome::Increment);

        // Turn 3: transfer_to_agent success → ActionReset (counter → 0)
        let tools = vec![make_tool_call("transfer_to_agent", true)];
        let outcome3 = categorize_dispatch_outcome(2, 4, 0, &tools, false, false);
        assert_eq!(
            outcome3,
            DispatchOutcome::ActionReset {
                tool_name: "transfer_to_agent".to_string()
            },
            "transfer_to_agent success should ActionReset"
        );

        // Turn 4: no tools → Increment (counter = 1 after reset)
        let outcome4 = categorize_dispatch_outcome(3, 4, 0, &[], false, false);
        assert_eq!(outcome4, DispatchOutcome::Increment);

        // Turn 5: no tools → Increment (counter = 2)
        let outcome5 = categorize_dispatch_outcome(4, 4, 0, &[], false, false);
        assert_eq!(outcome5, DispatchOutcome::Increment);
    }

    // ── Edge: unknown tool name → treated as InfoLookup (skip) ───────────
    #[test]
    fn edge_unknown_tool_name_defaults_to_info_lookup() {
        let tools = vec![make_tool_call("some_future_tool", true)];
        let outcome = categorize_dispatch_outcome(0, 4, 0, &tools, false, false);
        // Unknown tool → InfoLookup default → InfoLookupSkip
        assert_eq!(
            outcome,
            DispatchOutcome::InfoLookupSkip {
                tool_name: "some_future_tool".to_string()
            },
            "unknown tool with success should be InfoLookupSkip (safe default)"
        );
    }

    // ── Edge: cap=0 → feature disabled, no branch evaluates ──────────────
    #[test]
    fn edge_cap_zero_feature_disabled() {
        let outcome = categorize_dispatch_outcome(0, 0, 0, &[], false, false);
        assert_eq!(
            outcome,
            DispatchOutcome::Disabled,
            "max_turns_without_resolution=0 should disable the feature"
        );

        // Even with tools, still Disabled
        let tools = vec![make_tool_call("transfer_to_agent", true)];
        let outcome2 = categorize_dispatch_outcome(5, 0, 3, &tools, false, false);
        assert_eq!(outcome2, DispatchOutcome::Disabled);
    }

    // ── Spec 1.5: Action wins over InfoLookup in same turn ───────────────
    #[test]
    fn action_wins_over_info_lookup_same_turn() {
        let tools = vec![
            make_tool_call("list_plans", true),
            make_tool_call("create_ticket", true),
        ];
        let outcome = categorize_dispatch_outcome(0, 4, 0, &tools, false, false);
        assert!(
            matches!(outcome, DispatchOutcome::ActionReset { .. }),
            "Action tool should win over InfoLookup in same turn"
        );
    }

    // ── Spec 1.4: Failed InfoLookup → Increment ───────────────────────────
    #[test]
    fn failed_info_lookup_increments() {
        let tools = vec![make_tool_call("list_plans", false)]; // failed
        let outcome = categorize_dispatch_outcome(0, 4, 0, &tools, false, false);
        assert_eq!(
            outcome,
            DispatchOutcome::Increment,
            "failed InfoLookup tool should increment the counter"
        );
    }

    // ── Spec 2.2: Turn at window boundary uses normal evaluation ─────────
    #[test]
    fn window_boundary_uses_normal_evaluation() {
        // prior_ai_turns == window → B1 does NOT apply (strict <)
        let outcome = categorize_dispatch_outcome(
            4,   // prior_ai_turns == window
            4,   // max_turns_without_resolution
            4,   // qualification_window_turns
            &[], // no tools
            false,
            false,
        );
        assert_eq!(
            outcome,
            DispatchOutcome::Increment,
            "at window boundary (prior == window) normal evaluation should apply"
        );
    }
}

// ── Validator tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod handler_validator_tests {
    use crate::{
        error::ApiError,
        models::ai_agent::{AiEscalationRules, AiEscalationRulesInput},
    };

    fn base_escalation() -> AiEscalationRules {
        AiEscalationRules {
            keywords: vec![],
            max_turns_without_resolution: 3,
            qualification_window_turns: 0,
            max_identification_attempts: 2,
            escalate_on_critical_tool_failure: true,
            always_escalate_when_asked: true,
            default_ticket_category_id: None,
        }
    }

    fn apply_escalation_under_test(
        cur: &mut AiEscalationRules,
        patch: Option<AiEscalationRulesInput>,
    ) -> Result<(), ApiError> {
        let Some(p) = patch else {
            return Ok(());
        };
        if let Some(v) = p.keywords {
            cur.keywords = v;
        }
        if let Some(v) = p.max_turns_without_resolution {
            cur.max_turns_without_resolution = v;
        }
        if let Some(v) = p.qualification_window_turns {
            if v > 10 {
                return Err(ApiError::domain_simple(
                    axum::http::StatusCode::BAD_REQUEST,
                    "qualification_window_turns_out_of_range",
                    format!(
                        "qualification_window_turns must be between 0 and 10, got {}",
                        v
                    ),
                ));
            }
            cur.qualification_window_turns = v;
        }
        if let Some(v) = p.max_identification_attempts {
            cur.max_identification_attempts = v;
        }
        if let Some(v) = p.escalate_on_critical_tool_failure {
            cur.escalate_on_critical_tool_failure = v;
        }
        if let Some(v) = p.always_escalate_when_asked {
            cur.always_escalate_when_asked = v;
        }
        if p.default_ticket_category_id.is_some() {
            cur.default_ticket_category_id = p.default_ticket_category_id;
        }
        Ok(())
    }

    // Task 4.7: qualification_window_turns = 11 → Err with correct error code
    #[test]
    fn validator_rejects_window_above_10() {
        let mut esc = base_escalation();
        let patch = Some(AiEscalationRulesInput {
            qualification_window_turns: Some(11),
            ..Default::default()
        });
        let result = apply_escalation_under_test(&mut esc, patch);
        assert!(result.is_err(), "value 11 should be rejected");
        if let Err(ApiError::Domain { code, .. }) = result {
            assert_eq!(
                code, "qualification_window_turns_out_of_range",
                "error code must be qualification_window_turns_out_of_range"
            );
        } else {
            panic!("expected ApiError::Domain variant");
        }
        // Value must NOT be applied on error
        assert_eq!(
            esc.qualification_window_turns, 0,
            "value must not be stored on error"
        );
    }

    // Task 4.8: qualification_window_turns = 10 → Ok, value stored
    #[test]
    fn validator_accepts_window_at_upper_boundary() {
        let mut esc = base_escalation();
        let patch = Some(AiEscalationRulesInput {
            qualification_window_turns: Some(10),
            ..Default::default()
        });
        let result = apply_escalation_under_test(&mut esc, patch);
        assert!(
            result.is_ok(),
            "value 10 should be accepted (upper boundary inclusive)"
        );
        assert_eq!(
            esc.qualification_window_turns, 10,
            "value 10 must be stored"
        );
    }

    // Bonus: value 0 (lower boundary) → Ok
    #[test]
    fn validator_accepts_window_at_lower_boundary() {
        let mut esc = base_escalation();
        esc.qualification_window_turns = 5; // start at 5
        let patch = Some(AiEscalationRulesInput {
            qualification_window_turns: Some(0),
            ..Default::default()
        });
        let result = apply_escalation_under_test(&mut esc, patch);
        assert!(
            result.is_ok(),
            "value 0 should be accepted (lower boundary)"
        );
        assert_eq!(esc.qualification_window_turns, 0);
    }
}
