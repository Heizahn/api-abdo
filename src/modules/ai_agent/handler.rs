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
use std::sync::Arc;

use crate::{
    crypto::aes::encrypt_payload,
    db::{AiAgentRepository, UserRepository, WhatsAppRepository},
    error::ApiError,
    models::{
        ai_agent::{
            AiAgent, AiAgentDeleteResponse, AiAgentFaq, AiAgentFaqItem, AiAgentFaqListResponse,
            AiAgentFaqResponse, AiAgentItem, AiAgentMode, AiAgentModelItem,
            AiAgentModelsListResponse, AiAgentResponse, AiAgentsListResponse, AiEscalationRules,
            AiLimits, AiModelConfig, AiPersonality, AiSchedule, AiToolConfig,
            CreateAiAgentFaqRequest, CreateAiAgentRequest, TestConnectionData,
            TestConnectionRequest, TestConnectionResponse, TestConnectionSource,
            UpdateAiAgentFaqRequest, UpdateAiAgentRequest,
        },
        users::User,
    },
    state::AppState,
};

use super::{gemini::AiRelay, runner::decrypt_api_key};

const SUPERADMIN_ROLE: f32 = 0.0;
/// Sentinel para `nRole` del bot. 99 deja libres 6/7/8 para roles humanos
/// futuros y señala visualmente que es no-humano.
const AI_AGENT_ROLE: f32 = 99.0;

const LABEL_MAX_LEN: usize = 100;
const DESCRIPTION_MAX_LEN: usize = 500;
const PROMPT_MAX_LEN: usize = 16_000;
const FAQ_QUESTION_MAX_LEN: usize = 500;
const FAQ_ANSWER_MAX_LEN: usize = 4_000;
const FAQ_TAG_MAX_LEN: usize = 64;
const FAQ_TAGS_MAX_COUNT: usize = 16;

const MODELS_CACHE_TTL_SECS: u64 = 600;
const MODELS_FETCH_TIMEOUT: u32 = 15;
const TEST_TIMEOUT_MAX: u32 = 30;
const DEFAULT_TEST_MODEL: &str = "gemini-1.5-flash-latest";

const RECOMMENDED_MODEL_IDS: &[&str] = &[
    "gemini-1.5-flash-latest",
    "gemini-1.5-pro-latest",
];

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

fn ai_agent_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
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

fn missing_api_key() -> ApiError {
    ApiError::domain_simple(
        StatusCode::BAD_REQUEST,
        "missing_api_key",
        "Pasá `api_key` o configurá la del agente antes",
    )
}

// ============================================
// Defaults
// ============================================

fn default_agent(label: String, description: String, ai_user_id: String, now: BsonDateTime) -> AiAgent {
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
            provider: "gemini".into(),
            model_id: "gemini-1.5-flash-latest".into(),
            temperature: 0.7,
            max_tokens: 500,
            timeout_seconds: 10,
            api_key_encrypted: String::new(),
        },
        personality: AiPersonality {
            assistant_name: "Asistente Virtual".into(),
            locale: "es-VE".into(),
            tone: "warm-coloquial".into(),
            greeting: String::new(),
            farewell: String::new(),
            forbidden_phrases: Vec::new(),
        },
        system_prompt: String::new(),
        tools: vec![
            AiToolConfig { name: "lookup_customer".into(), enabled: true, description_override: None },
            AiToolConfig { name: "get_invoices".into(),    enabled: true, description_override: None },
            AiToolConfig { name: "request_human".into(),   enabled: true, description_override: None },
            AiToolConfig { name: "create_ticket".into(),   enabled: true, description_override: None },
        ],
        escalation: AiEscalationRules {
            keywords: vec!["humano".into(), "operador".into(), "queja".into(), "reclamo".into()],
            max_turns_without_resolution: 3,
            max_identification_attempts: 2,
            escalate_on_critical_tool_failure: true,
            always_escalate_when_asked: true,
            default_ticket_category_id: Some("soporte_primer_segundo_nivel".into()),
        },
        limits: AiLimits::defaults(),
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

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/agents",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    request_body = CreateAiAgentRequest,
    responses(
        (status = 201, description = "Agente creado", body = AiAgentResponse),
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

    let label = body.label.trim().to_string();
    let description = body.description.trim().to_string();
    validate_required(&label, "label")?;
    validate_required(&description, "description")?;
    validate_string_len(&label, "label", LABEL_MAX_LEN)?;
    validate_string_len(&description, "description", DESCRIPTION_MAX_LEN)?;
    if let Some(p) = body.system_prompt.as_deref() {
        validate_string_len(p, "system_prompt", PROMPT_MAX_LEN)?;
    }

    let workspace_oids = parse_and_validate_workspace_ids(&state, &body.workspace_ids).await?;

    let ai_user_id = ensure_ai_user_for_agent(&state, &label, &current_user.id).await?;
    let now = BsonDateTime::now();
    let mut agent = default_agent(label, description, ai_user_id, now);
    agent.workspace_ids = workspace_oids;

    if let Some(v) = body.is_receptionist { agent.is_receptionist = v; }
    if let Some(v) = body.enabled { agent.enabled = v; }
    if let Some(v) = body.mode { agent.mode = v; }
    apply_schedule(&mut agent.schedule, body.schedule);
    apply_model(&mut agent.model, body.model)?;
    apply_personality(&mut agent.personality, body.personality);
    if let Some(sp) = body.system_prompt { agent.system_prompt = sp; }
    if let Some(tools) = body.tools {
        agent.tools = tools
            .into_iter()
            .map(|t| AiToolConfig {
                name: t.name,
                enabled: t.enabled,
                description_override: t.description_override,
            })
            .collect();
    }
    apply_escalation(&mut agent.escalation, body.escalation);
    apply_limits(&mut agent.limits, body.limits);

    let saved = state
        .db
        .create_ai_agent(agent)
        .await
        .map_err(ApiError::DatabaseError)?;

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

    let api_key_rotated = body
        .model
        .as_ref()
        .and_then(|m| m.api_key.as_deref())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    let mut agent = state
        .db
        .find_ai_agent_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(agent_not_found)?;

    if let Some(v) = body.label { agent.label = v.trim().to_string(); }
    if let Some(v) = body.description { agent.description = v.trim().to_string(); }
    if let Some(v) = body.is_receptionist { agent.is_receptionist = v; }
    if let Some(v) = new_workspace_oids { agent.workspace_ids = v; }
    if let Some(v) = body.enabled { agent.enabled = v; }
    if let Some(v) = body.mode { agent.mode = v; }
    apply_schedule(&mut agent.schedule, body.schedule);
    apply_model(&mut agent.model, body.model)?;
    apply_personality(&mut agent.personality, body.personality);
    if let Some(sp) = body.system_prompt { agent.system_prompt = sp; }
    if let Some(tools) = body.tools {
        agent.tools = tools
            .into_iter()
            .map(|t| AiToolConfig {
                name: t.name,
                enabled: t.enabled,
                description_override: t.description_override,
            })
            .collect();
    }
    apply_escalation(&mut agent.escalation, body.escalation);
    apply_limits(&mut agent.limits, body.limits);
    agent.updated_at = BsonDateTime::now();

    let saved = state
        .db
        .replace_ai_agent(&oid, agent)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(agent_not_found)?;

    if api_key_rotated {
        state.redis.invalidate_ai_models_cache(&oid.to_hex()).await;
    }

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

// ============================================
// Apply helpers (merge campo a campo)
// ============================================

fn apply_schedule(cur: &mut AiSchedule, patch: Option<crate::models::ai_agent::AiScheduleInput>) {
    let Some(p) = patch else { return };
    if let Some(v) = p.timezone { cur.timezone = v; }
    if let Some(v) = p.always_on { cur.always_on = v; }
    if let Some(v) = p.weekdays { cur.weekdays = v; }
    if let Some(v) = p.from_hour { cur.from_hour = v; }
    if let Some(v) = p.to_hour { cur.to_hour = v; }
}

fn apply_model(
    cur: &mut AiModelConfig,
    patch: Option<crate::models::ai_agent::AiModelConfigInput>,
) -> Result<(), ApiError> {
    let Some(p) = patch else { return Ok(()) };
    if let Some(v) = p.provider { cur.provider = v; }
    if let Some(v) = p.model_id { cur.model_id = v; }
    if let Some(v) = p.temperature { cur.temperature = v; }
    if let Some(v) = p.max_tokens { cur.max_tokens = v; }
    if let Some(v) = p.timeout_seconds { cur.timeout_seconds = v; }
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
    if let Some(v) = p.assistant_name { cur.assistant_name = v; }
    if let Some(v) = p.locale { cur.locale = v; }
    if let Some(v) = p.tone { cur.tone = v; }
    if let Some(v) = p.greeting { cur.greeting = v; }
    if let Some(v) = p.farewell { cur.farewell = v; }
    if let Some(v) = p.forbidden_phrases { cur.forbidden_phrases = v; }
}

fn apply_escalation(
    cur: &mut AiEscalationRules,
    patch: Option<crate::models::ai_agent::AiEscalationRulesInput>,
) {
    let Some(p) = patch else { return };
    if let Some(v) = p.keywords { cur.keywords = v; }
    if let Some(v) = p.max_turns_without_resolution { cur.max_turns_without_resolution = v; }
    if let Some(v) = p.max_identification_attempts { cur.max_identification_attempts = v; }
    if let Some(v) = p.escalate_on_critical_tool_failure {
        cur.escalate_on_critical_tool_failure = v;
    }
    if let Some(v) = p.always_escalate_when_asked { cur.always_escalate_when_asked = v; }
    if p.default_ticket_category_id.is_some() {
        cur.default_ticket_category_id = p.default_ticket_category_id;
    }
}

fn apply_limits(cur: &mut AiLimits, patch: Option<crate::models::ai_agent::AiLimitsInput>) {
    let Some(p) = patch else { return };
    if let Some(v) = p.max_turns_per_day { cur.max_turns_per_day = v; }
    if let Some(v) = p.max_turns_per_conversation { cur.max_turns_per_conversation = v; }
    if let Some(v) = p.max_tokens_per_day { cur.max_tokens_per_day = v; }
    if let Some(v) = p.cost_alert_threshold_pct { cur.cost_alert_threshold_pct = v; }
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
        (status = 422, description = "Falta api_key"),
        (status = 502, description = "ai_auth_failed / ai_model_not_found / ai_upstream_unreachable / ai_rate_limited"),
    )
)]
pub async fn test_connection_raw_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<TestConnectionRequest>,
) -> Result<Json<TestConnectionResponse>, ApiError> {
    require_superadmin(&current_user)?;

    let api_key = body
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::ValidationError {
            code: "missing_field".into(),
            field: "api_key".into(),
            message: "`api_key` es requerido".into(),
        })?
        .to_string();

    let model_id = body
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_TEST_MODEL)
        .to_string();
    let timeout = body
        .timeout_seconds
        .map(|n| n.clamp(1, TEST_TIMEOUT_MAX))
        .unwrap_or(10);

    let relay = AiRelay::from_config(&state.config);
    super::gemini::test_connection(
        &state.reqwest_client,
        &api_key,
        &model_id,
        timeout,
        relay.as_ref(),
    )
    .await?;

    Ok(Json(TestConnectionResponse {
        ok: true,
        data: TestConnectionData {
            reachable: true,
            model_id,
            source: TestConnectionSource::Body,
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

    let body_key = body.api_key.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (api_key, source) = match body_key {
        Some(k) => (k.to_string(), TestConnectionSource::Body),
        None => (
            decrypt_api_key(&agent, &ai_agent_secret())?,
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
    super::gemini::test_connection(
        &state.reqwest_client,
        &api_key,
        &model_id,
        timeout,
        relay.as_ref(),
    )
    .await?;

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
// LIST MODELS — raw (pre-creación)
// ============================================

#[derive(Debug, Deserialize)]
pub struct ListModelsRawQuery {
    pub api_key: String,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/models",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("api_key" = String, Query, description = "API key de Gemini para preview")),
    responses(
        (status = 200, description = "Modelos disponibles", body = AiAgentModelsListResponse),
        (status = 400, description = "missing_api_key"),
        (status = 401, description = "invalid_api_key"),
        (status = 429, description = "gemini_rate_limited"),
        (status = 502, description = "gemini_unreachable"),
    )
)]
pub async fn list_models_raw_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Query(q): Query<ListModelsRawQuery>,
) -> Result<Json<AiAgentModelsListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let api_key = q.api_key.trim().to_string();
    if api_key.is_empty() {
        return Err(missing_api_key());
    }

    // Sin :id no hay clave estable para cache. Cada call pega a Gemini.
    let relay = AiRelay::from_config(&state.config);
    let raw = super::gemini::list_models(
        &state.reqwest_client,
        &api_key,
        MODELS_FETCH_TIMEOUT,
        relay.as_ref(),
    )
    .await?;
    Ok(Json(AiAgentModelsListResponse {
        ok: true,
        data: filter_and_map_models(raw),
    }))
}

// ============================================
// LIST MODELS por agente
// ============================================

#[derive(Debug, Deserialize)]
pub struct ListModelsForAgentQuery {
    /// Override de api_key. Si no viene, se usa la guardada del agente.
    #[serde(default)]
    pub api_key: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/agents/{id}/models",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "ObjectId hex del agente"),
        ("api_key" = Option<String>, Query, description = "Override de api_key"),
    ),
    responses(
        (status = 200, description = "Modelos disponibles", body = AiAgentModelsListResponse),
        (status = 400, description = "missing_api_key"),
        (status = 401, description = "invalid_api_key"),
        (status = 404, description = "agent_not_found"),
        (status = 429, description = "gemini_rate_limited"),
        (status = 502, description = "gemini_unreachable"),
    )
)]
pub async fn list_models_for_agent_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Query(q): Query<ListModelsForAgentQuery>,
) -> Result<Json<AiAgentModelsListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let agent = state
        .db
        .find_ai_agent_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(agent_not_found)?;

    let query_key = q.api_key.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let api_key = match query_key {
        Some(k) => k.to_string(),
        None => {
            if agent.model.api_key_encrypted.is_empty() {
                return Err(missing_api_key());
            }
            decrypt_api_key(&agent, &ai_agent_secret())?
        }
    };

    let agent_hex = oid.to_hex();
    if let Some(cached) = state.redis.get_ai_models_cache(&agent_hex, &api_key).await {
        if let Ok(items) = serde_json::from_str::<Vec<AiAgentModelItem>>(&cached) {
            return Ok(Json(AiAgentModelsListResponse { ok: true, data: items }));
        }
    }

    let relay = AiRelay::from_config(&state.config);
    let raw = super::gemini::list_models(
        &state.reqwest_client,
        &api_key,
        MODELS_FETCH_TIMEOUT,
        relay.as_ref(),
    )
    .await?;
    let items = filter_and_map_models(raw);
    if let Ok(json) = serde_json::to_string(&items) {
        state
            .redis
            .set_ai_models_cache(&agent_hex, &api_key, &json, MODELS_CACHE_TTL_SECS)
            .await;
    }
    Ok(Json(AiAgentModelsListResponse {
        ok: true,
        data: items,
    }))
}

fn filter_and_map_models(
    raw: Vec<super::gemini::GeminiModelEntry>,
) -> Vec<AiAgentModelItem> {
    raw.into_iter()
        .filter_map(|m| {
            let id = m.name.strip_prefix("models/").unwrap_or(&m.name);
            if !id.starts_with("gemini-") {
                return None;
            }
            if !m
                .supported_generation_methods
                .iter()
                .any(|s| s == "generateContent")
            {
                return None;
            }
            let supports = true;
            let recommended = RECOMMENDED_MODEL_IDS.iter().any(|r| *r == id);
            Some(AiAgentModelItem {
                id: id.to_string(),
                display_name: m.display_name.unwrap_or_default(),
                description: m.description.unwrap_or_default(),
                input_token_limit: m.input_token_limit.unwrap_or(0),
                output_token_limit: m.output_token_limit.unwrap_or(0),
                supports_function_calling: supports,
                supports_system_instruction: supports,
                version: m.version.unwrap_or_default(),
                recommended,
            })
        })
        .collect()
}
