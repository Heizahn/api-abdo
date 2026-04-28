//! Handlers HTTP del módulo AI Agent.
//!
//! Endpoints (todos SUPERADMIN-only):
//! - GET    /v1/auth-user/whatsapp/ai-agent/settings
//! - GET    /v1/auth-user/whatsapp/ai-agent/settings/:workspace_id
//! - PATCH  /v1/auth-user/whatsapp/ai-agent/settings/:workspace_id   (upsert)
//! - GET    /v1/auth-user/whatsapp/ai-agent/faqs/:workspace_id
//! - POST   /v1/auth-user/whatsapp/ai-agent/faqs/:workspace_id
//! - PATCH  /v1/auth-user/whatsapp/ai-agent/faqs/item/:id
//! - DELETE /v1/auth-user/whatsapp/ai-agent/faqs/item/:id
//!
//! La `api_key` viene en el patch como string en claro y se cifra con AES-GCM
//! reusando `JWT_SECRET` (mismo patrón que `WaSettings.access_token_cipher`).

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
            AiAgentDeleteResponse, AiAgentFaq, AiAgentFaqItem, AiAgentFaqListResponse,
            AiAgentFaqResponse, AiAgentMode, AiAgentModelItem, AiAgentModelsListResponse,
            AiAgentSetting, AiAgentSettingItem, AiAgentSettingResponse,
            AiAgentSettingsListResponse, AiEscalationRules, AiLimits, AiModelConfig,
            AiPersonality, AiSchedule, AiToolConfig, CreateAiAgentFaqRequest,
            TestConnectionData, TestConnectionRequest, TestConnectionResponse,
            TestConnectionSource, UpdateAiAgentFaqRequest, UpdateAiAgentSettingsRequest,
        },
        users::User,
    },
    state::AppState,
};

use super::{gemini::AiRelay, runner::decrypt_api_key};

const SUPERADMIN_ROLE: f32 = 0.0;
/// Valor sentinel para `nRole` del bot. Ver plan v1.4 §1.2: 99 deja libres
/// 6/7/8 para futuros roles humanos y señala visualmente que es no-humano.
const AI_AGENT_ROLE: f32 = 99.0;

const PROMPT_MAX_LEN: usize = 16_000;
const FAQ_QUESTION_MAX_LEN: usize = 500;
const FAQ_ANSWER_MAX_LEN: usize = 4_000;
const FAQ_TAG_MAX_LEN: usize = 64;
const FAQ_TAGS_MAX_COUNT: usize = 16;

fn require_superadmin(current_user: &User) -> Result<(), ApiError> {
    if current_user.role != SUPERADMIN_ROLE {
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
    d.try_to_rfc3339_string()
        .unwrap_or_else(|_| String::new())
}

/// Reusa el secret del JWT para AES-GCM. Mismo patrón que el módulo WhatsApp.
fn ai_agent_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

// ============================================
// Defaults
// ============================================

fn default_setting(
    workspace_id: ObjectId,
    ai_user_id: String,
    now: BsonDateTime,
) -> AiAgentSetting {
    AiAgentSetting {
        id: None,
        workspace_id,
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
            model_id: "gemini-1.5-flash".into(),
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

fn setting_to_item(s: AiAgentSetting) -> AiAgentSettingItem {
    let api_key_set = !s.model.api_key_encrypted.is_empty();
    AiAgentSettingItem {
        id: s.id.map(|o| o.to_hex()).unwrap_or_default(),
        workspace_id: s.workspace_id.to_hex(),
        enabled: s.enabled,
        mode: s.mode,
        ai_user_id: s.ai_user_id,
        schedule: s.schedule.into(),
        model: crate::models::ai_agent::AiModelConfigDto {
            provider: s.model.provider,
            model_id: s.model.model_id,
            temperature: s.model.temperature,
            max_tokens: s.model.max_tokens,
            timeout_seconds: s.model.timeout_seconds,
            api_key_set,
        },
        personality: s.personality.into(),
        system_prompt: s.system_prompt,
        tools: s.tools.into_iter().map(Into::into).collect(),
        escalation: s.escalation.into(),
        limits: s.limits.into(),
        created_at: iso8601(s.created_at),
        updated_at: iso8601(s.updated_at),
    }
}

fn faq_to_item(f: AiAgentFaq) -> AiAgentFaqItem {
    AiAgentFaqItem {
        id: f.id.map(|o| o.to_hex()).unwrap_or_default(),
        workspace_id: f.workspace_id.to_hex(),
        question: f.question,
        answer: f.answer,
        tags: f.tags,
        created_at: iso8601(f.created_at),
        updated_at: iso8601(f.updated_at),
    }
}

// ============================================
// AI user sintético (creación idempotente)
// ============================================

/// Crea el `User` bot para un workspace si no existe ya. Devuelve el UUID.
///
/// El bot:
/// - `nRole = 99` (sentinel no-humano).
/// - `bIsBot = true` — flag explícito.
/// - `bCanChat = false`, `visible = false` — fuera de pickers humanos.
/// - Sin `password_hash` ni emisión de JWT — atribución pura.
/// - `email` sintético derivado de `workspace_id` para garantizar unicidad.
async fn ensure_ai_user(
    state: &Arc<AppState>,
    workspace_id: &ObjectId,
    creator_id: &str,
) -> Result<String, ApiError> {
    let email = format!("ai-agent-{}@internal", workspace_id.to_hex());

    if let Some(existing) = state
        .db
        .find_user_by_email(&email)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        return Ok(existing.id);
    }

    let now = mongodb::bson::DateTime::now();
    let user = User {
        id: uuid::Uuid::new_v4().to_string(),
        name: "Asistente Virtual".into(),
        role: AI_AGENT_ROLE,
        email,
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
    // Sin `create_user_credentials` — el bot no se loguea.
    Ok(user.id)
}

// ============================================
// Validación
// ============================================

fn workspace_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "workspace_not_found",
        "El workspace de WhatsApp no existe",
    )
}

fn setting_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "ai_agent_setting_not_found",
        "Configuración de Asistente Virtual no encontrada",
    )
}

fn faq_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "ai_agent_faq_not_found",
        "FAQ no encontrada",
    )
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

// ============================================
// SETTINGS handlers
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/settings",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Listado de configuraciones IA por workspace", body = AiAgentSettingsListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
    )
)]
pub async fn list_ai_agent_settings_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
) -> Result<Json<AiAgentSettingsListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let items = state
        .db
        .list_ai_agent_settings()
        .await
        .map_err(ApiError::DatabaseError)?;
    Ok(Json(AiAgentSettingsListResponse {
        ok: true,
        data: items.into_iter().map(setting_to_item).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/settings/{workspace_id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("workspace_id" = String, Path, description = "ObjectId hex del WaSettings")),
    responses(
        (status = 200, description = "Configuración IA del workspace", body = AiAgentSettingResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 404, description = "workspace_not_found / ai_agent_setting_not_found"),
    )
)]
pub async fn get_ai_agent_settings_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(workspace_id): Path<String>,
) -> Result<Json<AiAgentSettingResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&workspace_id, "workspace_id")?;

    // El workspace debe existir antes de buscar settings.
    if state
        .db
        .find_wa_settings_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .is_none()
    {
        return Err(workspace_not_found());
    }

    let setting = state
        .db
        .find_ai_agent_setting_by_workspace(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(setting_not_found)?;

    Ok(Json(AiAgentSettingResponse {
        ok: true,
        data: setting_to_item(setting),
    }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/ai-agent/settings/{workspace_id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("workspace_id" = String, Path, description = "ObjectId hex del WaSettings")),
    request_body = UpdateAiAgentSettingsRequest,
    responses(
        (status = 200, description = "Configuración actualizada (upsert)", body = AiAgentSettingResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 404, description = "workspace_not_found"),
        (status = 422, description = "Validación: invalid_id / field_too_long"),
    )
)]
pub async fn update_ai_agent_settings_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(workspace_id): Path<String>,
    Json(payload): Json<UpdateAiAgentSettingsRequest>,
) -> Result<Json<AiAgentSettingResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&workspace_id, "workspace_id")?;

    // El workspace de WhatsApp tiene que existir — no permitimos config IA
    // huérfana (un AiAgentSetting sin WaSettings que lo respalde no sirve).
    if state
        .db
        .find_wa_settings_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .is_none()
    {
        return Err(workspace_not_found());
    }

    if let Some(p) = payload.system_prompt.as_deref() {
        validate_string_len(p, "system_prompt", PROMPT_MAX_LEN)?;
    }

    // ¿El patch trae una api_key nueva non-empty? Lo decidimos antes de
    // consumir `payload` en `apply_patch`. Lo usamos al final para invalidar
    // el cache de modelos (la key vieja ya no debería servir lookups).
    let api_key_rotated = payload
        .model
        .as_ref()
        .and_then(|m| m.api_key.as_deref())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    let now = BsonDateTime::now();
    let existing = state
        .db
        .find_ai_agent_setting_by_workspace(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;

    let saved = match existing {
        // ─── Upsert: insert ────────────────────────────────────────────────
        None => {
            let ai_user_id = ensure_ai_user(&state, &oid, &current_user.id).await?;
            let mut setting = default_setting(oid, ai_user_id, now);
            apply_patch(&mut setting, payload, now)?;
            state
                .db
                .create_ai_agent_setting(setting)
                .await
                .map_err(|e| match e.as_str() {
                    "workspace_id_already_exists" => ApiError::domain_simple(
                        StatusCode::CONFLICT,
                        "workspace_id_already_exists",
                        "Ya existe una configuración IA para este workspace",
                    ),
                    other => ApiError::DatabaseError(other.to_string()),
                })?
        }
        // ─── Upsert: update ────────────────────────────────────────────────
        Some(mut setting) => {
            apply_patch(&mut setting, payload, now)?;
            let id = setting
                .id
                .ok_or_else(|| ApiError::Internal("AiAgentSetting sin _id".into()))?;
            // Replace mantiene `_id` y `created_at` (apply_patch no tocó created_at).
            state
                .db
                .replace_ai_agent_setting(&id, setting)
                .await
                .map_err(ApiError::DatabaseError)?
                .ok_or_else(setting_not_found)?
        }
    };

    if api_key_rotated {
        // Best-effort: borra todas las entradas de cache de modelos del
        // workspace (independiente del hash de la key vieja).
        state.redis.invalidate_ai_models_cache(&oid.to_hex()).await;
    }

    Ok(Json(AiAgentSettingResponse {
        ok: true,
        data: setting_to_item(saved),
    }))
}

/// Aplica el patch sobre un setting existente. **Merge campo a campo** dentro
/// de cada bloque: el FE puede mandar `model: { api_key: "..." }` y el resto
/// del bloque se preserva con lo que ya estaba guardado. Misma lógica para
/// schedule/personality/escalation/limits.
///
/// Excepción: `tools` reemplaza el array completo cuando viene — el FE tiene
/// el listado entero, no necesita mergear por nombre.
fn apply_patch(
    setting: &mut AiAgentSetting,
    patch: UpdateAiAgentSettingsRequest,
    now: BsonDateTime,
) -> Result<(), ApiError> {
    if let Some(e) = patch.enabled {
        setting.enabled = e;
    }
    if let Some(m) = patch.mode {
        setting.mode = m;
    }
    if let Some(s) = patch.schedule {
        let cur = &mut setting.schedule;
        if let Some(v) = s.timezone { cur.timezone = v; }
        if let Some(v) = s.always_on { cur.always_on = v; }
        if let Some(v) = s.weekdays { cur.weekdays = v; }
        if let Some(v) = s.from_hour { cur.from_hour = v; }
        if let Some(v) = s.to_hour { cur.to_hour = v; }
    }
    if let Some(m) = patch.model {
        let cur = &mut setting.model;
        if let Some(v) = m.provider { cur.provider = v; }
        if let Some(v) = m.model_id { cur.model_id = v; }
        if let Some(v) = m.temperature { cur.temperature = v; }
        if let Some(v) = m.max_tokens { cur.max_tokens = v; }
        if let Some(v) = m.timeout_seconds { cur.timeout_seconds = v; }
        // api_key: sólo se cifra y guarda si vino non-empty; si vino "" o
        // None, conservamos la api_key_encrypted previa.
        if let Some(raw) = m.api_key.as_deref() {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                cur.api_key_encrypted = encrypt_payload(&ai_agent_secret(), trimmed);
            }
        }
    }
    if let Some(p) = patch.personality {
        let cur = &mut setting.personality;
        if let Some(v) = p.assistant_name { cur.assistant_name = v; }
        if let Some(v) = p.locale { cur.locale = v; }
        if let Some(v) = p.tone { cur.tone = v; }
        if let Some(v) = p.greeting { cur.greeting = v; }
        if let Some(v) = p.farewell { cur.farewell = v; }
        if let Some(v) = p.forbidden_phrases { cur.forbidden_phrases = v; }
    }
    if let Some(sp) = patch.system_prompt {
        setting.system_prompt = sp;
    }
    if let Some(tools) = patch.tools {
        // Reemplazo total — el FE manda la lista completa cuando edita tools.
        setting.tools = tools
            .into_iter()
            .map(|t| AiToolConfig {
                name: t.name,
                enabled: t.enabled,
                description_override: t.description_override,
            })
            .collect();
    }
    if let Some(e) = patch.escalation {
        let cur = &mut setting.escalation;
        if let Some(v) = e.keywords { cur.keywords = v; }
        if let Some(v) = e.max_turns_without_resolution { cur.max_turns_without_resolution = v; }
        if let Some(v) = e.max_identification_attempts { cur.max_identification_attempts = v; }
        if let Some(v) = e.escalate_on_critical_tool_failure { cur.escalate_on_critical_tool_failure = v; }
        if let Some(v) = e.always_escalate_when_asked { cur.always_escalate_when_asked = v; }
        // default_ticket_category_id: sólo override si vino. Para "limpiarlo"
        // habría que agregar tri-state — por ahora no se necesita (la UI
        // siempre exige una categoría default).
        if e.default_ticket_category_id.is_some() {
            cur.default_ticket_category_id = e.default_ticket_category_id;
        }
    }
    if let Some(l) = patch.limits {
        let cur = &mut setting.limits;
        if let Some(v) = l.max_turns_per_day { cur.max_turns_per_day = v; }
        if let Some(v) = l.max_turns_per_conversation { cur.max_turns_per_conversation = v; }
        if let Some(v) = l.max_tokens_per_day { cur.max_tokens_per_day = v; }
        if let Some(v) = l.cost_alert_threshold_pct { cur.cost_alert_threshold_pct = v; }
    }
    setting.updated_at = now;
    Ok(())
}

// ============================================
// FAQs handlers
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/faqs/{workspace_id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("workspace_id" = String, Path, description = "ObjectId hex del WaSettings")),
    responses(
        (status = 200, description = "FAQs del workspace", body = AiAgentFaqListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
    )
)]
pub async fn list_ai_agent_faqs_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(workspace_id): Path<String>,
) -> Result<Json<AiAgentFaqListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&workspace_id, "workspace_id")?;
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
    path = "/v1/auth-user/whatsapp/ai-agent/faqs/{workspace_id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("workspace_id" = String, Path, description = "ObjectId hex del WaSettings")),
    request_body = CreateAiAgentFaqRequest,
    responses(
        (status = 201, description = "FAQ creada", body = AiAgentFaqResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 404, description = "workspace_not_found"),
        (status = 422, description = "Validación: missing_field / field_too_long / too_many_tags"),
    )
)]
pub async fn create_ai_agent_faq_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(workspace_id): Path<String>,
    Json(body): Json<CreateAiAgentFaqRequest>,
) -> Result<(StatusCode, Json<AiAgentFaqResponse>), ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&workspace_id, "workspace_id")?;

    if state
        .db
        .find_wa_settings_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .is_none()
    {
        return Err(workspace_not_found());
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
        workspace_id: oid,
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
    path = "/v1/auth-user/whatsapp/ai-agent/faqs/item/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la FAQ")),
    request_body = UpdateAiAgentFaqRequest,
    responses(
        (status = 200, description = "FAQ actualizada", body = AiAgentFaqResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
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

// ============================================
// TEST CONNECTION handler
// ============================================

const DEFAULT_TEST_MODEL: &str = "gemini-1.5-flash";
const TEST_TIMEOUT_MAX: u32 = 30;

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/test-connection",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    request_body = TestConnectionRequest,
    responses(
        (status = 200, description = "Conexión OK", body = TestConnectionResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 422, description = "Falta api_key o workspace_id"),
        (status = 502, description = "ai_auth_failed / ai_model_not_found / ai_upstream_unreachable / ai_rate_limited"),
        (status = 503, description = "ai_api_key_missing"),
    )
)]
pub async fn test_connection_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<TestConnectionRequest>,
) -> Result<Json<TestConnectionResponse>, ApiError> {
    require_superadmin(&current_user)?;

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

    // Resolver api_key. Body gana si vino non-empty; si no, intentar
    // descifrar la guardada del workspace.
    let body_key = body.api_key.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (api_key, source) = if let Some(k) = body_key {
        (k.to_string(), TestConnectionSource::Body)
    } else if let Some(ws_raw) = body
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let ws_oid = parse_oid(ws_raw, "workspace_id")?;
        let setting = state
            .db
            .find_ai_agent_setting_by_workspace(&ws_oid)
            .await
            .map_err(ApiError::DatabaseError)?
            .ok_or_else(setting_not_found)?;
        let key = decrypt_api_key(&setting, &ai_agent_secret())?;
        (key, TestConnectionSource::Stored)
    } else {
        return Err(ApiError::ValidationError {
            code: "missing_field".into(),
            field: "api_key".into(),
            message: "Pasá `api_key` o `workspace_id` para probar la conexión".into(),
        });
    };

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
// LIST MODELS handler
// ============================================

/// Cache TTL del listado. La lista de modelos cambia con poca frecuencia
/// (~semanas), así que 10 min es conservador. El SUPERADMIN puede forzar
/// refresh rotando la api_key (invalida cache implícitamente).
const MODELS_CACHE_TTL_SECS: u64 = 600;
const MODELS_FETCH_TIMEOUT: u32 = 15;

#[derive(Debug, Deserialize)]
pub struct ListModelsQuery {
    /// Override de api_key — útil para ver qué modelos ofrece una key antes
    /// de guardarla. Si vino vacío o no vino, se usa la guardada.
    #[serde(default)]
    pub api_key: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/models/{workspace_id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(
        ("workspace_id" = String, Path, description = "ObjectId hex del WaSettings"),
        ("api_key" = Option<String>, Query, description = "Override de api_key (preview antes de guardar)"),
    ),
    responses(
        (status = 200, description = "Listado de modelos Gemini disponibles para la key", body = AiAgentModelsListResponse),
        (status = 400, description = "missing_api_key — no hay key guardada y no vino por query"),
        (status = 401, description = "invalid_api_key — Gemini rechazó la key"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
        (status = 404, description = "workspace_not_found"),
        (status = 429, description = "gemini_rate_limited"),
        (status = 502, description = "gemini_unreachable"),
    )
)]
pub async fn list_ai_agent_models_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(workspace_id): Path<String>,
    Query(q): Query<ListModelsQuery>,
) -> Result<Json<AiAgentModelsListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&workspace_id, "workspace_id")?;

    if state
        .db
        .find_wa_settings_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .is_none()
    {
        return Err(workspace_not_found());
    }

    // Resolver api_key: query > stored. Si nada → 400 missing_api_key.
    let query_key = q
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let api_key = if let Some(k) = query_key {
        k.to_string()
    } else {
        let setting = state
            .db
            .find_ai_agent_setting_by_workspace(&oid)
            .await
            .map_err(ApiError::DatabaseError)?
            .ok_or_else(missing_api_key)?;
        if setting.model.api_key_encrypted.is_empty() {
            return Err(missing_api_key());
        }
        decrypt_api_key(&setting, &ai_agent_secret())?
    };

    // Cache hit → devolver tal cual.
    let ws_hex = oid.to_hex();
    if let Some(cached) = state
        .redis
        .get_ai_models_cache(&ws_hex, &api_key)
        .await
    {
        if let Ok(items) = serde_json::from_str::<Vec<AiAgentModelItem>>(&cached) {
            return Ok(Json(AiAgentModelsListResponse {
                ok: true,
                data: items,
            }));
        }
        // Si no parsea, ignoramos el cache y refetcheamos.
    }

    let relay = AiRelay::from_config(&state.config);
    let raw_models = super::gemini::list_models(
        &state.reqwest_client,
        &api_key,
        MODELS_FETCH_TIMEOUT,
        relay.as_ref(),
    )
    .await?;

    let items = filter_and_map_models(raw_models);

    // Cachear (best-effort).
    if let Ok(json) = serde_json::to_string(&items) {
        state
            .redis
            .set_ai_models_cache(&ws_hex, &api_key, &json, MODELS_CACHE_TTL_SECS)
            .await;
    }

    Ok(Json(AiAgentModelsListResponse {
        ok: true,
        data: items,
    }))
}

/// Modelos sugeridos por default — se marcan con `recommended: true`.
const RECOMMENDED_MODEL_IDS: &[&str] = &[
    "gemini-1.5-flash-latest",
    "gemini-1.5-pro-latest",
];

fn filter_and_map_models(
    raw: Vec<super::gemini::GeminiModelEntry>,
) -> Vec<AiAgentModelItem> {
    raw.into_iter()
        .filter_map(|m| {
            // Filtrar por familia gemini-* y por soporte de generateContent.
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
            let methods = &m.supported_generation_methods;
            let supports_function_calling = methods.iter().any(|s| s == "generateContent");
            let supports_system_instruction = supports_function_calling;
            let recommended = RECOMMENDED_MODEL_IDS.iter().any(|r| *r == id);
            Some(AiAgentModelItem {
                id: id.to_string(),
                display_name: m.display_name.unwrap_or_default(),
                description: m.description.unwrap_or_default(),
                input_token_limit: m.input_token_limit.unwrap_or(0),
                output_token_limit: m.output_token_limit.unwrap_or(0),
                supports_function_calling,
                supports_system_instruction,
                version: m.version.unwrap_or_default(),
                recommended,
            })
        })
        .collect()
}

fn missing_api_key() -> ApiError {
    ApiError::domain_simple(
        StatusCode::BAD_REQUEST,
        "missing_api_key",
        "Pasá `api_key` por query o configurá la del workspace antes",
    )
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/ai-agent/faqs/item/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la FAQ")),
    responses(
        (status = 200, description = "FAQ eliminada", body = AiAgentDeleteResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere rol SUPERADMIN"),
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
