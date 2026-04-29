//! Modelos del módulo AI Agent — modelo agent-centric.
//!
//! Cada `AiAgent` es una identidad/personalidad completa de IA con su propia
//! `api_key`, `model`, `system_prompt`, `tools` y `limits`. Un agente puede
//! atender 0+ workspaces (`workspace_ids[]`). La `description` es lo que la
//! recepcionista (paso 2) usará para decidir a quién derivar.
//!
//! Colecciones MongoDB:
//! - `AiAgents` — un doc por agente.
//! - `AiAgentFaqs` — knowledge base por **agente** (no por workspace).
//! - `AiInteractions` — log granular de turnos IA (PR 3).

use mongodb::bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================
// AiAgent — doc principal
// ============================================

/// Documento de la colección `AiAgents`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiAgent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Nombre corto que el SUPERADMIN ve en el listado ("Soporte", "Pagos",
    /// "Recepcionista").
    pub label: String,
    /// Para qué sirve este agente. La recepcionista lo va a usar para decidir
    /// el routing (paso 2). En PR actual sólo es informativo para el SUPERADMIN.
    pub description: String,
    /// Reservado para paso 2. Default `false`. No impone validaciones todavía.
    #[serde(default)]
    pub is_receptionist: bool,
    /// Workspaces (números de WhatsApp) donde este agente atiende. Vacío =
    /// agente "huérfano" sin atender todavía.
    #[serde(default)]
    pub workspace_ids: Vec<ObjectId>,
    /// Switch global del agente. `false` desactiva sin borrar.
    pub enabled: bool,
    /// `shadow` registra interacciones pero no envía al cliente. `live` envía.
    pub mode: AiAgentMode,
    /// UUID del `User` sintético atado a este agente. Se crea idempotente la
    /// primera vez que se persiste el agente. Atribución pura — no se le
    /// emite JWT.
    pub ai_user_id: String,
    pub schedule: AiSchedule,
    pub model: AiModelConfig,
    pub personality: AiPersonality,
    pub system_prompt: String,
    pub tools: Vec<AiToolConfig>,
    pub escalation: AiEscalationRules,
    pub limits: AiLimits,
    /// Segundos a esperar desde el último inbound antes de procesar la ráfaga
    /// (debounce). Si el cliente manda 4 mensajes en sucesión rápida, el bot
    /// espera `debounce_seconds` desde el último para responder UNA vez con
    /// todo el contexto. Default 10. 0 = procesar inmediato (no recomendado).
    #[serde(default = "default_debounce_seconds")]
    pub debounce_seconds: u32,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

fn default_debounce_seconds() -> u32 {
    10
}

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

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct AiSchedule {
    pub timezone: String,
    pub always_on: bool,
    pub weekdays: Vec<u8>,
    pub from_hour: u8,
    pub to_hour: u8,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiModelConfig {
    pub provider: String,
    pub model_id: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_seconds: u32,
    /// Ciphertext AES-GCM (Base64URL) de la `api_key`. Nunca se devuelve al
    /// front; el response usa `api_key_set: bool`.
    #[serde(default)]
    pub api_key_encrypted: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiPersonality {
    pub assistant_name: String,
    pub locale: String,
    pub tone: String,
    pub greeting: String,
    pub farewell: String,
    /// Despedida específica cuando la IA deriva la conv a un humano (limit
    /// reached, keyword match, etc). El back lo manda al cliente justo antes
    /// de pausar la IA. Si está vacío, se usa un fallback genérico.
    #[serde(default)]
    pub farewell_to_human: String,
    pub forbidden_phrases: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiToolConfig {
    pub name: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_override: Option<String>,
    /// Config opaca por tool (shape distinto según el `name`). Para
    /// `transfer_to_agent`: `{ "allowed_targets": ["<oid_hex>", ...] }`. Para
    /// `request_human`: hoy no se usa (toggle puro). El validador del back
    /// chequea el shape sólo cuando importa para esa tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiEscalationRules {
    pub keywords: Vec<String>,
    pub max_turns_without_resolution: u32,
    pub max_identification_attempts: u32,
    pub escalate_on_critical_tool_failure: bool,
    pub always_escalate_when_asked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ticket_category_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiLimits {
    pub max_turns_per_day: u32,
    pub max_turns_per_conversation: u32,
    pub max_tokens_per_day: u64,
    pub cost_alert_threshold_pct: u8,
}

impl AiLimits {
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
// AiAgentFaq — knowledge base por **agente**
// ============================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiAgentFaq {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub agent_id: ObjectId,
    pub question: String,
    pub answer: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

// ============================================
// Tool I/O shapes
// ============================================

#[derive(Debug, Serialize, Clone)]
pub struct AiClientLookup {
    pub client_id: String,
    pub name: Option<String>,
    pub identification: Option<String>,
    pub phone: String,
    pub status: String,
    pub balance: f64,
}

#[derive(Debug, Serialize, Clone)]
pub struct AiInvoice {
    pub id: String,
    pub amount: f64,
    pub reason: String,
    pub state: String,
    pub due_date: String,
}

// ============================================
// AiInteraction (PR 3 lo persiste)
// ============================================

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiInteraction {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub conversation_id: ObjectId,
    pub message_id: ObjectId,
    /// Workspace donde corrió el turno (necesario para auditar el inbound).
    pub workspace_id: ObjectId,
    /// Agente que corrió el turno. PR 3 lo va a usar para métricas por agente.
    pub agent_id: ObjectId,
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
// AiPlan — datos de plan que la tool `list_plans` expone
// ============================================

/// Documento de la colección `AiPlans`.
///
/// Expuesto sólo en español (lo que la IA va a leer literal). NO incluye
/// precio: la página pública dice "consultar" y el equipo comercial cierra el
/// monto al instalar — la IA nunca debe inventarlo.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiPlan {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub name: String,
    pub mbps: u32,
    pub devices_recommendation: String,
    #[serde(default)]
    pub benefits: Vec<String>,
    /// `false` lo oculta de `list_plans` sin borrar el doc.
    pub active: bool,
    /// Orden ascendente para `list_plans`. Default 0 — los nuevos van al final.
    #[serde(default)]
    pub display_order: i32,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiPlanItem {
    pub id: String,
    pub name: String,
    pub mbps: u32,
    pub devices_recommendation: String,
    pub benefits: Vec<String>,
    pub active: bool,
    pub display_order: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiPlanResponse {
    pub ok: bool,
    pub data: AiPlanItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiPlansListResponse {
    pub ok: bool,
    pub data: Vec<AiPlanItem>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAiPlanRequest {
    pub name: String,
    pub mbps: u32,
    pub devices_recommendation: String,
    #[serde(default)]
    pub benefits: Vec<String>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub display_order: Option<i32>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct UpdateAiPlanRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devices_recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benefits: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_order: Option<i32>,
}

// ============================================
// AiCoverageZone — la tool `check_coverage` matchea contra esto
// ============================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiCoverageZone {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Display name canónico (ej: "Valencia", "San Diego"). El matching
    /// normaliza tildes/case en runtime.
    pub name: String,
    /// Estado/región para contexto (ej: "Carabobo"). No participa del match.
    pub region: String,
    pub active: bool,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiCoverageZoneItem {
    pub id: String,
    pub name: String,
    pub region: String,
    pub active: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiCoverageZoneResponse {
    pub ok: bool,
    pub data: AiCoverageZoneItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiCoverageZonesListResponse {
    pub ok: bool,
    pub data: Vec<AiCoverageZoneItem>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAiCoverageZoneRequest {
    pub name: String,
    pub region: String,
    #[serde(default)]
    pub active: Option<bool>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct UpdateAiCoverageZoneRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiBusinessDataDeleteResponse {
    pub ok: bool,
}

// ============================================
// API DTOs (response)
// ============================================

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentItem {
    pub id: String,
    pub label: String,
    pub description: String,
    pub is_receptionist: bool,
    pub workspace_ids: Vec<String>,
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
    pub debounce_seconds: u32,
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
    pub api_key_set: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiPersonalityDto {
    pub assistant_name: String,
    pub locale: String,
    pub tone: String,
    pub greeting: String,
    pub farewell: String,
    pub farewell_to_human: String,
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
            farewell_to_human: p.farewell_to_human,
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
    /// Config por-tool tipada por el front según `name`. Passthrough opaco.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub config: Option<serde_json::Value>,
}

impl From<AiToolConfig> for AiToolConfigDto {
    fn from(t: AiToolConfig) -> Self {
        AiToolConfigDto {
            name: t.name,
            enabled: t.enabled,
            description_override: t.description_override,
            config: t.config,
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

/// Body de `POST /ai-agent/agents`. `label` y `description` son requeridos;
/// el resto cae a defaults.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAiAgentRequest {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_receptionist: Option<bool>,
    /// ObjectId hex de cada workspace donde el agente atiende. Puede estar
    /// vacío al crear; cada id se valida contra `WaSettings`.
    #[serde(default)]
    pub workspace_ids: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_seconds: Option<u32>,
}

/// Body de `PATCH /ai-agent/agents/:id`. Todo opcional; merge campo a campo
/// dentro de cada bloque (igual que el PATCH de settings viejo).
#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct UpdateAiAgentRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_receptionist: Option<bool>,
    /// Reemplaza la lista entera cuando viene. Cada id se valida.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_ids: Option<Vec<String>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_seconds: Option<u32>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct AiScheduleInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_on: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekdays: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hour: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_hour: Option<u8>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct AiModelConfigInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
    /// `Some(non-empty)` cifra y guarda. `None` o `Some("")` no toca la
    /// guardada.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct AiPersonalityInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub greeting: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub farewell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub farewell_to_human: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbidden_phrases: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiToolConfigInput {
    pub name: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct AiEscalationRulesInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_without_resolution: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_identification_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate_on_critical_tool_failure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_escalate_when_asked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ticket_category_id: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct AiLimitsInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_per_day: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_per_conversation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_per_day: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_alert_threshold_pct: Option<u8>,
}

// ─── Response envelopes ─────────────────────────────────────────────────────

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentResponse {
    pub ok: bool,
    pub data: AiAgentItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentsListResponse {
    pub ok: bool,
    pub data: Vec<AiAgentItem>,
}

// ─── FAQ DTOs ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentFaqItem {
    pub id: String,
    pub agent_id: String,
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

// ─── Test connection ────────────────────────────────────────────────────────

/// Body de `POST /ai-agent/test-connection` (sin :id, raw) y de
/// `POST /ai-agent/agents/:id/test-connection` (con :id, override opcional).
#[derive(Debug, Deserialize, ToSchema)]
pub struct TestConnectionRequest {
    /// Override de api_key (raw). En el endpoint sin `:id` es requerido.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Override de model_id. En el endpoint sin `:id` es requerido.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TestConnectionData {
    pub reachable: bool,
    pub model_id: String,
    pub source: TestConnectionSource,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TestConnectionSource {
    /// `api_key` vino en el body.
    Body,
    /// Se descifró desde `AiAgent.model.api_key_encrypted`.
    Stored,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TestConnectionResponse {
    pub ok: bool,
    pub data: TestConnectionData,
}

// ─── List models ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct AiAgentModelItem {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub input_token_limit: u32,
    pub output_token_limit: u32,
    pub supports_function_calling: bool,
    pub supports_system_instruction: bool,
    pub version: String,
    pub recommended: bool,
    /// `true` cuando el modelo está disponible en el plan free de Google AI
    /// Studio. Determinado por whitelist hardcoded en el back. Si Google
    /// cambia los tiers, hay que actualizar la lista.
    pub free_tier: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentModelsListResponse {
    pub ok: bool,
    pub data: Vec<AiAgentModelItem>,
}
