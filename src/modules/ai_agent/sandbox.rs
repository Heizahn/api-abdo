//! Endpoint sandbox del AI Agent.
//!
//! `POST /v1/auth-user/whatsapp/ai-agent/agents/:agent_id/sandbox`
//!
//! Ejecuta un turno completo del runner con tools reales pero con
//! `is_sandbox=true` — no persiste `AiInteraction`, no crea tickets reales,
//! no toca conversaciones. Sirve para que el SUPERADMIN valide que el
//! system prompt + tools + api_key + relay del agente funcionan extremo a
//! extremo antes de pasar a `mode = live`.
//!
//! El body lleva `workspace_id` para que los tools tengan contexto del
//! número simulado (`business_phone`). Si el workspace no está en
//! `agent.workspace_ids` igualmente lo aceptamos — el sandbox es una
//! herramienta de testing, no quiero bloquearlo por config.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use mongodb::bson::oid::ObjectId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::{
    db::{AiAgentRepository, WhatsAppRepository},
    error::ApiError,
    models::{ai_agent::AiToolCallLog, users::User},
    state::AppState,
};

use super::{
    config_resolver::resolve_ai_api_key,
    openrouter::{resolve_base_url, AiRelay},
    runner::{run_turn, ConvRole, ConvTurn, PromptVariables},
    tools::{extract_allowed_transfer_targets, ToolContext},
};

const SUPERADMIN_ROLE: f32 = 0.0;
const HISTORY_MAX_TURNS: usize = 20;
const MESSAGE_MAX_CHARS: usize = 4_000;

fn require_superadmin(u: &User) -> Result<(), ApiError> {
    if u.role != SUPERADMIN_ROLE {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

fn parse_oid(s: &str, field: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(s).map_err(|_| ApiError::ValidationError {
        code: "invalid_id".into(),
        field: field.into(),
        message: format!("'{}' no es un ObjectId válido", field),
    })
}

// ============================================
// Request / response shapes
// ============================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct SandboxRequest {
    /// Workspace donde simular el turno — saca el `business_phone` que
    /// queda en `ToolContext`. Requerido.
    pub workspace_id: String,
    /// Mensaje "del cliente" simulado.
    pub message: String,
    #[serde(default)]
    pub history: Vec<SandboxHistoryEntry>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SandboxHistoryEntry {
    pub role: String,
    pub text: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SandboxResponse {
    pub ok: bool,
    pub data: SandboxData,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SandboxData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_text: Option<String>,
    pub tool_calls: Vec<SandboxToolCall>,
    pub usage: SandboxUsage,
    pub escalated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalation_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SandboxToolCall {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub result_summary: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u32,
}

impl From<AiToolCallLog> for SandboxToolCall {
    fn from(l: AiToolCallLog) -> Self {
        SandboxToolCall {
            tool_name: l.tool_name,
            args: l.args,
            result_summary: l.result_summary,
            success: l.success,
            error: l.error,
            duration_ms: l.duration_ms,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SandboxUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    pub cost_usd_estimate: f64,
    pub latency_ms: u32,
}

// ============================================
// Handler
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{agent_id}/sandbox",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("agent_id" = String, Path, description = "ObjectId hex del AiAgent")),
    request_body = SandboxRequest,
    responses(
        (status = 200, description = "Turno IA ejecutado en sandbox", body = SandboxResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 404, description = "agent_not_found / workspace_not_found"),
        (status = 422, description = "Validación: invalid_id / missing_field / field_too_long"),
        (status = 502, description = "ai_upstream_unreachable / ai_invalid_request / ai_auth_failed / ai_rate_limited"),
        (status = 503, description = "ai_api_key_missing"),
    )
)]
pub async fn sandbox_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(agent_id): Path<String>,
    Json(body): Json<SandboxRequest>,
) -> Result<Json<SandboxResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let agent_oid = parse_oid(&agent_id, "agent_id")?;
    let workspace_oid = parse_oid(&body.workspace_id, "workspace_id")?;

    let message = body.message.trim().to_string();
    if message.is_empty() {
        return Err(ApiError::ValidationError {
            code: "missing_field".into(),
            field: "message".into(),
            message: "El mensaje es requerido".into(),
        });
    }
    if message.chars().count() > MESSAGE_MAX_CHARS {
        return Err(ApiError::ValidationError {
            code: "field_too_long".into(),
            field: "message".into(),
            message: format!(
                "El mensaje no puede superar {} caracteres",
                MESSAGE_MAX_CHARS
            ),
        });
    }
    if body.history.len() > HISTORY_MAX_TURNS {
        return Err(ApiError::ValidationError {
            code: "history_too_long".into(),
            field: "history".into(),
            message: format!("El historial no puede superar {} turnos", HISTORY_MAX_TURNS),
        });
    }

    let history: Vec<ConvTurn> = body
        .history
        .into_iter()
        .filter_map(|h| {
            let role = match h.role.as_str() {
                "user" => ConvRole::User,
                "assistant" | "model" => ConvRole::Assistant,
                _ => return None,
            };
            let text = h.text.trim().to_string();
            if text.is_empty() {
                return None;
            }
            Some(ConvTurn { role, text })
        })
        .collect();

    let agent = state
        .db
        .find_ai_agent_by_id(&agent_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "agent_not_found",
                "Agente no encontrado",
            )
        })?;

    let wa_setting = state
        .db
        .find_wa_settings_by_id(&workspace_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "workspace_not_found",
                "El workspace de WhatsApp no existe",
            )
        })?;

    let api_key = resolve_ai_api_key(&state).await?;

    let faqs = state
        .db
        .list_ai_agent_faqs(&agent_oid)
        .await
        .map_err(ApiError::DatabaseError)?;
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
    let transfer_target_labels: Vec<(mongodb::bson::oid::ObjectId, String)> =
        if allowed_transfer_targets.is_empty() {
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
                Err(_) => Vec::new(),
            }
        };
    let agent_snapshot = std::sync::Arc::new(agent.clone());
    let tool_ctx = ToolContext {
        state: state.clone(),
        workspace_id: workspace_oid,
        business_phone: wa_setting.phone.clone(),
        agent_id: agent_oid,
        conversation_id: None,
        ai_user_id: agent.ai_user_id.clone(),
        ai_user_name: agent.personality.assistant_name.clone(),
        is_sandbox: true,
        allowed_transfer_targets,
        transfer_target_labels,
        agent_snapshot: agent_snapshot.clone(),
        default_ticket_category_id: agent.escalation.default_ticket_category_id.clone(),
        customer_explicit_zones: Vec::new(),
        session_media_ids: Vec::new(),
        current_turn_media_ids: Vec::new(),
        // Sandbox no tiene WaSettings real — defaulteamos a guardrails ON.
        // La gate de `is_sandbox` adicional en cada tool evita que el
        // guardrail bloquee testing de edge cases.
        workspace_enable_guardrails: true,
        // Sandbox no tiene conversación real — ownership guard se omite
        // cuando customer_phone está vacío.
        customer_phone: String::new(),
    };

    use chrono::Datelike;
    let now = crate::utils::timezone::VenezuelaDateTime::now();
    let weekday = match now.in_venezuela().weekday() {
        chrono::Weekday::Mon => "lunes",
        chrono::Weekday::Tue => "martes",
        chrono::Weekday::Wed => "miércoles",
        chrono::Weekday::Thu => "jueves",
        chrono::Weekday::Fri => "viernes",
        chrono::Weekday::Sat => "sábado",
        chrono::Weekday::Sun => "domingo",
    };
    let prompt_vars = PromptVariables {
        assistant_name: agent.personality.assistant_name.clone(),
        workspace_name: wa_setting.workspace_name.clone(),
        customer_name: String::new(),
        customer_phone: String::new(),
        business_phone: wa_setting.phone.clone(),
        today: now.date_string_venezuela(),
        weekday: weekday.to_string(),
    };

    let effective_base_url = resolve_base_url();

    let output = run_turn(
        &state.reqwest_client,
        &agent,
        &api_key,
        relay,
        &effective_base_url,
        &history,
        &message,
        &[],
        &[], // burst_intents — sandbox has no burst
        faqs_inline.as_deref(),
        None, // customer_context
        None, // transfer_context
        None, // first_turn_note
        None, // reopen_note — sandbox is stateless
        None, // agent_state
        None, // turn_state
        None, // conversation_state — sandbox is stateless (Phase 2)
        Some(&prompt_vars),
        &tool_ctx,
    )
    .await?;

    Ok(Json(SandboxResponse {
        ok: true,
        data: SandboxData {
            response_text: output.response_text,
            tool_calls: output.tool_calls.into_iter().map(Into::into).collect(),
            usage: SandboxUsage {
                input_tokens: output.input_tokens,
                output_tokens: output.output_tokens,
                total_tokens: output.total_tokens,
                cost_usd_estimate: output.cost_usd_estimate,
                latency_ms: output.latency_ms,
            },
            escalated: output.escalated,
            escalation_reason: output.escalation_reason,
            finish_reason: output.finish_reason,
        },
    }))
}
