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
    crypto::aes::decrypt_payload,
    error::ApiError,
    models::ai_agent::{AiAgent, AiInteraction, AiToolCallLog},
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
    gemini::{
        self, AiRelay, Content, FunctionCall, FunctionResponse, GenerateContentRequest,
        GenerationConfig, Part, SystemInstruction, ToolDeclaration, UsageMetadata,
    },
    tools::{build_function_declarations, execute_tool, ToolContext},
};

/// Adjunto multimedia que viene junto al texto del usuario en el primer
/// turno. El runner lo inyecta como `Part::inline` en `contents[0]`.
#[derive(Debug, Clone)]
pub struct MediaInput {
    pub mime_type: String,
    /// Base64 estándar (NO url-safe). El caller hace `STANDARD.encode(bytes)`.
    pub data_base64: String,
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

/// Salida del runner. El caller decide si persiste como `AiInteraction`.
#[derive(Debug, Clone)]
pub struct RunnerOutput {
    pub response_text: Option<String>,
    pub tool_calls: Vec<AiToolCallLog>,
    pub input_tokens: u32,
    pub output_tokens: u32,
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
}

impl RunnerOutput {
    /// Construye un `AiInteraction` listo para persistir (PR 3 lo va a usar
    /// desde el dispatch real). Sandbox descarta esto.
    #[allow(dead_code)]
    pub fn to_interaction(
        &self,
        conversation_id: mongodb::bson::oid::ObjectId,
        message_id: mongodb::bson::oid::ObjectId,
        workspace_id: mongodb::bson::oid::ObjectId,
        agent_id: mongodb::bson::oid::ObjectId,
        turn_index: u32,
        model_id: &str,
    ) -> AiInteraction {
        let now = mongodb::bson::DateTime::now();
        AiInteraction {
            id: None,
            conversation_id,
            message_id,
            workspace_id,
            agent_id,
            turn_index,
            model_id: model_id.to_string(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd_estimate: self.cost_usd_estimate,
            latency_ms: self.latency_ms,
            tool_calls: self.tool_calls.clone(),
            response_text: self.response_text.clone(),
            escalated: self.escalated,
            escalation_reason: self.escalation_reason.clone(),
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
    vars: Option<&PromptVariables>,
) -> SystemInstruction {
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

    if let Some(faqs) = faqs_inline {
        if !faqs.trim().is_empty() {
            chunks.push(format!("[faqs]\n{}", faqs.trim()));
        }
    }

    SystemInstruction {
        parts: vec![Part::text(chunks.join("\n\n"))],
    }
}

fn convert_history(history: &[ConvTurn]) -> Vec<Content> {
    history
        .iter()
        .map(|t| Content {
            role: match t.role {
                ConvRole::User => "user".into(),
                ConvRole::Assistant => "model".into(),
            },
            parts: vec![Part::text(&t.text)],
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
    history: &[ConvTurn],
    user_message: &str,
    user_media: &[MediaInput],
    faqs_inline: Option<&str>,
    customer_context: Option<&str>,
    transfer_context: Option<&str>,
    prompt_vars: Option<&PromptVariables>,
    tool_ctx: &ToolContext,
) -> Result<RunnerOutput, ApiError> {
    let started = Instant::now();
    let system_instruction = build_system_instruction(
        agent,
        faqs_inline,
        customer_context,
        transfer_context,
        prompt_vars,
    );

    let mut contents = convert_history(history);
    // Mensaje nuevo del cliente: texto + cualquier multimedia adjunta.
    let mut user_parts: Vec<Part> = Vec::with_capacity(1 + user_media.len());
    if !user_message.trim().is_empty() {
        user_parts.push(Part::text(user_message));
    }
    for m in user_media {
        user_parts.push(Part::inline(&m.mime_type, &m.data_base64));
    }
    if user_parts.is_empty() {
        // No hay nada que mandar. Defensivo.
        user_parts.push(Part::text(""));
    }
    contents.push(Content {
        role: "user".into(),
        parts: user_parts,
    });

    let function_declarations = build_function_declarations(agent);
    let tools_block = if function_declarations.is_empty() {
        None
    } else {
        Some(vec![ToolDeclaration {
            function_declarations,
        }])
    };

    let gen_config = GenerationConfig {
        temperature: Some(agent.model.temperature),
        max_output_tokens: Some(agent.model.max_tokens),
    };

    let mut total_in: u32 = 0;
    let mut total_out: u32 = 0;
    let mut tool_call_logs: Vec<AiToolCallLog> = Vec::new();
    let mut response_text: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut escalated = false;
    let mut escalation_reason: Option<String> = None;

    'turn: for iter in 0..MAX_ITERATIONS {
        let body = GenerateContentRequest {
            system_instruction: Some(SystemInstruction {
                parts: system_instruction.parts.iter().map(|p| Part {
                    text: p.text.clone(),
                    function_call: None,
                    function_response: None,
                    inline_data: None,
                }).collect(),
            }),
            contents: contents.clone(),
            tools: tools_block.clone(),
            generation_config: Some(GenerationConfig {
                temperature: gen_config.temperature,
                max_output_tokens: gen_config.max_output_tokens,
            }),
        };

        let resp = gemini::generate_content(
            http,
            api_key_decrypted,
            &agent.model.model_id,
            agent.model.timeout_seconds,
            &body,
            relay,
        )
        .await?;

        let usage = resp.usage_metadata.unwrap_or(UsageMetadata::default());
        total_in = total_in.saturating_add(usage.prompt_token_count);
        total_out = total_out.saturating_add(usage.candidates_token_count);

        let candidate = match resp.candidates.into_iter().next() {
            Some(c) => c,
            None => {
                // Sin candidatos = filtros de seguridad o input rechazado.
                tracing::warn!(
                    "[ai_agent] no candidates from gemini (iter {}), prompt_feedback={:?}",
                    iter,
                    resp.prompt_feedback
                );
                response_text = Some(
                    "Disculpá, no pude generar respuesta. Te conecto con un compañero del equipo."
                        .to_string(),
                );
                escalated = true;
                escalation_reason = Some("no_candidates".into());
                break 'turn;
            }
        };
        finish_reason = candidate.finish_reason.clone();

        // Separamos parts de texto vs function calls. Si hay ambos, el text
        // suele ser un comentario del modelo previo a la tool call —
        // prevalece el function call (debemos ejecutarlo y volver).
        let mut pending_calls: Vec<FunctionCall> = Vec::new();
        let mut accumulated_text = String::new();
        for p in &candidate.content.parts {
            if let Some(fc) = &p.function_call {
                pending_calls.push(fc.clone());
            } else if let Some(t) = &p.text {
                if !t.is_empty() {
                    accumulated_text.push_str(t);
                }
            }
        }

        if pending_calls.is_empty() {
            // Respuesta final en texto.
            response_text = Some(accumulated_text);
            break 'turn;
        }

        // Persistimos en `contents` el turno del modelo tal cual vino
        // (texto opcional + function calls), para que el siguiente roundtrip
        // mantenga el contexto coherente.
        contents.push(Content {
            role: "model".into(),
            parts: candidate.content.parts.clone(),
        });

        // Ejecutar cada function call y mandar la respuesta como `user` part.
        for call in pending_calls {
            // Detectar escalation tools antes de ejecutar (si el call es
            // exitoso, el flag queda en `true`; si falla, el LLM decide).
            let is_escalation = call.name == "request_human" || call.name == "create_ticket";

            let result = execute_tool(&call.name, call.args.clone(), tool_ctx).await;

            tool_call_logs.push(AiToolCallLog {
                tool_name: call.name.clone(),
                args: call.args.clone(),
                result_summary: truncate_summary(&result.data),
                success: result.success,
                error: result.error.clone(),
                duration_ms: result.duration_ms,
            });

            if is_escalation && result.success {
                escalated = true;
                escalation_reason = Some(format!("tool:{}", call.name));
            }

            // Empaquetar el resultado como functionResponse.
            let payload = if result.success {
                result.data
            } else {
                serde_json::json!({ "error": result.error.clone().unwrap_or_default() })
            };

            contents.push(Content {
                role: "user".into(),
                parts: vec![Part {
                    text: None,
                    function_call: None,
                    function_response: Some(FunctionResponse {
                        name: call.name,
                        response: payload,
                    }),
                    inline_data: None,
                }],
            });
        }
        // Si la IA escaló por create_ticket o request_human, igual seguimos
        // un turno más para que pueda producir una despedida en texto. El
        // loop se corta cuando esa respuesta llegue (ya no hay tool call).
    }

    if response_text.is_none() {
        // Salimos por max_iterations sin texto final.
        response_text = Some(
            "Disculpá, no logré resolverlo en este momento. Te derivo con un compañero del equipo."
                .to_string(),
        );
        escalated = true;
        escalation_reason = Some("max_iterations_reached".into());
    }

    let cost_usd_estimate =
        gemini::estimate_cost_usd(&agent.model.model_id, total_in, total_out);
    let latency_ms = started.elapsed().as_millis() as u32;

    Ok(RunnerOutput {
        response_text,
        tool_calls: tool_call_logs,
        input_tokens: total_in,
        output_tokens: total_out,
        total_tokens: total_in.saturating_add(total_out),
        cost_usd_estimate,
        latency_ms,
        escalated,
        escalation_reason,
        finish_reason,
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

// ============================================
// Helper para descifrar api_key del setting
// ============================================

/// Descifra `model.api_key_encrypted` o devuelve un error 503 si no hay key.
pub fn decrypt_api_key(agent: &AiAgent, secret: &str) -> Result<String, ApiError> {
    let enc = &agent.model.api_key_encrypted;
    if enc.is_empty() {
        return Err(ApiError::domain_simple(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "ai_api_key_missing",
            "El agente no tiene api_key de Gemini configurada",
        ));
    }
    decrypt_payload(secret, enc).ok_or_else(|| {
        ApiError::domain_simple(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "ai_api_key_corrupt",
            "No se pudo descifrar la api_key — posible cambio de JWT_SECRET",
        )
    })
}
