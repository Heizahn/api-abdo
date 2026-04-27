//! Modelos del módulo AI Agent (PR 1).
//!
//! Este módulo define la persistencia base del Asistente Virtual de WhatsApp:
//! - `AiAgentSetting` — configuración por workspace (un doc por `WaSettings`).
//! - `AiAgentFaq` — knowledge base inline editable (CRUD separado).
//! - `AiInteraction` — log granular de cada turno IA (se persiste cuando el
//!   loop esté activo en PR 2; el modelo se define ahora para fijar el shape).
//!
//! El plan v1.4 §4 detalla las decisiones: la `api_key` va cifrada con AES-GCM
//! reusando `JWT_SECRET` (mismo patrón que `WaSettings.access_token`); el
//! `system_prompt` no requiere cifrado.

use mongodb::bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================
// AiAgentSetting — config por workspace
// ============================================

/// Documento de la colección `AiAgentSettings`. Único por `workspace_id`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiAgentSetting {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// FK a `WaSettings._id`. Único — un workspace tiene un único setting IA.
    pub workspace_id: ObjectId,
    /// `true` para procesar inbounds; `false` desactiva el agente sin borrar la config.
    pub enabled: bool,
    /// Modo de operación. `shadow` registra `AiInteraction` pero no envía
    /// la respuesta al cliente. `live` envía. Default: `shadow`.
    pub mode: AiAgentMode,
    /// UUID del `User` sintético atado a este setting. Se crea idempotente
    /// la primera vez que se persiste el setting. Pensado solo para
    /// atribución (timeline, métricas) — no se le emite JWT.
    pub ai_user_id: String,
    pub schedule: AiSchedule,
    pub model: AiModelConfig,
    pub personality: AiPersonality,
    pub system_prompt: String,
    pub tools: Vec<AiToolConfig>,
    pub escalation: AiEscalationRules,
    pub limits: AiLimits,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

/// Modo del agente. `shadow` es el default seguro: la IA procesa pero no
/// envía respuestas reales al cliente — un humano sigue atendiendo.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AiAgentMode {
    Shadow,
    Live,
}

impl Default for AiAgentMode {
    fn default() -> Self {
        AiAgentMode::Shadow
    }
}

/// Horario en que el agente atiende. `always_on` corta cualquier cálculo de
/// ventana — el resto de los campos quedan informativos.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct AiSchedule {
    /// IANA TZ (ej: `America/Caracas`).
    pub timezone: String,
    pub always_on: bool,
    /// Días de la semana ISO (1=Mon..7=Sun).
    pub weekdays: Vec<u8>,
    /// 0..23
    pub from_hour: u8,
    /// 0..23 (puede ser menor que `from_hour` si el horario cruza medianoche).
    pub to_hour: u8,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiModelConfig {
    /// `gemini` por ahora. Reservado para multi-provider en fase 5.
    pub provider: String,
    /// `gemini-1.5-flash` por default.
    pub model_id: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_seconds: u32,
    /// Ciphertext AES-GCM (Base64URL) de la `api_key`. Nunca se devuelve al
    /// front; el response usa `api_key_set: bool` para indicar presencia.
    /// Patrón idéntico a `WaSettings.access_token`.
    #[serde(default)]
    pub api_key_encrypted: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiPersonality {
    pub assistant_name: String,
    /// `es-VE` por default — venezolano coloquial.
    pub locale: String,
    pub tone: String,
    pub greeting: String,
    pub farewell: String,
    pub forbidden_phrases: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiToolConfig {
    /// `lookup_customer`, `get_invoices`, `request_human`, `create_ticket` en PR 1.
    pub name: String,
    pub enabled: bool,
    /// Override opcional del description que se manda a Gemini. `None` ⇒ usar el
    /// default del back. Útil para SUPERADMIN sin redeploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_override: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiEscalationRules {
    pub keywords: Vec<String>,
    pub max_turns_without_resolution: u32,
    pub max_identification_attempts: u32,
    pub escalate_on_critical_tool_failure: bool,
    pub always_escalate_when_asked: bool,
    /// Categoría por default cuando la IA escala sin inferir una. Suele apuntar
    /// a `soporte_primer_segundo_nivel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ticket_category_id: Option<String>,
}

/// Topes operacionales obligatorios. Ver plan v1.4 §7.4.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiLimits {
    pub max_turns_per_day: u32,
    pub max_turns_per_conversation: u32,
    pub max_tokens_per_day: u64,
    /// 0..100 — aviso al SUPERADMIN al cruzar este % de cualquier cap.
    pub cost_alert_threshold_pct: u8,
}

impl AiLimits {
    /// Defaults sugeridos por el plan §7.4.
    pub fn defaults() -> Self {
        AiLimits {
            max_turns_per_day: 5_000,
            max_turns_per_conversation: 20,
            max_tokens_per_day: 5_000_000,
            cost_alert_threshold_pct: 80,
        }
    }
}

// ============================================
// Tool I/O shapes — usados por los tools del AI Agent (PR 2)
// ============================================

/// Resultado de `lookup_customer`. Una entrada por cliente match.
#[derive(Debug, Serialize, Clone)]
pub struct AiClientLookup {
    pub client_id: String,
    pub name: Option<String>,
    /// `sDni` con prefijo `V-` si existe; si no, `sRif` con prefijo correspondiente.
    pub identification: Option<String>,
    pub phone: String,
    pub status: String,
    pub balance: f64,
}

/// Resultado de `get_invoices`. Una entrada por deuda activa o reciente.
#[derive(Debug, Serialize, Clone)]
pub struct AiInvoice {
    pub id: String,
    pub amount: f64,
    pub reason: String,
    pub state: String,
    pub due_date: String,
}

// ============================================
// AiAgentFaq — knowledge base
// ============================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiAgentFaq {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub workspace_id: ObjectId,
    pub question: String,
    pub answer: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

// ============================================
// AiInteraction — log de turnos (PR 2, modelo definido ahora)
// ============================================

/// Cada turno IA persiste un `AiInteraction`. PR 1 deja el modelo declarado;
/// el persist real arranca cuando el loop entre por PR 2.
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiInteraction {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub conversation_id: ObjectId,
    pub message_id: ObjectId,
    pub workspace_id: ObjectId,
    pub turn_index: u32,
    pub model_id: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost_usd_estimate: f64,
    pub latency_ms: u32,
    pub tool_calls: Vec<AiToolCallLog>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_text: Option<String>,
    pub escalated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_reason: Option<String>,
    pub created_at: DateTime,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiToolCallLog {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub result_summary: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u32,
}

// ============================================
// API DTOs (request/response)
// ============================================

/// Shape devuelto por GET y POST/PATCH (sin `api_key_encrypted`).
#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentSettingItem {
    pub id: String,
    pub workspace_id: String,
    pub enabled: bool,
    pub mode: AiAgentMode,
    pub ai_user_id: String,
    pub schedule: AiScheduleDto,
    pub model: AiModelConfigDto,
    pub personality: AiPersonalityDto,
    pub system_prompt: String,
    pub tools: Vec<AiToolConfigDto>,
    pub escalation: AiEscalationRulesDto,
    pub limits: AiLimitsDto,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiScheduleDto {
    pub timezone: String,
    pub always_on: bool,
    pub weekdays: Vec<u8>,
    pub from_hour: u8,
    pub to_hour: u8,
}

impl From<AiSchedule> for AiScheduleDto {
    fn from(s: AiSchedule) -> Self {
        AiScheduleDto {
            timezone: s.timezone,
            always_on: s.always_on,
            weekdays: s.weekdays,
            from_hour: s.from_hour,
            to_hour: s.to_hour,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiModelConfigDto {
    pub provider: String,
    pub model_id: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_seconds: u32,
    /// `true` cuando hay api_key configurada (cifrada). Nunca se devuelve la
    /// key en claro.
    pub api_key_set: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiPersonalityDto {
    pub assistant_name: String,
    pub locale: String,
    pub tone: String,
    pub greeting: String,
    pub farewell: String,
    pub forbidden_phrases: Vec<String>,
}

impl From<AiPersonality> for AiPersonalityDto {
    fn from(p: AiPersonality) -> Self {
        AiPersonalityDto {
            assistant_name: p.assistant_name,
            locale: p.locale,
            tone: p.tone,
            greeting: p.greeting,
            farewell: p.farewell,
            forbidden_phrases: p.forbidden_phrases,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiToolConfigDto {
    pub name: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_override: Option<String>,
}

impl From<AiToolConfig> for AiToolConfigDto {
    fn from(t: AiToolConfig) -> Self {
        AiToolConfigDto {
            name: t.name,
            enabled: t.enabled,
            description_override: t.description_override,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiEscalationRulesDto {
    pub keywords: Vec<String>,
    pub max_turns_without_resolution: u32,
    pub max_identification_attempts: u32,
    pub escalate_on_critical_tool_failure: bool,
    pub always_escalate_when_asked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_ticket_category_id: Option<String>,
}

impl From<AiEscalationRules> for AiEscalationRulesDto {
    fn from(e: AiEscalationRules) -> Self {
        AiEscalationRulesDto {
            keywords: e.keywords,
            max_turns_without_resolution: e.max_turns_without_resolution,
            max_identification_attempts: e.max_identification_attempts,
            escalate_on_critical_tool_failure: e.escalate_on_critical_tool_failure,
            always_escalate_when_asked: e.always_escalate_when_asked,
            default_ticket_category_id: e.default_ticket_category_id,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiLimitsDto {
    pub max_turns_per_day: u32,
    pub max_turns_per_conversation: u32,
    pub max_tokens_per_day: u64,
    pub cost_alert_threshold_pct: u8,
}

impl From<AiLimits> for AiLimitsDto {
    fn from(l: AiLimits) -> Self {
        AiLimitsDto {
            max_turns_per_day: l.max_turns_per_day,
            max_turns_per_conversation: l.max_turns_per_conversation,
            max_tokens_per_day: l.max_tokens_per_day,
            cost_alert_threshold_pct: l.cost_alert_threshold_pct,
        }
    }
}

// ─── Requests ───────────────────────────────────────────────────────────────

/// Body de `PATCH /ai-agent/settings/:workspace_id`. Todo opcional —
/// upsert: si no existe el doc, se crea con defaults + lo que vino.
#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct UpdateAiAgentSettingsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AiAgentMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<AiScheduleInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<AiModelConfigInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<AiPersonalityInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AiToolConfigInput>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<AiEscalationRulesInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<AiLimitsInput>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiScheduleInput {
    pub timezone: String,
    pub always_on: bool,
    pub weekdays: Vec<u8>,
    pub from_hour: u8,
    pub to_hour: u8,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiModelConfigInput {
    pub provider: String,
    pub model_id: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_seconds: u32,
    /// Si viene en `Some(non-empty)` se cifra y se guarda. `None` o `Some("")`
    /// no tocan la api_key existente.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiPersonalityInput {
    pub assistant_name: String,
    pub locale: String,
    pub tone: String,
    pub greeting: String,
    pub farewell: String,
    #[serde(default)]
    pub forbidden_phrases: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiToolConfigInput {
    pub name: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_override: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiEscalationRulesInput {
    #[serde(default)]
    pub keywords: Vec<String>,
    pub max_turns_without_resolution: u32,
    pub max_identification_attempts: u32,
    pub escalate_on_critical_tool_failure: bool,
    pub always_escalate_when_asked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ticket_category_id: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiLimitsInput {
    pub max_turns_per_day: u32,
    pub max_turns_per_conversation: u32,
    pub max_tokens_per_day: u64,
    pub cost_alert_threshold_pct: u8,
}

// ─── Response envelopes ─────────────────────────────────────────────────────

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentSettingResponse {
    pub ok: bool,
    pub data: AiAgentSettingItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentSettingsListResponse {
    pub ok: bool,
    pub data: Vec<AiAgentSettingItem>,
}

// ─── FAQ DTOs ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentFaqItem {
    pub id: String,
    pub workspace_id: String,
    pub question: String,
    pub answer: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAiAgentFaqRequest {
    pub question: String,
    pub answer: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct UpdateAiAgentFaqRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentFaqResponse {
    pub ok: bool,
    pub data: AiAgentFaqItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentFaqListResponse {
    pub ok: bool,
    pub data: Vec<AiAgentFaqItem>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentDeleteResponse {
    pub ok: bool,
}
