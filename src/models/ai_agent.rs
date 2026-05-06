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
// AiConfig — singleton de configuración global
// ============================================

/// Documento único de la colección `AiConfig`.
/// App-level singleton: se usa `find_one({})` + upsert para garantizar
/// como máximo un documento.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// AES-GCM ciphertext (Base64URL) de la OpenRouter API key. String vacío = no configurada.
    #[serde(default)]
    pub openrouter_api_key: String,
    /// Slug del modelo default pre-rellenado al crear un nuevo AiAgent.
    #[serde(default)]
    pub default_model: String,
    pub updated_at: DateTime,
    /// UUID del staff user que persistió este doc por última vez.
    #[serde(default)]
    pub editor_id: String,
}

impl AiConfig {
    /// Convierte el documento a DTO de respuesta HTTP. Nunca expone ciphertext ni cleartext.
    pub fn to_dto(&self) -> AiConfigDto {
        AiConfigDto {
            has_api_key: !self.openrouter_api_key.is_empty(),
            default_model: self.default_model.clone(),
            updated_at: Some(
                self.updated_at
                    .try_to_rfc3339_string()
                    .unwrap_or_default(),
            ),
            editor_id: if self.editor_id.is_empty() {
                None
            } else {
                Some(self.editor_id.clone())
            },
        }
    }
}

/// DTO de respuesta para GET/PATCH /config. No contiene la clave en claro.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiConfigDto {
    /// `true` si hay una api_key configurada (no expone el valor).
    pub has_api_key: bool,
    /// Slug del modelo default para nuevos agentes. Vacío = sin configurar.
    pub default_model: String,
    /// ISO8601 de la última escritura. `null` cuando la colección está vacía.
    pub updated_at: Option<String>,
    /// UUID del editor. `null` cuando la colección está vacía.
    pub editor_id: Option<String>,
}

impl Default for AiConfigDto {
    /// Usado cuando la colección `AiConfig` está vacía.
    fn default() -> Self {
        Self {
            has_api_key: false,
            default_model: String::new(),
            updated_at: None,
            editor_id: None,
        }
    }
}

/// Body de `PATCH /v1/auth-user/whatsapp/ai-agent/config`. Ambos campos son opcionales
/// pero al menos uno debe estar presente.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AiConfigPatchRequest {
    /// Cleartext de la OpenRouter API key. El servidor la cifra antes de guardar.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Slug del modelo default (ej: `"openai/gpt-4o"`).
    #[serde(default)]
    pub default_model: Option<String>,
}

/// Envelope de respuesta para GET/PATCH /config.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiConfigResponse {
    pub ok: bool,
    pub data: AiConfigDto,
}

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
    /// Phase 3a. Propósito semántico del agente para enrutamiento por el
    /// pre-clasificador. `None` = agente legacy, siempre cae a Sofía.
    /// Set by SUPERADMIN to enable Clear* routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<AiAgentPurpose>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

fn default_debounce_seconds() -> u32 {
    10
}

/// Propósito semántico del agente (Phase 3a). Usado por el pre-clasificador
/// para enrutar mensajes directamente a un especialista sin pasar por Sofía.
///
/// Set by SUPERADMIN to enable Clear* routing; legacy agents (`None`) always
/// fall through to Sofía. Valores válidos: `recepcionista`, `ventas`,
/// `pagos`, `soporte`.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AiAgentPurpose {
    Recepcionista,
    Ventas,
    Pagos,
    Soporte,
}

/// Tipo de conexión disponible en una zona de cobertura.
/// Una zona puede soportar uno o ambos tipos.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ToSchema, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionType {
    Fibra,
    Antena,
}

impl ConnectionType {
    /// Slug stable usado en paths de URL y matching de la tool. Misma forma que el `Serialize`.
    pub fn as_slug(&self) -> &'static str {
        match self {
            ConnectionType::Fibra => "fibra",
            ConnectionType::Antena => "antena",
        }
    }

    /// Parsea desde slug. Case-insensitive. Retorna `None` si no matchea.
    pub fn from_slug(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "fibra" => Some(ConnectionType::Fibra),
            "antena" => Some(ConnectionType::Antena),
            _ => None,
        }
    }
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
    /// DEPRECATED 2026-05-06: era el discriminante del proveedor (`gemini` /
    /// `openrouter`) cuando había multi-provider. Hoy con OpenRouter como único
    /// runtime, el campo es informativo y NO se valida. Nuevos docs se siembran
    /// con `"openrouter"`. Ignorado en POST/PATCH del agente.
    #[serde(default)]
    pub provider: String,
    pub model_id: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_seconds: u32,
    /// DEPRECATED 2026-05-05: runtime ignores this field. SUPERADMIN sets the
    /// global key via PATCH /v1/auth-user/whatsapp/ai-agent/config. Field kept
    /// to avoid breaking existing documents.
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
    /// Number of initial AI turns where the `no_resolution_count` counter is bypassed.
    /// Recommended values:
    /// - 0: Receptionist/router agents (must classify and transfer fast)
    /// - 2-3: Payment/billing agents (structured flow)
    /// - 3-4: Technical support (initial diagnostic questions)
    /// - 4-5: Sales agents (qualification window: zone, usage, devices, etc.)
    /// Max: 10.
    #[serde(default)]
    pub qualification_window_turns: u32,
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
    /// Phase 3a. Tokens consumidos por razonamiento interno (thinking models).
    /// Separado de `output_tokens` para diagnosticar truncamiento.
    #[serde(default)]
    pub thinking_tokens: u32,
    /// Phase 3a. Tokens servidos desde el caché implícito/explícito de Gemini.
    /// Ausente en docs legacy → 0 via `#[serde(default)]`.
    #[serde(default)]
    pub cached_tokens: u32,
    /// Phase 3a. `true` cuando el turno pasó por el pre-clasificador.
    #[serde(default)]
    pub pre_classified: bool,
    /// Phase 3a. Resultado del pre-clasificador ("Spam", "GreetingOnly",
    /// "ClearVentas", "ClearPagos", "ClearSoporte", "Ambiguous"). `None` si
    /// el pre-clasificador no corrió en este turno.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_class_result: Option<String>,
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
    /// Precio del plan en USD. La IA convierte a Bs (BCV + IVA) al cotizar.
    /// Default 0 para docs legacy — el handler PATCH valida que se setee
    /// antes de exponerlo al cliente.
    #[serde(default)]
    pub price_usd: f64,
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
    pub price_usd: f64,
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
    /// Precio en USD. Required al crear: la IA cotiza con esto.
    pub price_usd: f64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_usd: Option<f64>,
}

// ============================================
// AiCoverageZone — la tool `check_coverage` matchea contra esto
// ============================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiCoverageZone {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Etiqueta canónica que el SUPERADMIN ve en la UI. Cliente-provista.
    pub display_name: String,
    /// Estado canónico VE — debe existir en ve_political_divisions.
    pub state: String,
    /// Municipio canónico VE — debe pertenecer al `state` indicado.
    pub municipality: String,
    /// Parroquia/barrio opcional. Texto libre.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parish: Option<String>,
    /// Sector/urbanización opcional. Texto libre.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sector: Option<String>,
    /// Aliases para tolerancia a typos. Máx 5, normalizados y deduplicados.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Tipos de conexión disponibles en esta zona. Una zona puede soportar
    /// uno o ambos. Default vacío en docs legacy — el migration script setea
    /// `["fibra"]` en docs existentes; el handler PATCH valida ≥ 1 elemento
    /// cuando el campo viene en el body.
    #[serde(default)]
    pub connection_types: Vec<ConnectionType>,
    pub is_active: bool,
    /// `true` para zonas migradas del esquema legacy que necesitan revisión
    /// del SUPERADMIN antes de activarse. Read-only en la API.
    pub needs_review: bool,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiCoverageZoneItem {
    pub id: String,
    pub display_name: String,
    pub state: String,
    pub municipality: String,
    pub parish: Option<String>,
    pub sector: Option<String>,
    pub aliases: Vec<String>,
    pub connection_types: Vec<ConnectionType>,
    pub is_active: bool,
    pub needs_review: bool,
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
    pub display_name: String,
    pub state: String,
    pub municipality: String,
    #[serde(default)]
    pub parish: Option<String>,
    #[serde(default)]
    pub sector: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Tipos de conexión soportados. Required: mínimo 1.
    pub connection_types: Vec<ConnectionType>,
    /// Default `false` — el admin opta explícitamente por activar.
    /// `needs_review` no se deserializa: es campo server-controlled.
    #[serde(default)]
    pub is_active: Option<bool>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct UpdateAiCoverageZoneRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub municipality: Option<String>,
    /// `Some(Some(v))` → setear; `Some(None)` → limpiar; `None` → no tocar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parish: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sector: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    /// Si viene presente, debe tener ≥ 1 elemento (validado en el handler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_types: Option<Vec<ConnectionType>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,
}

// ─── Political Divisions DTOs ────────────────────────────────────────────────

#[derive(Debug, Serialize, ToSchema)]
pub struct PoliticalDivisionItem {
    pub state: String,
    pub municipalities: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PoliticalDivisionsResponse {
    pub ok: bool,
    pub data: Vec<PoliticalDivisionItem>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiBusinessDataDeleteResponse {
    pub ok: bool,
}

// ============================================
// AiInstallationConfig — costos de instalación por tipo de conexión
// ============================================
//
// Colección `AiInstallationConfigs` — exactamente 2 docs (fibra y antena).
// La colección se siembra lazy al primer GET si está vacía.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiInstallationConfig {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Discriminante: `fibra` o `antena`. Único por colección.
    pub connection_type: ConnectionType,
    /// Costo base de la instalación en USD.
    pub base_cost_usd: f64,
    /// Texto libre con lo que incluye la instalación
    /// (ej: "Router Wi-Fi, 150mt cable, instalación").
    pub includes: String,
    /// Costo por metro extra de cable. `None` = no aplica para este tipo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excedente_per_meter_usd: Option<f64>,
    /// Texto libre con notas sobre el excedente
    /// (ej: "Sin tope. Asesor confirma metros en sitio.").
    #[serde(default)]
    pub excedente_notes: String,
    /// Notas adicionales (texto libre).
    #[serde(default)]
    pub notes: String,
    pub updated_at: DateTime,
    /// UUID del staff user que persistió este doc por última vez.
    #[serde(default)]
    pub editor_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiInstallationConfigItem {
    pub connection_type: ConnectionType,
    pub base_cost_usd: f64,
    pub includes: String,
    pub excedente_per_meter_usd: Option<f64>,
    pub excedente_notes: String,
    pub notes: String,
    pub updated_at: String,
    pub editor_id: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiInstallationConfigResponse {
    pub ok: bool,
    pub data: AiInstallationConfigItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiInstallationConfigsListResponse {
    pub ok: bool,
    pub data: Vec<AiInstallationConfigItem>,
}

/// Body de `PATCH /installations/:type`. Todo opcional (PATCH semántico).
#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct UpdateAiInstallationConfigRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub includes: Option<String>,
    /// Tri-state. `Some(Some(v))` → setear; `Some(None)` → limpiar (no aplica
    /// excedente); `None` → no tocar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excedente_per_meter_usd: Option<Option<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excedente_notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

// ============================================
// AiPromotion — promociones vigentes que la tool `get_active_promotions` expone
// ============================================
//
// Colección `AiPromotions`. Texto libre para `description`/`conditions`/`benefit`
// — la IA las cuenta literal al cliente. El back NO interpreta semánticamente
// las condiciones; solo filtra por fecha + flag `is_active`.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiPromotion {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Etiqueta corta para el listado del SUPERADMIN.
    pub name: String,
    /// Resumen de la promo (texto libre).
    pub description: String,
    /// Condiciones para que aplique
    /// (ej: "Solo plan Conexión Avanzada", "Solo pago en USD").
    pub conditions: String,
    /// Beneficio (ej: "Instalación gratis", "10% off plan").
    pub benefit: String,
    pub starts_at: DateTime,
    pub ends_at: DateTime,
    /// Override manual del admin: `false` la apaga aunque las fechas estén OK.
    pub is_active: bool,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiPromotionItem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub conditions: String,
    pub benefit: String,
    /// ISO8601 con timezone Caracas.
    pub starts_at: String,
    pub ends_at: String,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiPromotionResponse {
    pub ok: bool,
    pub data: AiPromotionItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiPromotionsListResponse {
    pub ok: bool,
    pub data: Vec<AiPromotionItem>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAiPromotionRequest {
    pub name: String,
    pub description: String,
    pub conditions: String,
    pub benefit: String,
    /// ISO8601 con timezone (ej: `"2026-04-01T00:00:00-04:00"`).
    pub starts_at: String,
    pub ends_at: String,
    #[serde(default)]
    pub is_active: Option<bool>,
}

#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct UpdateAiPromotionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benefit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,
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
    pub qualification_window_turns: u32,
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
            qualification_window_turns: e.qualification_window_turns,
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
    pub qualification_window_turns: Option<u32>,
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

// ──────────────────────────────────────────────────────────────────────────────
// Phase 3a — Metrics response DTOs
// ──────────────────────────────────────────────────────────────────────────────

/// Breakdown de pre-clasificaciones por variante (`Spam`, `GreetingOnly`, etc.).
#[derive(Debug, Serialize, ToSchema, Default)]
pub struct AiAgentPreClassBreakdown {
    pub spam: u64,
    pub greeting_only: u64,
    pub clear_ventas: u64,
    pub clear_pagos: u64,
    pub clear_soporte: u64,
    pub ambiguous: u64,
}

/// Bucket diario (cuando `granularity=daily`).
#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentMetricsDailyBucketDto {
    /// Fecha en formato `YYYY-MM-DD` (TZ Caracas).
    pub date: String,
    pub total_turns: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_thinking_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_cost_usd: f64,
    pub pre_classified_count: u64,
    pub escalated_count: u64,
}

/// Métricas resumen para el rango pedido.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentMetricsData {
    pub total_turns: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_thinking_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_cost_usd: f64,
    pub avg_latency_ms: f64,
    pub pre_classified_count: u64,
    pub escalated_count: u64,
    pub tool_calls_count: u64,
    /// Spec 30.3: `total_cached_tokens / total_input_tokens` (0..1). 0.0 cuando
    /// no hay input. Mide la efectividad del implicit caching de Gemini —
    /// idealmente ≥ 0.5 con prompts estables.
    pub cache_hit_rate: f64,
    pub pre_class_breakdown: AiAgentPreClassBreakdown,
    /// `null` cuando `granularity=summary`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily: Option<Vec<AiAgentMetricsDailyBucketDto>>,
}

/// Respuesta del endpoint `GET /agents/:id/metrics`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiAgentMetricsResponse {
    pub ok: bool,
    pub data: AiAgentMetricsData,
}

// ──────────────────────────────────────────────────────────────────────────────
// Cost estimation — OpenRouter
// ──────────────────────────────────────────────────────────────────────────────

struct ModelRates {
    input_per_m: f64,
    output_per_m: f64,
    /// Cached input rate. OpenRouter no cobra caché diferenciado en la mayoría
    /// de los modelos — usamos 0.0 como fallback seguro (subestima ligeramente).
    cached_input_per_m: f64,
}

// Tarifas 2026-05. Fuente: https://openrouter.ai/models
// Revisión trimestral recomendada.
const RATES_GPT4O_MINI: ModelRates = ModelRates {
    input_per_m: 0.15,
    output_per_m: 0.60,
    cached_input_per_m: 0.075,
};
const RATES_CLAUDE_HAIKU: ModelRates = ModelRates {
    input_per_m: 1.00,
    output_per_m: 5.00,
    cached_input_per_m: 0.10,
};
const RATES_LLAMA_70B: ModelRates = ModelRates {
    input_per_m: 0.12,
    output_per_m: 0.30,
    cached_input_per_m: 0.0,
};
/// Fallback para modelos no reconocidos — usamos gpt-4o-mini rates como
/// estimación conservadora.
const RATES_DEFAULT: ModelRates = RATES_GPT4O_MINI;

fn rate_for_model(model_id: &str) -> ModelRates {
    let m = model_id.to_lowercase();
    if m.contains("gpt-4o-mini") {
        RATES_GPT4O_MINI
    } else if m.contains("claude") && m.contains("haiku") {
        RATES_CLAUDE_HAIKU
    } else if m.contains("llama") && m.contains("70b") {
        RATES_LLAMA_70B
    } else {
        tracing::debug!(
            "[ai_agent] model_id '{}' no reconocido — usando RATES_DEFAULT (gpt-4o-mini)",
            model_id
        );
        RATES_DEFAULT
    }
}

/// Estimación de costo USD para un turno completo.
///
/// Formula:
///   billable_input = input_tokens - cached_tokens
///   cost = billable_input * input_rate + cached * cached_rate + (output + thinking) * output_rate
///   (todo por 1M)
///
/// `thinking_tokens` siempre es 0 en modelos OpenRouter no-reasoning (mantenido
/// por estabilidad de esquema). `cached_tokens` también es 0 en la mayoría de
/// los modelos salvo que el proveedor lo soporte explícitamente.
///
/// El resultado es una ESTIMACIÓN — el billing real viene de OpenRouter Console.
pub fn estimate_cost_usd(
    model_id: &str,
    input_tokens: u32,
    cached_tokens: u32,
    output_tokens: u32,
    thinking_tokens: u32,
) -> f64 {
    let r = rate_for_model(model_id);
    let billable_input = input_tokens.saturating_sub(cached_tokens) as f64;
    let cached = cached_tokens as f64;
    let output = output_tokens as f64;
    let thinking = thinking_tokens as f64;
    (billable_input * r.input_per_m
        + cached * r.cached_input_per_m
        + output * r.output_per_m
        + thinking * r.output_per_m)
        / 1_000_000.0
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── AiConfig::to_dto tests (A4) ────────────────────────────────────────

    #[test]
    fn ai_config_to_dto_populated() {
        let doc = AiConfig {
            id: None,
            openrouter_api_key: "ciphertext_abc".to_string(),
            default_model: "openai/gpt-4o".to_string(),
            updated_at: DateTime::now(),
            editor_id: "user-uuid-123".to_string(),
        };
        let dto = doc.to_dto();
        assert!(dto.has_api_key, "has_api_key should be true when key is non-empty");
        assert_eq!(dto.default_model, "openai/gpt-4o");
        assert!(dto.updated_at.is_some(), "updated_at should be present");
        assert_eq!(dto.editor_id, Some("user-uuid-123".to_string()));
    }

    #[test]
    fn ai_config_to_dto_empty_editor_id() {
        let doc = AiConfig {
            id: None,
            openrouter_api_key: "some_cipher".to_string(),
            default_model: String::new(),
            updated_at: DateTime::now(),
            editor_id: String::new(),
        };
        let dto = doc.to_dto();
        assert!(dto.has_api_key);
        assert!(dto.editor_id.is_none(), "empty editor_id should map to None");
    }

    #[test]
    fn ai_config_to_dto_empty_key() {
        let doc = AiConfig {
            id: None,
            openrouter_api_key: String::new(),
            default_model: String::new(),
            updated_at: DateTime::now(),
            editor_id: String::new(),
        };
        let dto = doc.to_dto();
        assert!(!dto.has_api_key, "has_api_key should be false when key is empty");
    }

    #[test]
    fn ai_config_dto_default() {
        let dto = AiConfigDto::default();
        assert!(!dto.has_api_key);
        assert!(dto.default_model.is_empty());
        assert!(dto.updated_at.is_none());
        assert!(dto.editor_id.is_none());
    }

    // ─── estimate_cost_usd tests ─────────────────────────────────────────────

    #[test]
    fn test_estimate_gpt4o_mini() {
        // 1M input + 1M output → $0.15 + $0.60 = $0.75
        let cost = estimate_cost_usd("openai/gpt-4o-mini", 1_000_000, 0, 1_000_000, 0);
        let expected = RATES_GPT4O_MINI.input_per_m + RATES_GPT4O_MINI.output_per_m;
        assert!((cost - expected).abs() < 1e-9, "cost={}, expected={}", cost, expected);
    }

    #[test]
    fn test_estimate_unknown_model_returns_default() {
        // Unknown model falls back to gpt-4o-mini rates
        let cost_unknown = estimate_cost_usd("unknown/model-xyz", 1_000_000, 0, 0, 0);
        let cost_default = estimate_cost_usd("openai/gpt-4o-mini", 1_000_000, 0, 0, 0);
        assert!((cost_unknown - cost_default).abs() < 1e-9);
    }

    #[test]
    fn test_estimate_zero_tokens_is_zero() {
        let cost = estimate_cost_usd("openai/gpt-4o-mini", 0, 0, 0, 0);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn test_estimate_cached_reduces_billable() {
        // With 500k cached out of 1M input: billable = 500k
        let full_cost = estimate_cost_usd("openai/gpt-4o-mini", 1_000_000, 0, 0, 0);
        let cached_cost = estimate_cost_usd("openai/gpt-4o-mini", 1_000_000, 500_000, 0, 0);
        assert!(cached_cost < full_cost, "cached should cost less than full");
    }
}
