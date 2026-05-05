//! Loop runner del AI Agent.
//!
//! Un "turno" = un mensaje del cliente (`user`) → uno o más roundtrips a Gemini
//! con tool calls intermedios → respuesta final en texto.
//!
//! ```text
//!   user_msg ─┐
//!             ▼
//!         contents.push(user)
//!         loop max_iterations:
//!             resp = gemini(system + contents + tools)
//!             if resp tiene functionCall:
//!                 contents.push(model functionCall)
//!                 result = execute_tool(...)
//!                 contents.push(user functionResponse)
//!                 continue
//!             else:
//!                 break con response.text
//! ```
//!
//! El runner es agnóstico de "es sandbox o real". `ToolContext.is_sandbox` lo
//! consumen los tools (decide si persisten o no). El handler que llama
//! `run_turn` decide si persiste `AiInteraction`.

use std::time::Instant;

use crate::{
    error::ApiError,
    models::{
        ai_agent::{AiAgent, AiInteraction, AiToolCallLog},
        whatsapp::StatePatch,
    },
};

/// Valores que el back inyecta en `system_prompt` reemplazando los
/// placeholders `{snake_case}` que el SUPERADMIN escribe desde el front.
/// Valores vacíos generan substring `""` (no rompen el prompt — quedan visibles
/// como espacios en blanco si el SUPERADMIN testea en sandbox).
#[derive(Debug, Default, Clone)]
pub struct PromptVariables {
    pub assistant_name: String,
    pub workspace_name: String,
    pub customer_name: String,
    pub customer_phone: String,
    pub business_phone: String,
    /// Fecha en formato `YYYY-MM-DD` en TZ Caracas.
    pub today: String,
    /// Día de la semana en español (`lunes`, `martes`, …).
    pub weekday: String,
}

fn substitute_prompt(text: &str, vars: &PromptVariables) -> String {
    text.replace("{assistant_name}", &vars.assistant_name)
        .replace("{workspace_name}", &vars.workspace_name)
        .replace("{customer_name}", &vars.customer_name)
        .replace("{customer_phone}", &vars.customer_phone)
        .replace("{business_phone}", &vars.business_phone)
        .replace("{today}", &vars.today)
        .replace("{weekday}", &vars.weekday)
}

use super::{
    openrouter::{
        AiRelay, ChatCompletionRequest, ChatMessage, ContentBlock, MessageContent,
        OpenRouterClient, ToolChoice,
    },
    tools::{build_function_declarations, execute_tool, ToolContext},
};

/// Adjunto multimedia que viene del dispatch junto al texto del usuario.
/// El runner lo convierte a `ContentBlock` según el MIME type.
#[derive(Debug, Clone)]
pub struct MediaInput {
    pub mime_type: String,
    /// Base64 estándar (NO url-safe). El caller hace `STANDARD.encode(bytes)`.
    pub data_base64: String,
}

impl MediaInput {
    /// Convierte este adjunto a un `ContentBlock` de OpenAI.
    ///
    /// - `image/*` → `ContentBlock::ImageUrl` con data URI base64
    /// - `audio/wav`, `audio/mp3`, `audio/ogg` → `ContentBlock::InputAudio`
    /// - otros (incluido audio desconocido) → `ContentBlock::Text` con placeholder
    pub fn to_content_block(&self) -> ContentBlock {
        let mime = self.mime_type.as_str();
        if mime.starts_with("image/") {
            ContentBlock::ImageUrl {
                image_url: super::openrouter::ImageUrlInner {
                    url: format!("data:{};base64,{}", mime, self.data_base64),
                },
            }
        } else if matches!(mime, "audio/wav" | "audio/mp3" | "audio/mpeg" | "audio/ogg") {
            let format = if mime.contains("wav") {
                "wav"
            } else if mime.contains("ogg") {
                "ogg"
            } else {
                "mp3"
            };
            ContentBlock::InputAudio {
                input_audio: super::openrouter::InputAudioInner {
                    data: self.data_base64.clone(),
                    format: format.to_string(),
                },
            }
        } else {
            // PDF, Office docs, etc. → placeholder texto para que el modelo
            // sepa que hay un adjunto sin poder procesarlo directamente.
            tracing::warn!(
                "[ai_agent.runner] MIME type '{}' no soportado como ContentBlock — usando placeholder",
                mime
            );
            ContentBlock::Text {
                text: format!("[attachment type={}]", mime),
            }
        }
    }
}

/// Cap defensivo para evitar loops infinitos. Si la IA gira pidiendo tools sin
/// converger, escalamos por `max_iterations_reached`.
const MAX_ITERATIONS: u32 = 5;

/// Una entrada del historial de conversación que llega al runner. El handler
/// del sandbox lo construye desde el body del POST; en producción (PR 3) lo
/// arma desde `WaMessages`.
#[derive(Debug, Clone)]
pub struct ConvTurn {
    /// `"user"` (cliente) o `"assistant"` (IA o humano outbound).
    pub role: ConvRole,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvRole {
    User,
    Assistant,
}

/// Info estructurada del último `transfer_to_agent` exitoso del turno. El
/// dispatch la lee para decidir si re-correr el ciclo con el agente destino
/// (mismo workspace) o enviar `client_message` al cliente (otro workspace).
#[derive(Debug, Clone)]
pub struct TransferInfo {
    pub target_agent_id: mongodb::bson::oid::ObjectId,
    /// `true` cuando el target NO atiende el workspace de la conv actual y
    /// hay que avisarle al cliente que escriba a otro número.
    pub cross_workspace: bool,
    /// Texto sugerido para enviar al cliente. Solo presente en cross-workspace.
    pub client_message: Option<String>,
    pub reason: String,
}

/// Salida del runner. El caller decide si persiste como `AiInteraction`.
#[derive(Debug, Clone)]
pub struct RunnerOutput {
    pub response_text: Option<String>,
    pub tool_calls: Vec<AiToolCallLog>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens gastados en reasoning interno por modelos thinking. Separado de
    /// `output_tokens` (texto visible) para diagnosticar truncamiento.
    pub thinking_tokens: u32,
    /// Phase 3a — tokens servidos desde caché implícito/explícito de Gemini.
    pub cached_tokens: u32,
    pub total_tokens: u32,
    pub cost_usd_estimate: f64,
    pub latency_ms: u32,
    /// `true` cuando el LLM pidió `request_human` o `create_ticket`.
    pub escalated: bool,
    #[allow(dead_code)]
    pub escalation_reason: Option<String>,
    /// Eco del último `finishReason` de Gemini (`STOP`, `MAX_TOKENS`,
    /// `SAFETY`, `OTHER`, ...). Útil para diagnosticar respuestas truncadas.
    pub finish_reason: Option<String>,
    /// Set cuando el turno terminó con un `transfer_to_agent` exitoso. El
    /// dispatch lo usa para decidir si re-correr en memoria con el target o
    /// enviar el mensaje cross-workspace al cliente.
    pub transfer: Option<TransferInfo>,
    /// Patches de estado acumulados por todas las tools ejecutadas en este
    /// turno, en orden de ejecución. El dispatch los pliega en
    /// `WaConversation.ai_conv_state` después del chain loop.
    pub state_patches: Vec<StatePatch>,
}

impl RunnerOutput {
    /// Construye un `AiInteraction` listo para persistir (PR 3 lo va a usar
    /// desde el dispatch real). Sandbox descarta esto.
    ///
    /// `pre_class` — cuando el pre-clasificador corrió antes del LLM, pasar
    /// `Some(&result)` para registrar `pre_classified=true` y el raw variant.
    /// Pasar `None` si el turno no pasó por el gate (comportamiento anterior).
    #[allow(dead_code)]
    pub fn to_interaction(
        &self,
        conversation_id: mongodb::bson::oid::ObjectId,
        message_id: mongodb::bson::oid::ObjectId,
        workspace_id: mongodb::bson::oid::ObjectId,
        agent_id: mongodb::bson::oid::ObjectId,
        turn_index: u32,
        model_id: &str,
        pre_class: Option<&super::pre_classifier::PreClassResultFull>,
    ) -> AiInteraction {
        let now = mongodb::bson::DateTime::now();

        // Cuando el pre-clasificador corrió y el turno cayó al LLM (fall-through),
        // sumamos sus tokens y costo al registro del turno completo para que las
        // métricas sean exactas (un row = un turno inbound, incluyendo todo el
        // gasto del gate).
        let pc_in    = pre_class.map_or(0u32, |p| p.tokens.input);
        let pc_out   = pre_class.map_or(0u32, |p| p.tokens.output);
        let pc_cost  = pre_class.map_or(0.0_f64, |p| {
            // Pre-classifier usando openai/gpt-4o-mini — costo trivial, no rastrear.
            crate::models::ai_agent::estimate_cost_usd(
                "openai/gpt-4o-mini",
                p.tokens.input,
                0,
                p.tokens.output,
                0,
            )
        });
        let pc_lat   = pre_class.map_or(0u32, |p| p.latency_ms);

        AiInteraction {
            id: None,
            conversation_id,
            message_id,
            workspace_id,
            agent_id,
            turn_index,
            model_id: model_id.to_string(),
            input_tokens:  self.input_tokens.saturating_add(pc_in),
            output_tokens: self.output_tokens.saturating_add(pc_out),
            cost_usd_estimate: self.cost_usd_estimate + pc_cost,
            latency_ms: self.latency_ms.saturating_add(pc_lat),
            tool_calls: self.tool_calls.clone(),
            response_text: self.response_text.clone(),
            escalated: self.escalated,
            escalation_reason: self.escalation_reason.clone(),
            thinking_tokens: self.thinking_tokens,
            cached_tokens: self.cached_tokens,
            pre_classified: pre_class.is_some(),
            pre_class_result: pre_class.map(|p| p.variant.as_str().to_string()),
            created_at: now,
        }
    }
}

// ============================================
// Build prompt + payload
// ============================================

fn build_system_instruction(
    agent: &AiAgent,
    faqs_inline: Option<&str>,
    customer_context: Option<&str>,
    transfer_context: Option<&str>,
    first_turn_note: Option<&str>,
    agent_state: Option<&str>,
    turn_state: Option<&str>,
    conversation_state: Option<&str>,   // NEW — Phase 2
    vars: Option<&PromptVariables>,
) -> String {
    // El back solo pasa DATOS etiquetados — el SUPERADMIN decide el
    // comportamiento desde `system_prompt` en el front. No metemos
    // instrucciones imperativas ("NO pidas cédula", "úsalo cuando…").
    let mut chunks: Vec<String> = Vec::new();

    let prompt_owned;
    let prompt_str: &str = if !agent.system_prompt.trim().is_empty() {
        if let Some(v) = vars {
            prompt_owned = substitute_prompt(agent.system_prompt.trim(), v);
            &prompt_owned
        } else {
            agent.system_prompt.trim()
        }
    } else {
        ""
    };
    if !prompt_str.is_empty() {
        chunks.push(prompt_str.to_string());
    }

    // Datos de personalidad como etiquetas neutras. El system_prompt los
    // referencia como prefiera ("usá el saludo configurado", etc).
    let mut personality_lines = Vec::new();
    let p = &agent.personality;
    if !p.assistant_name.is_empty() {
        personality_lines.push(format!("assistant_name: {}", p.assistant_name));
    }
    if !p.tone.is_empty() {
        personality_lines.push(format!("tone: {}", p.tone));
    }
    if !p.locale.is_empty() {
        personality_lines.push(format!("locale: {}", p.locale));
    }
    if !p.greeting.trim().is_empty() {
        personality_lines.push(format!("greeting: {}", p.greeting.trim()));
    }
    if !p.farewell.trim().is_empty() {
        personality_lines.push(format!("farewell: {}", p.farewell.trim()));
    }
    if !p.farewell_to_human.trim().is_empty() {
        personality_lines.push(format!("farewell_to_human: {}", p.farewell_to_human.trim()));
    }
    if !p.forbidden_phrases.is_empty() {
        personality_lines.push(format!(
            "forbidden_phrases: {}",
            p.forbidden_phrases.join(", ")
        ));
    }
    if !personality_lines.is_empty() {
        chunks.push(format!("[personality]\n{}", personality_lines.join("\n")));
    }

    if let Some(ctx) = customer_context {
        if !ctx.trim().is_empty() {
            chunks.push(ctx.trim().to_string());
        }
    }

    if let Some(tc) = transfer_context {
        if !tc.trim().is_empty() {
            chunks.push(format!("[transfer_context]\n{}", tc.trim()));
        }
    }

    if let Some(note) = first_turn_note {
        if !note.trim().is_empty() {
            chunks.push(format!("[ai_first_turn]\n{}", note.trim()));
        }
    }

    if let Some(state) = agent_state {
        if !state.trim().is_empty() {
            chunks.push(format!("[agent_state]\n{}", state.trim()));
        }
    }

    if let Some(ts) = turn_state {
        if !ts.trim().is_empty() {
            chunks.push(format!("[turn_state]\n{}", ts.trim()));
        }
    }

    // NEW — Phase 2: bloque de estado IA persistido, entre [turn_state] y [faqs].
    if let Some(cs) = conversation_state {
        if !cs.trim().is_empty() {
            chunks.push(format!("[conversation_state]\n{}", cs.trim()));
        }
    }

    if let Some(faqs) = faqs_inline {
        if !faqs.trim().is_empty() {
            chunks.push(format!("[faqs]\n{}", faqs.trim()));
        }
    }

    chunks.join("\n\n")
}

fn convert_history(history: &[ConvTurn]) -> Vec<ChatMessage> {
    history
        .iter()
        .map(|t| ChatMessage {
            role: match t.role {
                ConvRole::User => "user".into(),
                ConvRole::Assistant => "assistant".into(),
            },
            content: Some(MessageContent::Text(t.text.clone())),
            ..Default::default()
        })
        .collect()
}

// ============================================
// Loop principal
// ============================================

/// Corre un turno completo. Devuelve la respuesta final en texto + métricas.
///
/// `api_key_decrypted`: la api_key descifrada por el caller (sandbox o
/// dispatch). El runner no descifra para evitar duplicación.
///
/// `faqs_inline`: bloque de FAQs ya formateado (`Q: ... A: ...\n...`). El
/// caller lo trae de `AiAgentRepository::list_ai_agent_faqs` si quiere.
pub async fn run_turn(
    http: &reqwest::Client,
    agent: &AiAgent,
    api_key_decrypted: &str,
    relay: Option<&AiRelay>,
    base_url: &str,
    history: &[ConvTurn],
    user_message: &str,
    user_media: &[MediaInput],
    faqs_inline: Option<&str>,
    customer_context: Option<&str>,
    transfer_context: Option<&str>,
    first_turn_note: Option<&str>,
    agent_state: Option<&str>,
    turn_state: Option<&str>,
    conversation_state: Option<&str>,   // NEW — Phase 2
    prompt_vars: Option<&PromptVariables>,
    tool_ctx: &ToolContext,
) -> Result<RunnerOutput, ApiError> {
    let started = Instant::now();
    let system_instruction = build_system_instruction(
        agent,
        faqs_inline,
        customer_context,
        transfer_context,
        first_turn_note,
        agent_state,
        turn_state,
        conversation_state,  // NEW — Phase 2
        prompt_vars,
    );

    // ── Diagnóstico ────────────────────────────────────────────────────────
    let enabled_tool_names: Vec<&str> = agent
        .tools
        .iter()
        .filter(|t| t.enabled)
        .map(|t| t.name.as_str())
        .collect();
    tracing::info!(
        "[ai_agent.runner] turno start (agent_id={}, model={}, system_chars={}, tools_enabled={}, history_turns={}, has_customer_ctx={}, has_transfer_ctx={}, has_first_turn_note={}, has_agent_state={})",
        agent.id.map(|o| o.to_hex()).unwrap_or_default(),
        agent.model.model_id,
        system_instruction.chars().count(),
        enabled_tool_names.len(),
        history.len(),
        customer_context.is_some(),
        transfer_context.is_some(),
        first_turn_note.is_some(),
        agent_state.is_some(),
    );
    tracing::debug!(
        "[ai_agent.runner] system_instruction (final, placeholders sustituidos):\n{}",
        system_instruction
    );
    tracing::debug!(
        "[ai_agent.runner] tools enviadas a openrouter: {:?}",
        enabled_tool_names
    );

    // Construir el historial base (system + history convt + nuevo turno user).
    let mut messages: Vec<ChatMessage> = Vec::new();

    // System message en messages[0] (OpenAI style).
    if !system_instruction.is_empty() {
        messages.push(ChatMessage {
            role: "system".into(),
            content: Some(MessageContent::Text(system_instruction)),
            ..Default::default()
        });
    }

    // Historial previo.
    messages.extend(convert_history(history));

    // Nuevo turno del usuario: texto + adjuntos.
    let mut user_blocks: Vec<ContentBlock> = Vec::new();
    if !user_message.trim().is_empty() {
        user_blocks.push(ContentBlock::Text { text: user_message.to_string() });
    }
    for m in user_media {
        user_blocks.push(m.to_content_block());
    }
    let user_content = match user_blocks.len() {
        0 => MessageContent::Text(String::new()), // fallback defensivo
        1 => {
            if let ContentBlock::Text { text } = &user_blocks[0] {
                MessageContent::Text(text.clone())
            } else {
                MessageContent::Blocks(user_blocks)
            }
        }
        _ => MessageContent::Blocks(user_blocks),
    };
    messages.push(ChatMessage {
        role: "user".into(),
        content: Some(user_content),
        ..Default::default()
    });

    // Tools del agente.
    let tool_list = build_function_declarations(agent, &tool_ctx.transfer_target_labels);
    let tools_option = if tool_list.is_empty() { None } else { Some(tool_list) };
    let tool_choice_option = tools_option.as_ref().map(|_| ToolChoice::Auto);

    // Construir el cliente OpenRouter.
    let or_client = OpenRouterClient::new(
        http.clone(),
        base_url.to_string(),
        api_key_decrypted.to_string(),
        relay.cloned(),
    );

    // Acumuladores de tokens y resultado.
    let mut total_in: i64 = 0;
    let mut total_out: i64 = 0;
    // OpenRouter / OpenAI non-reasoning models no exponen thinking tokens —
    // siempre 0. Se mantiene el campo para compat con AiInteraction schema.
    let total_thinking: i64 = 0;
    let mut total_cached: i64 = 0;
    let mut tool_call_logs: Vec<AiToolCallLog> = Vec::new();
    let mut response_text: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut escalated = false;
    let mut escalation_reason: Option<String> = None;
    // Phase 2: acumula patches de cada tool call en este turno.
    let mut state_patches_acc: Vec<StatePatch> = Vec::new();

    'turn: for iter in 0..MAX_ITERATIONS {
        let req = ChatCompletionRequest {
            model: agent.model.model_id.clone(),
            messages: messages.clone(),
            tools: tools_option.clone(),
            tool_choice: tool_choice_option.clone(),
            response_format: None,
            temperature: Some(agent.model.temperature),
            max_tokens: Some(agent.model.max_tokens),
            stream: None,
        };

        let resp = or_client.complete(&req).await?;

        let usage = resp.usage.unwrap_or_default();
        total_in += usage.prompt_tokens;
        total_out += usage.completion_tokens;
        total_cached += usage
            .prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .unwrap_or(0);

        let choice = match resp.choices.into_iter().next() {
            Some(c) => c,
            None => {
                tracing::warn!(
                    "[ai_agent.runner] no choices from openrouter (iter {})",
                    iter
                );
                response_text = Some(
                    "Disculpá, no pude generar respuesta. Te conecto con un compañero del equipo."
                        .to_string(),
                );
                escalated = true;
                escalation_reason = Some("no_choices".into());
                break 'turn;
            }
        };
        finish_reason = choice.finish_reason.clone();

        let assistant_msg = choice.message;
        let tool_calls = assistant_msg.tool_calls.clone().unwrap_or_default();

        if tool_calls.is_empty() {
            // Respuesta final en texto.
            let text = match assistant_msg.content {
                Some(MessageContent::Text(s)) => s,
                Some(MessageContent::Blocks(blocks)) => blocks
                    .into_iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text) } else { None })
                    .collect::<Vec<_>>()
                    .join(""),
                None => String::new(),
            };
            response_text = Some(text);
            break 'turn;
        }

        // Appendear el mensaje del assistant con sus tool_calls al historial.
        messages.push(ChatMessage {
            role: "assistant".into(),
            content: assistant_msg.content.clone(),
            tool_calls: Some(tool_calls.clone()),
            ..Default::default()
        });

        // Ejecutar cada tool call y agregar mensaje {role:"tool"} por call.
        for tc in &tool_calls {
            let is_escalation = tc.function.name == "request_human" || tc.function.name == "create_ticket";

            tracing::info!(
                "[ai_agent.runner] tool_call: id={} name={} args={}",
                tc.id,
                tc.function.name,
                &tc.function.arguments
            );

            // Parsear argumentos: JSON-encoded string → Value.
            let args_value: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "[ai_agent.runner] tool_call id={} args parse error: {} | raw='{}'",
                        tc.id, e, tc.function.arguments
                    );
                    serde_json::json!({ "error": "invalid_args", "details": e.to_string() })
                }
            };

            // Detectar si es args de error — no ejecutar la tool en ese caso.
            let result = if args_value.get("error").is_some() && args_value.get("details").is_some() {
                // Args inválidos: devolver error como resultado sin llamar la tool.
                super::tools::ToolResult {
                    success: false,
                    data: args_value.clone(),
                    error: Some("invalid_args".into()),
                    duration_ms: 0,
                    state_patches: Vec::new(),
                }
            } else {
                execute_tool(&tc.function.name, args_value.clone(), tool_ctx).await
            };

            if result.success {
                tracing::info!(
                    "[ai_agent.runner] tool_result: id={} name={} success=true duration_ms={} summary={}",
                    tc.id,
                    tc.function.name,
                    result.duration_ms,
                    truncate_summary(&result.data),
                );
                state_patches_acc.extend(result.state_patches.iter().cloned());
            } else {
                tracing::warn!(
                    "[ai_agent.runner] tool_result: id={} name={} success=false duration_ms={} error={:?}",
                    tc.id,
                    tc.function.name,
                    result.duration_ms,
                    result.error,
                );
                state_patches_acc.push(crate::models::whatsapp::StatePatch::AddFailedAttempt {
                    tool: tc.function.name.clone(),
                    error: result.error.clone().unwrap_or_else(|| "unknown_error".into()),
                });
            }

            tool_call_logs.push(AiToolCallLog {
                tool_name: tc.function.name.clone(),
                args: args_value,
                result_summary: truncate_summary(&result.data),
                success: result.success,
                error: result.error.clone(),
                duration_ms: result.duration_ms,
            });

            if is_escalation && result.success {
                escalated = true;
                escalation_reason = Some(format!("tool:{}", tc.function.name));
            }

            let payload = if result.success {
                result.data
            } else {
                serde_json::json!({ "error": result.error.clone().unwrap_or_default() })
            };

            let content_str = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());

            // {role:"tool"} con tool_call_id — CRÍTICO: debe coincidir con tc.id.
            messages.push(ChatMessage {
                role: "tool".into(),
                content: Some(MessageContent::Text(content_str)),
                tool_call_id: Some(tc.id.clone()),
                name: Some(tc.function.name.clone()),
                ..Default::default()
            });
        }
        // Si escaló por create_ticket/request_human, continúa un turno más
        // para que el LLM produzca la despedida en texto.
    }

    if response_text.is_none() {
        response_text = Some(
            "Disculpá, no logré resolverlo en este momento. Te derivo con un compañero del equipo."
                .to_string(),
        );
        escalated = true;
        escalation_reason = Some("max_iterations_reached".into());
    }

    // Mapear tokens a u32 para compatibilidad con el schema de AiInteraction.
    let total_in_u32 = total_in.max(0) as u32;
    let total_out_u32 = total_out.max(0) as u32;
    let total_thinking_u32 = total_thinking.max(0) as u32;
    let total_cached_u32 = total_cached.max(0) as u32;

    let cost_usd_estimate = crate::models::ai_agent::estimate_cost_usd(
        &agent.model.model_id,
        total_in_u32,
        total_cached_u32,
        total_out_u32,
        total_thinking_u32,
    );
    let latency_ms = started.elapsed().as_millis() as u32;

    // Extraer info del último transfer_to_agent exitoso (si lo hubo). El
    // result_summary está truncado a 500 chars pero los campos relevantes
    // (target_agent_id, mode, client_message corto) caben holgado.
    let transfer = tool_call_logs.iter().rev().find_map(|t| {
        if t.tool_name != "transfer_to_agent" || !t.success {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(&t.result_summary).ok()?;
        let target_hex = v.get("target_agent_id")?.as_str()?;
        let target_oid = mongodb::bson::oid::ObjectId::parse_str(target_hex).ok()?;
        let mode = v.get("mode").and_then(|m| m.as_str()).unwrap_or("");
        let cross_workspace = mode == "cross_workspace";
        let client_message = v
            .get("client_message")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());
        let reason = v
            .get("reason")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        Some(TransferInfo {
            target_agent_id: target_oid,
            cross_workspace,
            client_message,
            reason,
        })
    });

    Ok(RunnerOutput {
        response_text,
        tool_calls: tool_call_logs,
        input_tokens: total_in_u32,
        output_tokens: total_out_u32,
        thinking_tokens: total_thinking_u32,
        cached_tokens: total_cached_u32,
        total_tokens: total_in_u32.saturating_add(total_out_u32),
        cost_usd_estimate,
        latency_ms,
        escalated,
        escalation_reason,
        finish_reason,
        transfer,
        state_patches: state_patches_acc,
    })
}

/// Trunca el JSON serializado a 500 chars para no inflar la DB cuando se
/// persista `AiInteraction.tool_calls.result_summary`.
fn truncate_summary(value: &serde_json::Value) -> String {
    let s = value.to_string();
    if s.len() <= 500 {
        s
    } else {
        format!("{}…(truncated)", &s[..500])
    }
}

