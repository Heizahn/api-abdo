//! Handlers HTTP del módulo AI Agent — modelo agent-centric.
//!
//! Endpoints (todos SUPERADMIN-only):
//! - GET    /v1/auth-user/whatsapp/ai-agent/agents               (con ?workspace_id=)
//! - POST   /v1/auth-user/whatsapp/ai-agent/agents
//! - GET    /v1/auth-user/whatsapp/ai-agent/agents/:id
//! - PATCH  /v1/auth-user/whatsapp/ai-agent/agents/:id
//! - DELETE /v1/auth-user/whatsapp/ai-agent/agents/:id
//! - POST   /v1/auth-user/whatsapp/ai-agent/test-connection                  (raw, pre-creación)
//! - GET    /v1/auth-user/whatsapp/ai-agent/models                           (raw, ?api_key=)
//! - POST   /v1/auth-user/whatsapp/ai-agent/agents/:id/test-connection
//! - GET    /v1/auth-user/whatsapp/ai-agent/agents/:id/models
//! - GET    /v1/auth-user/whatsapp/ai-agent/agents/:id/faqs
//! - POST   /v1/auth-user/whatsapp/ai-agent/agents/:id/faqs
//! - PATCH  /v1/auth-user/whatsapp/ai-agent/agents/faqs/item/:id
//! - DELETE /v1/auth-user/whatsapp/ai-agent/agents/faqs/item/:id
//!
//! La `api_key` viaja en claro en el body y se cifra con AES-GCM reusando
//! `JWT_SECRET` (mismo patrón que `WaSettings.access_token_cipher`).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

use crate::{
    crypto::aes::encrypt_payload,
    db::{
        AiAgentRepository, AiConfigRepository, MetricsGranularity, UserRepository,
        WhatsAppRepository,
    },
    error::ApiError,
    models::{
        ai_agent::{
            AiAgent, AiAgentDeleteResponse, AiAgentExportData, AiAgentExportResponse, AiAgentFaq,
            AiAgentFaqItem, AiAgentFaqListResponse, AiAgentFaqResponse, AiAgentImportData,
            AiAgentImportRequest, AiAgentImportResponse, AiAgentItem, AiAgentMetricsDailyBucketDto,
            AiAgentMetricsData, AiAgentMetricsResponse, AiAgentMode, AiAgentPreClassBreakdown,
            AiAgentResponse, AiAgentTransferTargetRef, AiAgentsExportPackageData,
            AiAgentsExportPackageResponse, AiAgentsImportPackageData, AiAgentsImportPackageRequest,
            AiAgentsImportPackageResponse, AiAgentsListResponse, AiConfigDto, AiConfigPatchRequest,
            AiConfigResponse, AiEscalationRules, AiEscalationRulesInput, AiLimits, AiLimitsInput,
            AiModelConfig, AiModelConfigInput, AiPersonality, AiPersonalityInput, AiSchedule,
            AiScheduleInput, AiToolConfig, AiToolConfigInput, CreateAiAgentFaqRequest,
            CreateAiAgentRequest, TestConnectionData, TestConnectionRequest,
            TestConnectionResponse, TestConnectionSource, UpdateAiAgentFaqRequest,
            UpdateAiAgentRequest,
        },
        users::User,
    },
    state::AppState,
};

use super::{
    ai_agent_secret,
    config_resolver::resolve_ai_api_key,
    openrouter::{resolve_base_url, AiRelay, OpenRouterClient},
};

const SUPERADMIN_ROLE: f32 = 0.0;
/// Sentinel para `nRole` del bot. 99 deja libres 6/7/8 para roles humanos
/// futuros y señala visualmente que es no-humano.
const AI_AGENT_ROLE: f32 = 99.0;

const LABEL_MAX_LEN: usize = 100;
const DESCRIPTION_MAX_LEN: usize = 500;
const PROMPT_MAX_LEN: usize = 30_000;
const FAQ_QUESTION_MAX_LEN: usize = 500;
const FAQ_ANSWER_MAX_LEN: usize = 4_000;
const FAQ_TAG_MAX_LEN: usize = 64;
const FAQ_TAGS_MAX_COUNT: usize = 16;

const TEST_TIMEOUT_MAX: u32 = 30;
const DEFAULT_TEST_MODEL: &str = "openai/gpt-4o-mini";
const AI_AGENT_EXPORT_SCHEMA_VERSION: u32 = 1;

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

fn iso8601(d: BsonDateTime) -> String {
    d.try_to_rfc3339_string().unwrap_or_default()
}

fn validate_string_len(value: &str, field: &str, max: usize) -> Result<(), ApiError> {
    if value.chars().count() > max {
        return Err(ApiError::ValidationError {
            code: "field_too_long".into(),
            field: field.into(),
            message: format!("'{}' no puede superar {} caracteres", field, max),
        });
    }
    Ok(())
}

fn validate_required(value: &str, field: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() {
        return Err(ApiError::ValidationError {
            code: "missing_field".into(),
            field: field.into(),
            message: format!("'{}' es requerido", field),
        });
    }
    Ok(())
}

fn validate_tags(tags: &[String]) -> Result<(), ApiError> {
    if tags.len() > FAQ_TAGS_MAX_COUNT {
        return Err(ApiError::ValidationError {
            code: "too_many_tags".into(),
            field: "tags".into(),
            message: format!("Máximo {} tags por FAQ", FAQ_TAGS_MAX_COUNT),
        });
    }
    for t in tags {
        if t.trim().is_empty() {
            return Err(ApiError::ValidationError {
                code: "empty_tag".into(),
                field: "tags".into(),
                message: "Las tags no pueden ser strings vacíos".into(),
            });
        }
        validate_string_len(t, "tags[]", FAQ_TAG_MAX_LEN)?;
    }
    Ok(())
}

fn agent_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "agent_not_found",
        "Agente no encontrado",
    )
}

fn faq_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "ai_agent_faq_not_found",
        "FAQ no encontrada",
    )
}

// ============================================
// Defaults
// ============================================

fn default_agent(
    label: String,
    description: String,
    ai_user_id: String,
    now: BsonDateTime,
) -> AiAgent {
    AiAgent {
        id: None,
        label,
        description,
        is_receptionist: false,
        workspace_ids: Vec::new(),
        enabled: false,
        mode: AiAgentMode::Shadow,
        ai_user_id,
        schedule: AiSchedule {
            timezone: "America/Caracas".into(),
            always_on: true,
            weekdays: vec![1, 2, 3, 4, 5, 6, 7],
            from_hour: 0,
            to_hour: 23,
        },
        model: AiModelConfig {
            provider: "openrouter".into(),
            model_id: "openai/gpt-4o-mini".into(),
            temperature: 0.7,
            // 2000 default. Antes era 500 que se quedaba corto cuando el
            // agente combinaba check_coverage + list_plans + recomendación
            // + cierre en un solo turno (Carla truncaba a media palabra).
            max_tokens: 2000,
            // 20s default — los modelos con function calling tardan 5-15s
            // en armar la respuesta. 10s era insuficiente.
            timeout_seconds: 20,
            api_key_encrypted: String::new(),
        },
        personality: AiPersonality {
            assistant_name: "Asistente Virtual".into(),
            locale: "es-VE".into(),
            tone: "warm-coloquial".into(),
            greeting: String::new(),
            farewell: String::new(),
            farewell_to_human: String::new(),
            forbidden_phrases: Vec::new(),
        },
        system_prompt: String::new(),
        tools: vec![
            AiToolConfig {
                name: "lookup_customer".into(),
                enabled: true,
                description_override: None,
                config: None,
            },
            AiToolConfig {
                name: "get_invoices".into(),
                enabled: true,
                description_override: None,
                config: None,
            },
            AiToolConfig {
                name: "request_human".into(),
                enabled: true,
                description_override: None,
                config: None,
            },
            AiToolConfig {
                name: "create_ticket".into(),
                // Tickets quedan desactivados por defecto para nuevos agentes:
                // el flujo principal de WhatsApp IA usa alertas visuales del
                // chat + request_human/asignación humana.
                enabled: false,
                description_override: None,
                config: None,
            },
            AiToolConfig {
                name: "transfer_to_agent".into(),
                enabled: false,
                description_override: None,
                config: None,
            },
            AiToolConfig {
                name: "list_plans".into(),
                enabled: false,
                description_override: None,
                config: None,
            },
            AiToolConfig {
                name: "check_coverage".into(),
                enabled: false,
                description_override: None,
                config: None,
            },
        ],
        escalation: AiEscalationRules {
            keywords: vec![
                "humano".into(),
                "operador".into(),
                "queja".into(),
                "reclamo".into(),
            ],
            max_turns_without_resolution: 3,
            qualification_window_turns: 0,
            max_identification_attempts: 2,
            escalate_on_critical_tool_failure: true,
            always_escalate_when_asked: true,
            default_ticket_category_id: Some("soporte_primer_segundo_nivel".into()),
        },
        limits: AiLimits::defaults(),
        debounce_seconds: 10,
        purpose: None,
        created_at: now,
        updated_at: now,
    }
}

// ============================================
// Conversión a DTO
// ============================================

fn agent_to_item(a: AiAgent) -> AiAgentItem {
    let api_key_set = !a.model.api_key_encrypted.is_empty();
    AiAgentItem {
        id: a.id.map(|o| o.to_hex()).unwrap_or_default(),
        label: a.label,
        description: a.description,
        is_receptionist: a.is_receptionist,
        purpose: a.purpose,
        workspace_ids: a.workspace_ids.into_iter().map(|o| o.to_hex()).collect(),
        enabled: a.enabled,
        mode: a.mode,
        ai_user_id: a.ai_user_id,
        schedule: a.schedule.into(),
        model: crate::models::ai_agent::AiModelConfigDto {
            provider: a.model.provider,
            model_id: a.model.model_id,
            temperature: a.model.temperature,
            max_tokens: a.model.max_tokens,
            timeout_seconds: a.model.timeout_seconds,
            api_key_set,
        },
        personality: a.personality.into(),
        system_prompt: a.system_prompt,
        tools: a.tools.into_iter().map(Into::into).collect(),
        escalation: a.escalation.into(),
        limits: a.limits.into(),
        debounce_seconds: a.debounce_seconds,
        created_at: iso8601(a.created_at),
        updated_at: iso8601(a.updated_at),
    }
}

fn faq_to_item(f: AiAgentFaq) -> AiAgentFaqItem {
    AiAgentFaqItem {
        id: f.id.map(|o| o.to_hex()).unwrap_or_default(),
        agent_id: f.agent_id.to_hex(),
        question: f.question,
        answer: f.answer,
        tags: f.tags,
        created_at: iso8601(f.created_at),
        updated_at: iso8601(f.updated_at),
    }
}

fn schedule_to_input(s: &AiSchedule) -> AiScheduleInput {
    AiScheduleInput {
        timezone: Some(s.timezone.clone()),
        always_on: Some(s.always_on),
        weekdays: Some(s.weekdays.clone()),
        from_hour: Some(s.from_hour),
        to_hour: Some(s.to_hour),
    }
}

fn model_to_input(m: &AiModelConfig) -> AiModelConfigInput {
    AiModelConfigInput {
        model_id: Some(m.model_id.clone()),
        temperature: Some(m.temperature),
        max_tokens: Some(m.max_tokens),
        timeout_seconds: Some(m.timeout_seconds),
        // Nunca exportamos secretos. La key efectiva vive en AiConfig global.
        api_key: None,
    }
}

fn personality_to_input(p: &AiPersonality) -> AiPersonalityInput {
    AiPersonalityInput {
        assistant_name: Some(p.assistant_name.clone()),
        locale: Some(p.locale.clone()),
        tone: Some(p.tone.clone()),
        greeting: Some(p.greeting.clone()),
        farewell: Some(p.farewell.clone()),
        farewell_to_human: Some(p.farewell_to_human.clone()),
        forbidden_phrases: Some(p.forbidden_phrases.clone()),
    }
}

fn tool_to_input(t: &AiToolConfig) -> AiToolConfigInput {
    AiToolConfigInput {
        name: t.name.clone(),
        enabled: t.enabled,
        description_override: t.description_override.clone(),
        config: t.config.clone(),
    }
}

fn escalation_to_input(e: &AiEscalationRules) -> AiEscalationRulesInput {
    AiEscalationRulesInput {
        keywords: Some(e.keywords.clone()),
        max_turns_without_resolution: Some(e.max_turns_without_resolution),
        qualification_window_turns: Some(e.qualification_window_turns),
        max_identification_attempts: Some(e.max_identification_attempts),
        escalate_on_critical_tool_failure: Some(e.escalate_on_critical_tool_failure),
        always_escalate_when_asked: Some(e.always_escalate_when_asked),
        default_ticket_category_id: e.default_ticket_category_id.clone(),
    }
}

fn limits_to_input(l: &AiLimits) -> AiLimitsInput {
    AiLimitsInput {
        max_turns_per_day: Some(l.max_turns_per_day),
        max_turns_per_conversation: Some(l.max_turns_per_conversation),
        max_tokens_per_day: Some(l.max_tokens_per_day),
        cost_alert_threshold_pct: Some(l.cost_alert_threshold_pct),
    }
}

fn agent_to_create_request(a: &AiAgent) -> CreateAiAgentRequest {
    CreateAiAgentRequest {
        label: a.label.clone(),
        description: a.description.clone(),
        is_receptionist: Some(a.is_receptionist),
        purpose: a.purpose,
        workspace_ids: a.workspace_ids.iter().map(|id| id.to_hex()).collect(),
        enabled: Some(a.enabled),
        mode: Some(a.mode),
        schedule: Some(schedule_to_input(&a.schedule)),
        model: Some(model_to_input(&a.model)),
        personality: Some(personality_to_input(&a.personality)),
        system_prompt: Some(a.system_prompt.clone()),
        tools: Some(a.tools.iter().map(tool_to_input).collect()),
        escalation: Some(escalation_to_input(&a.escalation)),
        limits: Some(limits_to_input(&a.limits)),
        debounce_seconds: Some(a.debounce_seconds),
    }
}

fn faq_to_create_request(f: &AiAgentFaq) -> CreateAiAgentFaqRequest {
    CreateAiAgentFaqRequest {
        question: f.question.clone(),
        answer: f.answer.clone(),
        tags: f.tags.clone(),
    }
}

fn transfer_target_ids_from_tools(tools: &[AiToolConfig]) -> Vec<ObjectId> {
    let mut ids = Vec::new();
    for tool in tools.iter().filter(|t| t.name == "transfer_to_agent") {
        let Some(arr) = tool
            .config
            .as_ref()
            .and_then(|cfg| cfg.get("allowed_targets"))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for value in arr {
            let Some(raw) = value.as_str() else { continue };
            if let Ok(oid) = ObjectId::parse_str(raw) {
                if !ids.contains(&oid) {
                    ids.push(oid);
                }
            }
        }
    }
    ids
}

async fn export_data_for_agent(
    state: &Arc<AppState>,
    agent: AiAgent,
) -> Result<AiAgentExportData, ApiError> {
    let agent_id = agent
        .id
        .ok_or_else(|| ApiError::Internal("agent sin _id".into()))?;
    let faqs = state
        .db
        .list_ai_agent_faqs(&agent_id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let target_ids = transfer_target_ids_from_tools(&agent.tools);
    let targets = state
        .db
        .find_ai_agents_by_ids(&target_ids)
        .await
        .map_err(ApiError::DatabaseError)?;
    let transfer_targets = targets
        .into_iter()
        .filter_map(|target| {
            let source_id = target.id.map(|id| id.to_hex())?;
            Some(AiAgentTransferTargetRef {
                source_id,
                label: target.label,
                purpose: target.purpose,
            })
        })
        .collect();

    Ok(AiAgentExportData {
        schema_version: AI_AGENT_EXPORT_SCHEMA_VERSION,
        source_agent_id: agent_id.to_hex(),
        agent: agent_to_create_request(&agent),
        faqs: faqs.iter().map(faq_to_create_request).collect(),
        transfer_targets,
    })
}

// ============================================
// AI user sintético — uno por agente
// ============================================

/// Crea un `User` bot atado a un agente. Idempotente — la primera persistencia
/// del agente lo crea con email sintético; ediciones posteriores reusan el
/// mismo (no se renombra aunque cambie el `label` del agente).
async fn ensure_ai_user_for_agent(
    state: &Arc<AppState>,
    agent_label: &str,
    creator_id: &str,
) -> Result<String, ApiError> {
    let now = mongodb::bson::DateTime::now();
    let user = User {
        id: uuid::Uuid::new_v4().to_string(),
        name: agent_label.to_string(),
        role: AI_AGENT_ROLE,
        // Email único por agente — si dos agentes se llaman "Soporte" igual
        // tienen UUIDs distintos, así que el email no choca.
        email: format!("ai-agent-{}@internal", uuid::Uuid::new_v4()),
        visible: false,
        can_chat: false,
        is_bot: true,
        tag: None,
        id_creator: Some(creator_id.to_string()),
        role_prev: None,
        d_creation: Some(mongodb::bson::Bson::DateTime(now)),
    };
    state
        .db
        .create_user(user.clone())
        .await
        .map_err(ApiError::DatabaseError)?;
    Ok(user.id)
}

// ============================================
// Validación de workspace_ids (cada uno existe en WaSettings)
// ============================================

/// Valida la `config` de cada tool del agente — hoy el único shape custom es
/// `transfer_to_agent.config = { allowed_targets: [<oid_hex>...] }`.
///
/// Reglas para `transfer_to_agent`:
/// - Si `enabled = true`, `allowed_targets` debe estar y ser array no vacío.
/// - Cada id válido como ObjectId.
/// - Cada id ≠ `current_agent_id` (no puede transferirse a sí mismo).
/// - Cada id existe en `AiAgents`.
///
/// `current_agent_id` viene `None` en POST (todavía no existe) — la validación
/// de "self" se omite en ese caso porque el agente nuevo no tiene id aún.
async fn validate_tools_config(
    state: &Arc<AppState>,
    tools: &[AiToolConfig],
    current_agent_id: Option<&ObjectId>,
) -> Result<(), ApiError> {
    let Some(transfer) = tools.iter().find(|t| t.name == "transfer_to_agent") else {
        return Ok(());
    };
    if !transfer.enabled {
        return Ok(());
    }

    let cfg = transfer
        .config
        .as_ref()
        .ok_or_else(|| ApiError::ValidationError {
            code: "transfer_targets_required".into(),
            field: "tools.transfer_to_agent.config.allowed_targets".into(),
            message: "Seleccioná al menos un agente destino para habilitar transfer_to_agent"
                .into(),
        })?;
    let arr = cfg
        .get("allowed_targets")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ApiError::ValidationError {
            code: "transfer_targets_required".into(),
            field: "tools.transfer_to_agent.config.allowed_targets".into(),
            message: "Seleccioná al menos un agente destino para habilitar transfer_to_agent"
                .into(),
        })?;
    if arr.is_empty() {
        return Err(ApiError::ValidationError {
            code: "transfer_targets_required".into(),
            field: "tools.transfer_to_agent.config.allowed_targets".into(),
            message: "Seleccioná al menos un agente destino para habilitar transfer_to_agent"
                .into(),
        });
    }

    let mut target_oids = Vec::with_capacity(arr.len());
    for v in arr {
        let s = v.as_str().ok_or_else(|| ApiError::ValidationError {
            code: "invalid_transfer_target".into(),
            field: "tools.transfer_to_agent.config.allowed_targets".into(),
            message: "Cada `allowed_target` debe ser un ObjectId hex".into(),
        })?;
        let oid = ObjectId::parse_str(s).map_err(|_| ApiError::ValidationError {
            code: "invalid_transfer_target".into(),
            field: "tools.transfer_to_agent.config.allowed_targets".into(),
            message: format!("'{}' no es un ObjectId válido", s),
        })?;
        if let Some(cur) = current_agent_id {
            if &oid == cur {
                return Err(ApiError::ValidationError {
                    code: "transfer_target_is_self".into(),
                    field: "tools.transfer_to_agent.config.allowed_targets".into(),
                    message: "El agente no puede transferirse a sí mismo".into(),
                });
            }
        }
        target_oids.push(oid);
    }

    let found = state
        .db
        .find_ai_agents_by_ids(&target_oids)
        .await
        .map_err(ApiError::DatabaseError)?;
    if found.len() != target_oids.len() {
        let found_set: std::collections::HashSet<_> = found.iter().filter_map(|a| a.id).collect();
        let missing: Vec<String> = target_oids
            .iter()
            .filter(|o| !found_set.contains(o))
            .map(|o| o.to_hex())
            .collect();
        return Err(ApiError::ValidationError {
            code: "transfer_target_not_found".into(),
            field: "tools.transfer_to_agent.config.allowed_targets".into(),
            message: format!("Agentes destino inexistentes: {}", missing.join(", ")),
        });
    }

    Ok(())
}

async fn parse_and_validate_workspace_ids(
    state: &Arc<AppState>,
    raw: &[String],
) -> Result<Vec<ObjectId>, ApiError> {
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let oid = ObjectId::parse_str(r).map_err(|_| ApiError::ValidationError {
            code: "invalid_workspace_id".into(),
            field: "workspace_ids".into(),
            message: format!("'{}' no es un ObjectId válido", r),
        })?;
        if state
            .db
            .find_wa_settings_by_id(&oid)
            .await
            .map_err(ApiError::DatabaseError)?
            .is_none()
        {
            return Err(ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "workspace_not_found",
                &format!("Workspace '{}' no existe", r),
            ));
        }
        out.push(oid);
    }
    Ok(out)
}

// ============================================
// CRUD AGENTES
// ============================================

#[derive(Debug, Deserialize)]
pub struct ListAgentsQuery {
    /// Si viene, filtra agentes que atienden ese workspace.
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/agents",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(
        ("workspace_id" = Option<String>, Query, description = "Filtrar por workspace"),
    ),
    responses(
        (status = 200, description = "Lista de agentes", body = AiAgentsListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
    )
)]
pub async fn list_ai_agents_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Query(q): Query<ListAgentsQuery>,
) -> Result<Json<AiAgentsListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let ws_oid = match q.workspace_id.as_deref() {
        Some(s) if !s.trim().is_empty() => Some(parse_oid(s.trim(), "workspace_id")?),
        _ => None,
    };
    let agents = state
        .db
        .list_ai_agents(ws_oid.as_ref())
        .await
        .map_err(ApiError::DatabaseError)?;
    Ok(Json(AiAgentsListResponse {
        ok: true,
        data: agents.into_iter().map(agent_to_item).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    responses(
        (status = 200, description = "Detalle del agente", body = AiAgentResponse),
        (status = 404, description = "agent_not_found"),
    )
)]
pub async fn get_ai_agent_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<AiAgentResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let agent = state
        .db
        .find_ai_agent_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(agent_not_found)?;
    Ok(Json(AiAgentResponse {
        ok: true,
        data: agent_to_item(agent),
    }))
}

async fn create_ai_agent_from_request(
    state: &Arc<AppState>,
    current_user: &User,
    body: CreateAiAgentRequest,
) -> Result<AiAgent, ApiError> {
    let label = body.label.trim().to_string();
    let description = body.description.trim().to_string();
    validate_required(&label, "label")?;
    validate_required(&description, "description")?;
    validate_string_len(&label, "label", LABEL_MAX_LEN)?;
    validate_string_len(&description, "description", DESCRIPTION_MAX_LEN)?;
    if let Some(p) = body.system_prompt.as_deref() {
        validate_string_len(p, "system_prompt", PROMPT_MAX_LEN)?;
    }

    let workspace_oids = parse_and_validate_workspace_ids(state, &body.workspace_ids).await?;

    let ai_user_id = ensure_ai_user_for_agent(state, &label, &current_user.id).await?;
    let now = BsonDateTime::now();
    let mut agent = default_agent(label, description, ai_user_id, now);
    agent.workspace_ids = workspace_oids;

    if let Some(v) = body.is_receptionist {
        agent.is_receptionist = v;
    }
    if let Some(v) = body.purpose {
        agent.purpose = Some(v);
    }
    if let Some(v) = body.enabled {
        agent.enabled = v;
    }
    if let Some(v) = body.mode {
        agent.mode = v;
    }
    apply_schedule(&mut agent.schedule, body.schedule);
    if let Some(ref m) = body.model {
        if m.api_key
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            tracing::warn!(
                "[ai_agent.handler] create/import: api_key en body está deprecada y es ignorada. \
                 Configurar la key global en PATCH /v1/auth-user/whatsapp/ai-agent/config"
            );
        }
    }
    apply_model(&mut agent.model, body.model)?;
    agent.model.api_key_encrypted = String::new();
    apply_personality(&mut agent.personality, body.personality);
    if let Some(sp) = body.system_prompt {
        agent.system_prompt = sp;
    }
    if let Some(tools) = body.tools {
        agent.tools = tools
            .into_iter()
            .map(|t| AiToolConfig {
                name: t.name,
                enabled: t.enabled,
                description_override: t.description_override,
                config: t.config,
            })
            .collect();
    }
    apply_escalation(&mut agent.escalation, body.escalation)?;
    apply_limits(&mut agent.limits, body.limits);
    if let Some(d) = body.debounce_seconds {
        agent.debounce_seconds = d;
    }

    validate_tools_config(state, &agent.tools, None).await?;

    state
        .db
        .create_ai_agent(agent)
        .await
        .map_err(ApiError::DatabaseError)
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/agents",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    request_body = CreateAiAgentRequest,
    responses(
        (status = 201, description = "Agente creado", body = AiAgentResponse),
        (status = 400, description = "qualification_window_turns_out_of_range"),
        (status = 422, description = "Validación"),
        (status = 404, description = "workspace_not_found"),
    )
)]
pub async fn create_ai_agent_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<CreateAiAgentRequest>,
) -> Result<(StatusCode, Json<AiAgentResponse>), ApiError> {
    require_superadmin(&current_user)?;
    let saved = create_ai_agent_from_request(&state, &current_user, body).await?;

    Ok((
        StatusCode::CREATED,
        Json(AiAgentResponse {
            ok: true,
            data: agent_to_item(saved),
        }),
    ))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    request_body = UpdateAiAgentRequest,
    responses(
        (status = 200, description = "Agente actualizado", body = AiAgentResponse),
        (status = 400, description = "qualification_window_turns_out_of_range"),
        (status = 404, description = "agent_not_found / workspace_not_found"),
        (status = 422, description = "Validación"),
    )
)]
pub async fn update_ai_agent_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAiAgentRequest>,
) -> Result<Json<AiAgentResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;

    if let Some(l) = body.label.as_deref() {
        validate_required(l.trim(), "label")?;
        validate_string_len(l, "label", LABEL_MAX_LEN)?;
    }
    if let Some(d) = body.description.as_deref() {
        validate_required(d.trim(), "description")?;
        validate_string_len(d, "description", DESCRIPTION_MAX_LEN)?;
    }
    if let Some(p) = body.system_prompt.as_deref() {
        validate_string_len(p, "system_prompt", PROMPT_MAX_LEN)?;
    }

    let new_workspace_oids = match body.workspace_ids.as_ref() {
        Some(raw) => Some(parse_and_validate_workspace_ids(&state, raw).await?),
        None => None,
    };

    // I2: api_key en body está deprecated — emitir warn, ignorar. No se escribe
    // api_key_encrypted en el agente. La key global va en PATCH /config.
    let api_key_in_body = body
        .model
        .as_ref()
        .and_then(|m| m.api_key.as_deref())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if api_key_in_body {
        tracing::warn!(
            "[ai_agent.handler] update: api_key en body está deprecada y es ignorada. \
             Configurar la key global en PATCH /v1/auth-user/whatsapp/ai-agent/config"
        );
    }
    let api_key_rotated = false; // deprecated — cache invalidation no longer needed per-agent

    let mut agent = state
        .db
        .find_ai_agent_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(agent_not_found)?;

    if let Some(v) = body.label {
        agent.label = v.trim().to_string();
    }
    if let Some(v) = body.description {
        agent.description = v.trim().to_string();
    }
    if let Some(v) = body.is_receptionist {
        agent.is_receptionist = v;
    }
    if let Some(v) = body.purpose {
        agent.purpose = Some(v);
    }
    if let Some(v) = new_workspace_oids {
        agent.workspace_ids = v;
    }
    if let Some(v) = body.enabled {
        agent.enabled = v;
    }
    if let Some(v) = body.mode {
        agent.mode = v;
    }
    apply_schedule(&mut agent.schedule, body.schedule);
    // Strippear api_key del model input antes de apply_model (deprecada — se ignora).
    let model_without_key = body.model.map(|mut m| {
        m.api_key = None;
        m
    });
    apply_model(&mut agent.model, model_without_key)?;
    apply_personality(&mut agent.personality, body.personality);
    if let Some(sp) = body.system_prompt {
        agent.system_prompt = sp;
    }
    if let Some(tools) = body.tools {
        agent.tools = tools
            .into_iter()
            .map(|t| AiToolConfig {
                name: t.name,
                enabled: t.enabled,
                description_override: t.description_override,
                config: t.config,
            })
            .collect();
    }
    apply_escalation(&mut agent.escalation, body.escalation)?;
    apply_limits(&mut agent.limits, body.limits);
    if let Some(d) = body.debounce_seconds {
        agent.debounce_seconds = d;
    }
    agent.updated_at = BsonDateTime::now();

    validate_tools_config(&state, &agent.tools, Some(&oid)).await?;

    let saved = state
        .db
        .replace_ai_agent(&oid, agent)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(agent_not_found)?;

    // api_key_rotated is always false (per-agent key deprecated); no cache invalidation needed.
    let _ = api_key_rotated;

    Ok(Json(AiAgentResponse {
        ok: true,
        data: agent_to_item(saved),
    }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    responses(
        (status = 200, description = "Agente eliminado", body = AiAgentDeleteResponse),
        (status = 404, description = "agent_not_found"),
    )
)]
pub async fn delete_ai_agent_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<AiAgentDeleteResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let ok = state
        .db
        .delete_ai_agent(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !ok {
        return Err(agent_not_found());
    }
    state.redis.invalidate_ai_models_cache(&oid.to_hex()).await;
    Ok(Json(AiAgentDeleteResponse { ok: true }))
}

fn transfer_target_unresolved(source_id: &str) -> ApiError {
    ApiError::ValidationError {
        code: "transfer_target_unresolved".into(),
        field: "tools.transfer_to_agent.config.allowed_targets".into(),
        message: format!(
            "No se pudo resolver el agente destino '{}' en este entorno",
            source_id
        ),
    }
}

fn resolve_transfer_target_id(
    source_id: &str,
    refs: &[AiAgentTransferTargetRef],
    source_to_new: &HashMap<String, String>,
    existing_agents: &[AiAgent],
) -> Option<String> {
    if let Some(mapped) = source_to_new.get(source_id) {
        return Some(mapped.clone());
    }

    if existing_agents
        .iter()
        .any(|a| a.id.map(|id| id.to_hex()).as_deref() == Some(source_id))
    {
        return Some(source_id.to_string());
    }

    let meta = refs.iter().find(|r| r.source_id == source_id)?;
    if let Some(purpose) = meta.purpose {
        let mut candidates = existing_agents
            .iter()
            .filter(|a| a.purpose == Some(purpose));
        if let Some(exact_label) = candidates
            .clone()
            .find(|a| a.label.eq_ignore_ascii_case(&meta.label))
        {
            return exact_label.id.map(|id| id.to_hex());
        }
        if let Some(first) = candidates.next() {
            return first.id.map(|id| id.to_hex());
        }
    }

    existing_agents
        .iter()
        .find(|a| a.label.eq_ignore_ascii_case(&meta.label))
        .and_then(|a| a.id.map(|id| id.to_hex()))
}

fn rewrite_transfer_targets(
    tools: &mut [AiToolConfigInput],
    refs: &[AiAgentTransferTargetRef],
    source_to_new: &HashMap<String, String>,
    existing_agents: &[AiAgent],
) -> Result<(), ApiError> {
    for tool in tools.iter_mut().filter(|t| t.name == "transfer_to_agent") {
        let Some(cfg) = tool.config.as_mut() else {
            continue;
        };
        let Some(arr) = cfg.get("allowed_targets").and_then(|v| v.as_array()) else {
            continue;
        };

        let mut rewritten = Vec::new();
        for value in arr {
            let Some(source_id) = value.as_str() else {
                if tool.enabled {
                    return Err(ApiError::ValidationError {
                        code: "invalid_transfer_target".into(),
                        field: "tools.transfer_to_agent.config.allowed_targets".into(),
                        message: "Cada allowed_target debe ser string".into(),
                    });
                }
                continue;
            };

            match resolve_transfer_target_id(source_id, refs, source_to_new, existing_agents) {
                Some(resolved) => rewritten.push(serde_json::Value::String(resolved)),
                None if tool.enabled => return Err(transfer_target_unresolved(source_id)),
                None => {}
            }
        }
        cfg["allowed_targets"] = serde_json::Value::Array(rewritten);
    }

    Ok(())
}

fn disable_transfer_to_agent_for_bootstrap(body: &mut CreateAiAgentRequest) {
    let Some(tools) = body.tools.as_mut() else {
        return;
    };
    for tool in tools.iter_mut().filter(|t| t.name == "transfer_to_agent") {
        tool.enabled = false;
    }
}

async fn import_faqs_for_agent(
    state: &Arc<AppState>,
    agent_id: ObjectId,
    faqs: &[CreateAiAgentFaqRequest],
) -> Result<usize, ApiError> {
    for f in faqs {
        let question = f.question.trim().to_string();
        let answer = f.answer.trim().to_string();
        validate_required(&question, "question")?;
        validate_required(&answer, "answer")?;
        validate_string_len(&question, "question", FAQ_QUESTION_MAX_LEN)?;
        validate_string_len(&answer, "answer", FAQ_ANSWER_MAX_LEN)?;
        validate_tags(&f.tags)?;

        let now = BsonDateTime::now();
        let faq = AiAgentFaq {
            id: None,
            agent_id,
            question,
            answer,
            tags: f.tags.clone(),
            created_at: now,
            updated_at: now,
        };
        state
            .db
            .create_ai_agent_faq(faq)
            .await
            .map_err(ApiError::DatabaseError)?;
    }
    Ok(faqs.len())
}

async fn create_imported_agent(
    state: &Arc<AppState>,
    current_user: &User,
    data: &AiAgentExportData,
    workspace_ids_override: Option<&[String]>,
    source_to_new: &HashMap<String, String>,
) -> Result<(AiAgent, usize), ApiError> {
    if data.schema_version != AI_AGENT_EXPORT_SCHEMA_VERSION {
        return Err(ApiError::ValidationError {
            code: "unsupported_schema_version".into(),
            field: "schema_version".into(),
            message: format!("schema_version {} no soportado", data.schema_version),
        });
    }

    let existing_agents = state
        .db
        .list_ai_agents(None)
        .await
        .map_err(ApiError::DatabaseError)?;
    let mut body = data.agent.clone();
    if let Some(override_ids) = workspace_ids_override {
        body.workspace_ids = override_ids.to_vec();
    }
    if let Some(tools) = body.tools.as_mut() {
        rewrite_transfer_targets(
            tools,
            &data.transfer_targets,
            source_to_new,
            &existing_agents,
        )?;
    }

    let saved = create_ai_agent_from_request(state, current_user, body).await?;
    let saved_id = saved
        .id
        .ok_or_else(|| ApiError::Internal("agente importado sin _id".into()))?;
    let imported_faqs = import_faqs_for_agent(state, saved_id, &data.faqs).await?;

    Ok((saved, imported_faqs))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{id}/export",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    responses(
        (status = 200, description = "Export portable del agente", body = AiAgentExportResponse),
        (status = 404, description = "agent_not_found"),
    )
)]
pub async fn export_ai_agent_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<AiAgentExportResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let agent = state
        .db
        .find_ai_agent_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(agent_not_found)?;
    Ok(Json(AiAgentExportResponse {
        ok: true,
        data: export_data_for_agent(&state, agent).await?,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ExportPackageQuery {
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/export-package",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("workspace_id" = Option<String>, Query, description = "Filtrar por workspace")),
    responses(
        (status = 200, description = "Paquete portable de agentes", body = AiAgentsExportPackageResponse),
    )
)]
pub async fn export_ai_agents_package_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Query(q): Query<ExportPackageQuery>,
) -> Result<Json<AiAgentsExportPackageResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let ws_oid = match q.workspace_id.as_deref() {
        Some(s) if !s.trim().is_empty() => Some(parse_oid(s.trim(), "workspace_id")?),
        _ => None,
    };
    let agents = state
        .db
        .list_ai_agents(ws_oid.as_ref())
        .await
        .map_err(ApiError::DatabaseError)?;
    let mut exported = Vec::with_capacity(agents.len());
    for agent in agents {
        exported.push(export_data_for_agent(&state, agent).await?);
    }

    Ok(Json(AiAgentsExportPackageResponse {
        ok: true,
        data: AiAgentsExportPackageData {
            schema_version: AI_AGENT_EXPORT_SCHEMA_VERSION,
            agents: exported,
        },
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/import",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    request_body = AiAgentImportRequest,
    responses(
        (status = 201, description = "Agente importado", body = AiAgentImportResponse),
        (status = 422, description = "Validación"),
    )
)]
pub async fn import_ai_agent_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<AiAgentImportRequest>,
) -> Result<(StatusCode, Json<AiAgentImportResponse>), ApiError> {
    require_superadmin(&current_user)?;
    let (saved, imported_faqs) = create_imported_agent(
        &state,
        &current_user,
        &body.data,
        body.workspace_ids_override.as_deref(),
        &HashMap::new(),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(AiAgentImportResponse {
            ok: true,
            data: AiAgentImportData {
                source_agent_id: body.data.source_agent_id,
                agent: agent_to_item(saved),
                imported_faqs,
            },
        }),
    ))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/import-package",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    request_body = AiAgentsImportPackageRequest,
    responses(
        (status = 201, description = "Paquete de agentes importado", body = AiAgentsImportPackageResponse),
        (status = 422, description = "Validación"),
    )
)]
pub async fn import_ai_agents_package_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<AiAgentsImportPackageRequest>,
) -> Result<(StatusCode, Json<AiAgentsImportPackageResponse>), ApiError> {
    require_superadmin(&current_user)?;
    if body.data.schema_version != AI_AGENT_EXPORT_SCHEMA_VERSION {
        return Err(ApiError::ValidationError {
            code: "unsupported_schema_version".into(),
            field: "schema_version".into(),
            message: format!("schema_version {} no soportado", body.data.schema_version),
        });
    }

    let mut source_to_new = HashMap::new();
    let mut created = Vec::with_capacity(body.data.agents.len());

    for data in &body.data.agents {
        let mut bootstrap = data.clone();
        disable_transfer_to_agent_for_bootstrap(&mut bootstrap.agent);
        let (saved, imported_faqs) = create_imported_agent(
            &state,
            &current_user,
            &bootstrap,
            body.workspace_ids_override.as_deref(),
            &HashMap::new(),
        )
        .await?;
        let new_id = saved
            .id
            .ok_or_else(|| ApiError::Internal("agente importado sin _id".into()))?
            .to_hex();
        source_to_new.insert(data.source_agent_id.clone(), new_id);
        created.push((data, saved, imported_faqs));
    }

    let existing_agents = state
        .db
        .list_ai_agents(None)
        .await
        .map_err(ApiError::DatabaseError)?;
    let mut imported = Vec::with_capacity(created.len());

    for (data, mut saved, imported_faqs) in created {
        let mut final_body = data.agent.clone();
        if let Some(override_ids) = body.workspace_ids_override.as_deref() {
            final_body.workspace_ids = override_ids.to_vec();
        }
        if let Some(tools) = final_body.tools.as_mut() {
            rewrite_transfer_targets(
                tools,
                &data.transfer_targets,
                &source_to_new,
                &existing_agents,
            )?;
            saved.tools = tools
                .iter()
                .map(|t| AiToolConfig {
                    name: t.name.clone(),
                    enabled: t.enabled,
                    description_override: t.description_override.clone(),
                    config: t.config.clone(),
                })
                .collect();
        }
        saved.updated_at = BsonDateTime::now();
        let saved_id = saved
            .id
            .ok_or_else(|| ApiError::Internal("agente importado sin _id".into()))?;
        validate_tools_config(&state, &saved.tools, Some(&saved_id)).await?;
        let replaced = state
            .db
            .replace_ai_agent(&saved_id, saved)
            .await
            .map_err(ApiError::DatabaseError)?
            .ok_or_else(agent_not_found)?;

        imported.push(AiAgentImportData {
            source_agent_id: data.source_agent_id.clone(),
            agent: agent_to_item(replaced),
            imported_faqs,
        });
    }

    Ok((
        StatusCode::CREATED,
        Json(AiAgentsImportPackageResponse {
            ok: true,
            data: AiAgentsImportPackageData { imported },
        }),
    ))
}

// ============================================
// Apply helpers (merge campo a campo)
// ============================================

fn apply_schedule(cur: &mut AiSchedule, patch: Option<crate::models::ai_agent::AiScheduleInput>) {
    let Some(p) = patch else { return };
    if let Some(v) = p.timezone {
        cur.timezone = v;
    }
    if let Some(v) = p.always_on {
        cur.always_on = v;
    }
    if let Some(v) = p.weekdays {
        cur.weekdays = v;
    }
    if let Some(v) = p.from_hour {
        cur.from_hour = v;
    }
    if let Some(v) = p.to_hour {
        cur.to_hour = v;
    }
}

fn apply_model(
    cur: &mut AiModelConfig,
    patch: Option<crate::models::ai_agent::AiModelConfigInput>,
) -> Result<(), ApiError> {
    let Some(p) = patch else { return Ok(()) };
    if let Some(v) = p.model_id {
        cur.model_id = v;
    }
    if let Some(v) = p.temperature {
        cur.temperature = v;
    }
    if let Some(v) = p.max_tokens {
        cur.max_tokens = v;
    }
    if let Some(v) = p.timeout_seconds {
        cur.timeout_seconds = v;
    }
    if let Some(raw) = p.api_key.as_deref() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            cur.api_key_encrypted = encrypt_payload(&ai_agent_secret(), trimmed);
        }
    }
    Ok(())
}

fn apply_personality(
    cur: &mut AiPersonality,
    patch: Option<crate::models::ai_agent::AiPersonalityInput>,
) {
    let Some(p) = patch else { return };
    if let Some(v) = p.assistant_name {
        cur.assistant_name = v;
    }
    if let Some(v) = p.locale {
        cur.locale = v;
    }
    if let Some(v) = p.tone {
        cur.tone = v;
    }
    if let Some(v) = p.greeting {
        cur.greeting = v;
    }
    if let Some(v) = p.farewell {
        cur.farewell = v;
    }
    if let Some(v) = p.farewell_to_human {
        cur.farewell_to_human = v;
    }
    if let Some(v) = p.forbidden_phrases {
        cur.forbidden_phrases = v;
    }
}

fn apply_escalation(
    cur: &mut AiEscalationRules,
    patch: Option<crate::models::ai_agent::AiEscalationRulesInput>,
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
            tracing::warn!(
                "[ai_agent.handler] qualification_window_turns rejected: value={} is out of range 0..=10",
                v
            );
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

fn apply_limits(cur: &mut AiLimits, patch: Option<crate::models::ai_agent::AiLimitsInput>) {
    let Some(p) = patch else { return };
    if let Some(v) = p.max_turns_per_day {
        cur.max_turns_per_day = v;
    }
    if let Some(v) = p.max_turns_per_conversation {
        cur.max_turns_per_conversation = v;
    }
    if let Some(v) = p.max_tokens_per_day {
        cur.max_tokens_per_day = v;
    }
    if let Some(v) = p.cost_alert_threshold_pct {
        cur.cost_alert_threshold_pct = v;
    }
}

// ============================================
// FAQs (anidadas bajo agentes)
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{id}/faqs",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    responses(
        (status = 200, description = "FAQs del agente", body = AiAgentFaqListResponse),
        (status = 404, description = "agent_not_found"),
    )
)]
pub async fn list_ai_agent_faqs_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<AiAgentFaqListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    if state
        .db
        .find_ai_agent_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .is_none()
    {
        return Err(agent_not_found());
    }
    let items = state
        .db
        .list_ai_agent_faqs(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;
    Ok(Json(AiAgentFaqListResponse {
        ok: true,
        data: items.into_iter().map(faq_to_item).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{id}/faqs",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    request_body = CreateAiAgentFaqRequest,
    responses(
        (status = 201, description = "FAQ creada", body = AiAgentFaqResponse),
        (status = 404, description = "agent_not_found"),
        (status = 422, description = "Validación"),
    )
)]
pub async fn create_ai_agent_faq_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(body): Json<CreateAiAgentFaqRequest>,
) -> Result<(StatusCode, Json<AiAgentFaqResponse>), ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    if state
        .db
        .find_ai_agent_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .is_none()
    {
        return Err(agent_not_found());
    }

    let question = body.question.trim().to_string();
    let answer = body.answer.trim().to_string();
    validate_required(&question, "question")?;
    validate_required(&answer, "answer")?;
    validate_string_len(&question, "question", FAQ_QUESTION_MAX_LEN)?;
    validate_string_len(&answer, "answer", FAQ_ANSWER_MAX_LEN)?;
    validate_tags(&body.tags)?;

    let now = BsonDateTime::now();
    let faq = AiAgentFaq {
        id: None,
        agent_id: oid,
        question,
        answer,
        tags: body.tags,
        created_at: now,
        updated_at: now,
    };
    let saved = state
        .db
        .create_ai_agent_faq(faq)
        .await
        .map_err(ApiError::DatabaseError)?;
    Ok((
        StatusCode::CREATED,
        Json(AiAgentFaqResponse {
            ok: true,
            data: faq_to_item(saved),
        }),
    ))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/faqs/item/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    request_body = UpdateAiAgentFaqRequest,
    responses(
        (status = 200, description = "FAQ actualizada", body = AiAgentFaqResponse),
        (status = 404, description = "ai_agent_faq_not_found"),
        (status = 422, description = "Validación"),
    )
)]
pub async fn update_ai_agent_faq_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAiAgentFaqRequest>,
) -> Result<Json<AiAgentFaqResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;

    let question = match body.question {
        Some(q) => {
            let trimmed = q.trim().to_string();
            validate_required(&trimmed, "question")?;
            validate_string_len(&trimmed, "question", FAQ_QUESTION_MAX_LEN)?;
            Some(trimmed)
        }
        None => None,
    };
    let answer = match body.answer {
        Some(a) => {
            let trimmed = a.trim().to_string();
            validate_required(&trimmed, "answer")?;
            validate_string_len(&trimmed, "answer", FAQ_ANSWER_MAX_LEN)?;
            Some(trimmed)
        }
        None => None,
    };
    if let Some(ref tags) = body.tags {
        validate_tags(tags)?;
    }

    let updated = state
        .db
        .update_ai_agent_faq(&oid, question, answer, body.tags)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(faq_not_found)?;
    Ok(Json(AiAgentFaqResponse {
        ok: true,
        data: faq_to_item(updated),
    }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/faqs/item/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    responses(
        (status = 200, description = "FAQ eliminada", body = AiAgentDeleteResponse),
        (status = 404, description = "ai_agent_faq_not_found"),
    )
)]
pub async fn delete_ai_agent_faq_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<AiAgentDeleteResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let ok = state
        .db
        .delete_ai_agent_faq(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !ok {
        return Err(faq_not_found());
    }
    Ok(Json(AiAgentDeleteResponse { ok: true }))
}

// ============================================
// TEST CONNECTION (raw — pre-creación)
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/test-connection",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    request_body = TestConnectionRequest,
    responses(
        (status = 200, description = "Conexión OK", body = TestConnectionResponse),
        (status = 502, description = "ai_auth_failed / ai_model_not_found / ai_upstream_unreachable / ai_rate_limited"),
        (status = 503, description = "ai_global_config_missing — no hay api_key en el body ni AiConfig.openrouter_api_key configurado"),
    )
)]
pub async fn test_connection_raw_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<TestConnectionRequest>,
) -> Result<Json<TestConnectionResponse>, ApiError> {
    require_superadmin(&current_user)?;

    // Body wins if api_key is present and non-empty; otherwise fall back to AiConfig.openrouter_api_key.
    // Mirrors the exact pattern used in test_connection_for_agent_handler.
    let body_key = body
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (api_key, source) = match body_key {
        Some(k) => (k.to_string(), TestConnectionSource::Body),
        None => (
            resolve_ai_api_key(&state).await?,
            TestConnectionSource::Stored,
        ),
    };

    let model_id = body
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_TEST_MODEL)
        .to_string();
    let _timeout = body
        .timeout_seconds
        .map(|n| n.clamp(1, TEST_TIMEOUT_MAX))
        .unwrap_or(10);

    let relay = AiRelay::from_config(&state.config);
    let base_url = resolve_base_url();
    let or_client = OpenRouterClient::new(
        state.reqwest_client.clone(),
        base_url,
        api_key.clone(),
        relay,
    );
    or_client.test_connection(&model_id).await?;

    Ok(Json(TestConnectionResponse {
        ok: true,
        data: TestConnectionData {
            reachable: true,
            model_id,
            source,
        },
    }))
}

// ============================================
// TEST CONNECTION por agente
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{id}/test-connection",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    request_body = TestConnectionRequest,
    responses(
        (status = 200, description = "Conexión OK", body = TestConnectionResponse),
        (status = 404, description = "agent_not_found"),
        (status = 502, description = "ai_auth_failed / ai_model_not_found / ai_upstream_unreachable / ai_rate_limited"),
        (status = 503, description = "ai_api_key_missing"),
    )
)]
pub async fn test_connection_for_agent_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(body): Json<TestConnectionRequest>,
) -> Result<Json<TestConnectionResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let agent = state
        .db
        .find_ai_agent_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(agent_not_found)?;

    let body_key = body
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (api_key, source) = match body_key {
        Some(k) => (k.to_string(), TestConnectionSource::Body),
        None => (
            resolve_ai_api_key(&state).await?,
            TestConnectionSource::Stored,
        ),
    };

    let model_id = body
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| agent.model.model_id.clone());

    let timeout = body
        .timeout_seconds
        .map(|n| n.clamp(1, TEST_TIMEOUT_MAX))
        .unwrap_or(10);

    let relay = AiRelay::from_config(&state.config);
    let base_url = resolve_base_url();
    let or_client = OpenRouterClient::new(
        state.reqwest_client.clone(),
        base_url,
        api_key.clone(),
        relay,
    );
    // timeout está calculado arriba pero OpenRouterClient usa su propio backoff interno;
    // el campo se ignora — se deja en la firma por compatibilidad con TestConnectionRequest.
    let _ = timeout;
    or_client.test_connection(&model_id).await?;

    Ok(Json(TestConnectionResponse {
        ok: true,
        data: TestConnectionData {
            reachable: true,
            model_id,
            source,
        },
    }))
}

// ──────────────────────────────────────────────────────────────────────────────
// Phase 3a — Metrics handler
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MetricsQueryParams {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub granularity: Option<String>,
}

/// Parsea un string ISO-8601 UTC (ej. "2026-05-01T00:00:00Z") a `chrono::DateTime<Utc>`.
fn parse_iso(s: &str) -> Result<chrono::DateTime<chrono::Utc>, ()> {
    s.parse::<chrono::DateTime<chrono::Utc>>().map_err(|_| ())
}

/// `GET /v1/auth-user/whatsapp/ai-agent/agents/:id/metrics`
///
/// Devuelve métricas de uso del agente en el rango `[from, to]`.
/// `granularity=summary` (default): totales del período.
/// `granularity=daily`: totales por día en TZ Caracas.
#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{id}/metrics",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(
        ("id"          = String,          Path,  description = "AiAgent ObjectId hex"),
        ("from"        = String,          Query, description = "ISO-8601 UTC timestamp inclusive (ej. 2026-05-01T00:00:00Z)"),
        ("to"          = String,          Query, description = "ISO-8601 UTC timestamp inclusive"),
        ("granularity" = Option<String>,  Query, description = "summary | daily (default: summary)"),
    ),
    responses(
        (status = 200, description = "OK",            body = AiAgentMetricsResponse),
        (status = 400, description = "Bad request"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — solo SUPERADMIN"),
        (status = 404, description = "Agente no encontrado"),
    )
)]
pub async fn get_ai_agent_metrics_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(agent_id_hex): Path<String>,
    Query(params): Query<MetricsQueryParams>,
) -> Result<Json<AiAgentMetricsResponse>, ApiError> {
    require_superadmin(&current_user)?;

    // Spec 30.2: invalid_agent_id (no genérico invalid_id).
    let agent_id = ObjectId::parse_str(&agent_id_hex).map_err(|_| ApiError::ValidationError {
        code: "invalid_agent_id".into(),
        field: "id".into(),
        message: "El agent_id no es un ObjectId válido".into(),
    })?;

    // Spec 30.2: invalid_date_range cubre todos los problemas de fechas
    // (parse fail de from, parse fail de to, o from > to). Front recibe un
    // único code en vez de tres distintos.
    let from_dt = parse_iso(&params.from).map_err(|_| ApiError::ValidationError {
        code: "invalid_date_range".into(),
        field: "from".into(),
        message: "'from' inválido — usa ISO-8601 UTC (ej. 2026-05-01T00:00:00Z)".into(),
    })?;
    let to_dt = parse_iso(&params.to).map_err(|_| ApiError::ValidationError {
        code: "invalid_date_range".into(),
        field: "to".into(),
        message: "'to' inválido — usa ISO-8601 UTC (ej. 2026-05-31T23:59:59Z)".into(),
    })?;
    if to_dt < from_dt {
        return Err(ApiError::ValidationError {
            code: "invalid_date_range".into(),
            field: "to".into(),
            message: "'to' debe ser mayor o igual que 'from'".into(),
        });
    }

    let granularity = match params.granularity.as_deref() {
        Some("daily") => MetricsGranularity::Daily,
        Some("summary") | None => MetricsGranularity::Summary,
        Some(_) => {
            return Err(ApiError::ValidationError {
                code: "invalid_granularity".into(),
                field: "granularity".into(),
                message: "Valor inválido. Usa 'summary' o 'daily'".into(),
            })
        }
    };

    // 404 si el agente no existe.
    state
        .db
        .find_ai_agent_by_id(&agent_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(agent_not_found)?;

    let from_bson = mongodb::bson::DateTime::from_millis(from_dt.timestamp_millis());
    let to_bson = mongodb::bson::DateTime::from_millis(to_dt.timestamp_millis());

    let raw = state
        .db
        .get_ai_agent_metrics(&agent_id, from_bson, to_bson, granularity)
        .await
        .map_err(ApiError::DatabaseError)?;

    // Mapear breakdown: HashMap<String, u64> → AiAgentPreClassBreakdown.
    let bd = &raw.pre_class_breakdown;
    let breakdown = AiAgentPreClassBreakdown {
        spam: *bd.get("Spam").unwrap_or(&0),
        greeting_only: *bd.get("GreetingOnly").unwrap_or(&0),
        clear_ventas: *bd.get("ClearVentas").unwrap_or(&0),
        clear_pagos: *bd.get("ClearPagos").unwrap_or(&0),
        clear_soporte: *bd.get("ClearSoporte").unwrap_or(&0),
        ambiguous: *bd.get("Ambiguous").unwrap_or(&0),
    };

    // Mapear daily buckets si los hay.
    let daily = raw.daily.map(|buckets| {
        buckets
            .into_iter()
            .map(|b| AiAgentMetricsDailyBucketDto {
                date: b.date,
                total_turns: b.total_turns,
                total_input_tokens: b.total_input_tokens,
                total_output_tokens: b.total_output_tokens,
                total_thinking_tokens: b.total_thinking_tokens,
                total_cached_tokens: b.total_cached_tokens,
                total_cost_usd: b.total_cost_usd,
                pre_classified_count: b.pre_classified_count,
                escalated_count: b.escalated_count,
            })
            .collect()
    });

    let s = &raw.summary;
    Ok(Json(AiAgentMetricsResponse {
        ok: true,
        data: AiAgentMetricsData {
            total_turns: s.total_turns,
            total_input_tokens: s.total_input_tokens,
            total_output_tokens: s.total_output_tokens,
            total_thinking_tokens: s.total_thinking_tokens,
            total_cached_tokens: s.total_cached_tokens,
            total_cost_usd: s.total_cost_usd,
            avg_latency_ms: s.avg_latency_ms,
            pre_classified_count: s.pre_classified_count,
            escalated_count: s.escalated_count,
            tool_calls_count: s.tool_calls_count,
            // Spec 30.3: hit-rate del implicit caching del provider vía OpenRouter.
            // 0.0 cuando no hay input (rango vacío) para evitar div-by-zero.
            cache_hit_rate: if s.total_input_tokens == 0 {
                0.0
            } else {
                s.total_cached_tokens as f64 / s.total_input_tokens as f64
            },
            pre_class_breakdown: breakdown,
            daily,
        },
    }))
}

// ──────────────────────────────────────────────────────────────────────────────
// Global AI Config — GET + PATCH (SUPERADMIN only)
// ──────────────────────────────────────────────────────────────────────────────

const MAX_API_KEY_LEN: usize = 200;
const MAX_MODEL_LEN: usize = 100;

/// `GET /v1/auth-user/whatsapp/ai-agent/config`
///
/// Devuelve el estado actual de la configuración global de AI.
/// La API key nunca se devuelve — solo `has_api_key: bool`.
#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/config",
    tag = "AI Agent",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Configuración global de AI", body = AiConfigResponse),
        (status = 401, description = "Missing/invalid token"),
        (status = 403, description = "No es SUPERADMIN"),
    )
)]
pub async fn get_ai_config_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
) -> Result<Json<AiConfigResponse>, ApiError> {
    require_superadmin(&current_user)?;

    // Lee directo de DB (no de cache) — la UI debe ver estado fresco tras un PATCH.
    let dto = match state.db.get_ai_config().await {
        Ok(Some(cfg)) => cfg.to_dto(),
        Ok(None) => AiConfigDto::default(),
        Err(_) => {
            return Err(ApiError::domain_simple(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Error leyendo configuración",
            ))
        }
    };

    Ok(Json(AiConfigResponse {
        ok: true,
        data: dto,
    }))
}

/// `PATCH /v1/auth-user/whatsapp/ai-agent/config`
///
/// Actualiza la configuración global de AI (parcial). Al menos un campo
/// debe estar presente. La API key se cifra antes de persistir.
/// Invalida Redis `ai_agent:config` tras escribir.
#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/ai-agent/config",
    tag = "AI Agent",
    security(("bearerAuth" = [])),
    request_body = AiConfigPatchRequest,
    responses(
        (status = 200, description = "Configuración actualizada", body = AiConfigResponse),
        (status = 400, description = "empty_patch o campo demasiado largo"),
        (status = 401, description = "Missing/invalid token"),
        (status = 403, description = "No es SUPERADMIN"),
    )
)]
pub async fn patch_ai_config_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<AiConfigPatchRequest>,
) -> Result<Json<AiConfigResponse>, ApiError> {
    require_superadmin(&current_user)?;

    // Normalizar campos.
    let trimmed_key = body.api_key.as_ref().map(|s| s.trim().to_string());
    let trimmed_model = body.default_model.as_ref().map(|s| s.trim().to_string());
    let trimmed_text_model = body.text_model.as_ref().map(|s| s.trim().to_string());
    let trimmed_vision_model = body.vision_model.as_ref().map(|s| s.trim().to_string());
    let trimmed_stt_model = body.stt_model.as_ref().map(|s| s.trim().to_string());
    let trimmed_stt_language = body.stt_language.as_ref().map(|s| s.trim().to_string());

    let key_present = trimmed_key.as_deref().map_or(false, |s| !s.is_empty());
    let model_present = trimmed_model.as_deref().map_or(false, |s| !s.is_empty());
    let text_model_present = trimmed_text_model
        .as_deref()
        .map_or(false, |s| !s.is_empty());
    let vision_model_present = trimmed_vision_model
        .as_deref()
        .map_or(false, |s| !s.is_empty());
    let stt_model_present = trimmed_stt_model
        .as_deref()
        .map_or(false, |s| !s.is_empty());
    let stt_language_present = trimmed_stt_language
        .as_deref()
        .map_or(false, |s| !s.is_empty());
    let transcription_field_present = body.audio_transcription_enabled.is_some()
        || stt_model_present
        || stt_language_present
        || body.show_audio_transcription.is_some()
        || body.ai_uses_audio_transcription.is_some()
        || body.max_audio_transcription_seconds.is_some();

    let model_field_present = model_present || text_model_present || vision_model_present;

    // Al menos un campo reconocido debe estar presente y no vacío.
    if !key_present && !model_field_present && !transcription_field_present {
        return Err(ApiError::ValidationError {
            code: "empty_patch".into(),
            field: "request_body".into(),
            message: "Debe proveer al menos api_key, default_model, text_model o vision_model"
                .into(),
        });
    }

    // Validar longitudes.
    if let Some(ref k) = trimmed_key {
        if key_present && k.len() > MAX_API_KEY_LEN {
            return Err(ApiError::ValidationError {
                code: "field_too_long".into(),
                field: "api_key".into(),
                message: format!("api_key supera {} caracteres", MAX_API_KEY_LEN),
            });
        }
    }
    if let Some(ref m) = trimmed_model {
        if model_present && m.len() > MAX_MODEL_LEN {
            return Err(ApiError::ValidationError {
                code: "field_too_long".into(),
                field: "default_model".into(),
                message: format!("default_model supera {} caracteres", MAX_MODEL_LEN),
            });
        }
    }
    if let Some(ref m) = trimmed_text_model {
        if text_model_present && m.len() > MAX_MODEL_LEN {
            return Err(ApiError::ValidationError {
                code: "field_too_long".into(),
                field: "text_model".into(),
                message: format!("text_model supera {} caracteres", MAX_MODEL_LEN),
            });
        }
    }
    if let Some(ref m) = trimmed_vision_model {
        if vision_model_present && m.len() > MAX_MODEL_LEN {
            return Err(ApiError::ValidationError {
                code: "field_too_long".into(),
                field: "vision_model".into(),
                message: format!("vision_model supera {} caracteres", MAX_MODEL_LEN),
            });
        }
    }
    if let Some(ref m) = trimmed_stt_model {
        if stt_model_present && m.len() > MAX_MODEL_LEN {
            return Err(ApiError::ValidationError {
                code: "field_too_long".into(),
                field: "stt_model".into(),
                message: format!("stt_model supera {} caracteres", MAX_MODEL_LEN),
            });
        }
    }

    // Cifrar la api_key si viene.
    let api_key_cipher = if key_present {
        let plain = trimmed_key.unwrap();
        Some(encrypt_payload(&ai_agent_secret(), &plain))
    } else {
        None
    };

    let model_to_set = if model_present { trimmed_model } else { None };
    let text_model_to_set = if text_model_present {
        trimmed_text_model
    } else {
        None
    };
    let vision_model_to_set = if vision_model_present {
        trimmed_vision_model
    } else {
        None
    };
    let stt_model_to_set = if stt_model_present {
        trimmed_stt_model
    } else {
        None
    };
    let stt_language_to_set = if stt_language_present {
        trimmed_stt_language
    } else {
        None
    };

    let updated = state
        .db
        .upsert_ai_config(
            api_key_cipher,
            model_to_set,
            text_model_to_set,
            vision_model_to_set,
            body.audio_transcription_enabled,
            stt_model_to_set,
            stt_language_to_set,
            body.show_audio_transcription,
            body.ai_uses_audio_transcription,
            body.max_audio_transcription_seconds,
            &current_user.id,
        )
        .await
        .map_err(|_| {
            ApiError::domain_simple(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Error guardando configuración",
            )
        })?;

    // Invalidar cache para que el próximo dispatch lea la key fresca.
    state.redis.invalidate_ai_config_cache().await;

    Ok(Json(AiConfigResponse {
        ok: true,
        data: updated.to_dto(),
    }))
}
