//! Tool registry + implementaciones del AI Agent (PR 2 — 4 tools).
//!
//! El loop (en `runner.rs`) llama `build_function_declarations` con los tools
//! habilitados de la config y los pasa a Gemini. Cuando Gemini responde con un
//! `functionCall`, el loop invoca `execute_tool(name, args, ctx)` y reenvía el
//! resultado serializado al siguiente turno.
//!
//! `ToolContext.is_sandbox` corta side-effects en escritura: `request_human` y
//! `create_ticket` devuelven una respuesta sintética sin tocar DB. Tools de
//! lectura (`lookup_customer`, `get_invoices`) siempre pegan a DB — son
//! seguros y validar el flujo end-to-end es el punto del sandbox.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    db::{
        AiAgentRepository, AiInstallationRepository, AiPromotionRepository, ConversationAiPatch,
        ProfileRepository, SalesRepository, WaTicketRepository, WhatsAppRepository,
    },
    models::{
        ai_agent::{AiAgent, AiAgentMode, AiInvoice, AiToolConfig, ConnectionType},
        whatsapp::{StatePatch, WaTicket, WaTicketTimelineEntry},
    },
    state::AppState,
};

use super::escalation;
use super::state::slugify_label;

use super::openrouter::{Tool, ToolFunction};

// ============================================
// Contexto + resultado
// ============================================

/// Contexto compartido para la ejecución de tools dentro de un turno.
///
/// `workspace_id` y `business_phone` los va a usar el dispatch real de PR 3
/// para audit events y para enrutar mensajes outbound. `agent_id` queda para
/// poder etiquetar `AiInteraction` cuando el persist real arranque.
#[derive(Clone)]
pub struct ToolContext {
    pub state: Arc<AppState>,
    #[allow(dead_code)]
    pub workspace_id: ObjectId,
    #[allow(dead_code)]
    pub business_phone: String,
    pub agent_id: ObjectId,
    /// Conversación origen del turno. `None` cuando estamos en sandbox sin
    /// conv asociada — `create_ticket` devuelve fake en ese caso.
    pub conversation_id: Option<ObjectId>,
    /// UUID del AI user (creador atribuido en mensajes/tickets/audit).
    pub ai_user_id: String,
    pub ai_user_name: String,
    /// Cuando `true`: tools de escritura no persisten ni emiten WS — devuelven
    /// un payload sintético para que el loop pueda continuar.
    pub is_sandbox: bool,
    /// Lista de agentes IA a los que `transfer_to_agent` puede derivar — sale
    /// de `agent.tools[transfer_to_agent].config.allowed_targets`. Vacío si
    /// el tool está deshabilitado o sin `allowed_targets` configurados.
    pub allowed_transfer_targets: Vec<ObjectId>,
    /// Mapping `(id, label)` resuelto vía DB para los `allowed_transfer_targets`.
    /// Se inyecta en la `description` del enum del schema para que el LLM sepa
    /// qué agente representa cada hex (sin esto el modelo elige IDs al azar y
    /// el reason no matchea con el target real).
    pub transfer_target_labels: Vec<(ObjectId, String)>,
    /// Snapshot del agente al inicio del turno. Usado por `auto_escalate`
    /// para decidir si mandar `farewell_to_human` (sólo en `live`).
    pub agent_snapshot: Arc<AiAgent>,
    /// Categoría default que `create_ticket` usa cuando la IA no manda
    /// `category_id`. Sale de `escalation.default_ticket_category_id`.
    pub default_ticket_category_id: Option<String>,

    /// Zonas (textos crudos normalizados) que el cliente mencionó en sus
    /// mensajes inbound recientes. Precomputado en dispatch desde el slice
    /// `recent`. Vacío en sandbox (los guardrails se gatean por
    /// `is_sandbox` antes de leer este campo).
    pub customer_explicit_zones: Vec<String>,

    /// media_ids de mensajes inbound recientes con archivo adjunto.
    /// Precomputado en dispatch. Vacío en sandbox.
    pub recent_media_ids: Vec<String>,

    /// Toggle del workspace para guardrails server-side (Phase 1).
    /// Resuelto desde `WaSettings.enable_guardrails` en dispatch. Los
    /// agentes acatan la política del workspace al que pertenecen.
    /// Solo lo leen las tools de validación (check_coverage,
    /// report_payment); el toggle de conversation_state lo lee dispatch
    /// directamente desde wa_settings, no se propaga al ctx.
    pub workspace_enable_guardrails: bool,
}

#[allow(dead_code)]
fn agent_mode_is_live(ctx: &ToolContext) -> bool {
    matches!(ctx.agent_snapshot.mode, AiAgentMode::Live)
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub success: bool,
    pub data: Value,
    pub error: Option<String>,
    pub duration_ms: u32,
    /// Patches que el dispatch plegará en `WaConversation.ai_conv_state`
    /// después del chain loop. Vacío por defecto — las tools opt-in con
    /// `.with_patches(vec![...])` en el path de éxito.
    pub state_patches: Vec<StatePatch>,
}

impl ToolResult {
    fn ok(data: Value, started: Instant) -> Self {
        ToolResult {
            success: true,
            data,
            error: None,
            duration_ms: started.elapsed().as_millis() as u32,
            state_patches: Vec::new(),
        }
    }
    fn err(msg: impl Into<String>, started: Instant) -> Self {
        let m = msg.into();
        ToolResult {
            success: false,
            data: json!({ "error": &m }),
            error: Some(m),
            duration_ms: started.elapsed().as_millis() as u32,
            state_patches: Vec::new(),
        }
    }
    /// Builder: adjunta patches al resultado. Las tools lo llaman en el
    /// path de éxito: `ToolResult::ok(...).with_patches(vec![...])`.
    fn with_patches(mut self, patches: Vec<StatePatch>) -> Self {
        self.state_patches = patches;
        self
    }
}

// ============================================
// Schemas (JSON parameters) — los manda Gemini en cada call
// ============================================

pub const T_LOOKUP_CUSTOMER: &str = "lookup_customer";
pub const T_GET_INVOICES: &str = "get_invoices";
pub const T_REQUEST_HUMAN: &str = "request_human";
pub const T_CREATE_TICKET: &str = "create_ticket";
pub const T_TRANSFER_AGENT: &str = "transfer_to_agent";
pub const T_LIST_PLANS: &str = "list_plans";
pub const T_CHECK_COVERAGE: &str = "check_coverage";
pub const T_CALCULATE_AMOUNT_BS: &str = "calculate_amount_bs";
pub const T_REPORT_PAYMENT: &str = "report_payment";
pub const T_GET_INSTALLATION_INFO: &str = "get_installation_info";
pub const T_GET_ACTIVE_PROMOTIONS: &str = "get_active_promotions";
pub const T_GET_PAYMENT_METHODS: &str = "get_payment_methods";
pub const T_LIST_BANKS: &str = "list_banks";

/// Categoría operativa de un tool, usada por `dispatch.rs` para decidir si un
/// turn cuenta como "resolución" (que resetea el counter) o sólo como "trabajo
/// en progreso" (skip increment, sin reset).
///
/// **Action**: el tool cambia estado externo o transfiere al humano. Un turn
/// con un Action exitoso resetea `no_resolution_count`.
///
/// **InfoLookup**: el tool consulta info pública o de catálogo. Un turn con
/// sólo InfoLookup exitosos no resetea — el agente aún está conversando.
///
/// Al agregar una tool nueva, se debe agregar al `TOOL_CATALOG`. Una tool que
/// no esté en el catálogo cae en `InfoLookup` con `tracing::warn!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    InfoLookup,
    Action,
}

/// Metadata declarativa de un tool. Fuente de verdad única para:
/// - El catálogo que la UI consume (`/v1/auth-user/whatsapp/ai-agent/tools`)
/// - La categoría operativa que `dispatch.rs` lee
/// - El flag `default_enabled` que se aplica al crear agentes nuevos
///
/// El **prompt para Gemini** (description larga + JSON schema de parámetros)
/// vive aparte en `tool_default` — esa parte es prompt engineering y se
/// mantiene separada del metadata UX para no colapsar dos contratos distintos.
///
/// El **config_schema** del agente (ej: `allowed_targets` de `transfer_to_agent`)
/// se resuelve en `tool_config_schema(name)` — es opcional y muy puntual.
pub struct ToolMeta {
    /// Identificador estable. Se guarda en `AiAgent.tools[].name`.
    pub name: &'static str,
    /// Etiqueta corta para el editor (UI).
    pub display_name: &'static str,
    /// Descripción human-friendly que la UI muestra como helper text.
    /// NO es la description que va a Gemini — esa vive en `tool_default`.
    pub ui_description: &'static str,
    /// Categoría visual para agrupar en la UI ("lookup", "info", "escalation",
    /// "transfer", "action").
    pub ui_category: &'static str,
    /// Si la tool se incluye habilitada en agentes nuevos.
    pub default_enabled: bool,
    /// Categoría operativa para el dispatch (resolución vs progreso).
    pub operational_category: ToolCategory,
}

/// Catálogo único de tools soportadas. Agregar una tool nueva requiere:
/// 1. Constante `T_*` arriba
/// 2. Entrada acá
/// 3. Arm en `tool_default` (descripción Gemini + params schema)
/// 4. Arm en `execute_tool` dispatch
/// 5. Si tiene config del agente: arm en `tool_config_schema`
const TOOL_CATALOG: &[ToolMeta] = &[
    ToolMeta {
        name: T_LOOKUP_CUSTOMER,
        display_name: "Buscar cliente",
        ui_description: "Busca clientes ISP por teléfono o cédula. La IA debe llamar antes de hablar de datos personales.",
        ui_category: "lookup",
        default_enabled: true,
        operational_category: ToolCategory::InfoLookup,
    },
    ToolMeta {
        name: T_GET_INVOICES,
        display_name: "Consultar deudas / facturas",
        ui_description: "Devuelve las deudas activas con su saldo pendiente convertido a bolívares (tasa BCV vigente + IVA aplicado). El campo `amount_bs` es lo que falta por cobrar HOY, listo para mostrar al cliente.",
        ui_category: "lookup",
        default_enabled: true,
        operational_category: ToolCategory::InfoLookup,
    },
    ToolMeta {
        name: T_LIST_PLANS,
        display_name: "Listar planes de internet",
        ui_description: "Catálogo de planes (sin precio). Para uso típico del agente de Ventas.",
        ui_category: "info",
        default_enabled: false,
        operational_category: ToolCategory::InfoLookup,
    },
    ToolMeta {
        name: T_CHECK_COVERAGE,
        display_name: "Verificar cobertura por zona",
        ui_description: "Indica si una zona/sector tiene cobertura activa.",
        ui_category: "info",
        default_enabled: false,
        operational_category: ToolCategory::InfoLookup,
    },
    ToolMeta {
        name: T_REQUEST_HUMAN,
        display_name: "Derivar a humano",
        ui_description: "Pausa la IA y libera la conversación para que un agente humano la tome.",
        ui_category: "escalation",
        default_enabled: true,
        operational_category: ToolCategory::Action,
    },
    ToolMeta {
        name: T_CREATE_TICKET,
        display_name: "Crear ticket de soporte",
        ui_description: "Crea un ticket categorizado y cierra la conversación, escalando a humano.",
        ui_category: "escalation",
        default_enabled: true,
        operational_category: ToolCategory::Action,
    },
    ToolMeta {
        name: T_CALCULATE_AMOUNT_BS,
        display_name: "Calcular monto en Bs",
        ui_description: "Convierte USD a Bs aplicando la tasa BCV vigente y el IVA configurado (sTarget=DEFAULT). Llamar al cotizar precios en bolívares.",
        ui_category: "info",
        default_enabled: false,
        operational_category: ToolCategory::InfoLookup,
    },
    ToolMeta {
        name: T_GET_INSTALLATION_INFO,
        display_name: "Info de instalación",
        ui_description: "Retorna el costo base y detalles de instalación para un tipo de conexión (fibra o antena). Usar al cotizar instalación.",
        ui_category: "info",
        default_enabled: false,
        operational_category: ToolCategory::InfoLookup,
    },
    ToolMeta {
        name: T_GET_ACTIVE_PROMOTIONS,
        display_name: "Promociones activas",
        ui_description: "Lista las promociones vigentes. Llamar al cotizar para informar al cliente de descuentos o beneficios actuales.",
        ui_category: "info",
        default_enabled: false,
        operational_category: ToolCategory::InfoLookup,
    },
    ToolMeta {
        name: T_REPORT_PAYMENT,
        display_name: "Reportar pago",
        ui_description: "Registra un reporte de pago del cliente con referencia, monto y comprobante (foto). Crea un documento en PaymentReports en estado \"Pendiente\" y abre un ticket en cobranzas/facturación para que el equipo lo revise. La aprobación y el ajuste de saldo/deuda los hace un humano desde el panel.",
        ui_category: "action",
        default_enabled: false,
        operational_category: ToolCategory::Action,
    },
    ToolMeta {
        name: T_TRANSFER_AGENT,
        display_name: "Transferir a otro agente IA",
        ui_description: "Deriva la conversación a otro agente IA del whitelist (Soporte, Pagos, etc).",
        ui_category: "transfer",
        default_enabled: false,
        operational_category: ToolCategory::Action,
    },
    ToolMeta {
        name: T_GET_PAYMENT_METHODS,
        display_name: "Métodos de pago del proveedor",
        ui_description: "Devuelve los datos de pago móvil del proveedor que atiende al cliente (banco, cédula, teléfono). Llamar cuando el cliente pregunta '¿cómo pago?' o '¿a dónde transfiero?'.",
        ui_category: "info",
        default_enabled: false,
        operational_category: ToolCategory::InfoLookup,
    },
    ToolMeta {
        name: T_LIST_BANKS,
        display_name: "Listar bancos emisores",
        ui_description: "Catálogo de bancos del país (BCV). Llamar ANTES de report_payment para que el cliente elija el banco emisor y pasar el id elegido en issuing_bank_id.",
        ui_category: "info",
        // Prerequisito de report_payment: sin esta tool el LLM no tiene de
        // dónde sacar el ObjectId del banco emisor. Se enciende por default
        // para que agentes nuevos no queden bloqueados al reportar pagos.
        default_enabled: true,
        operational_category: ToolCategory::InfoLookup,
    },
];

/// Lista pública del catálogo de tools (orden estable).
pub fn tool_catalog() -> &'static [ToolMeta] {
    TOOL_CATALOG
}

/// Lookup por nombre. `None` si la tool no está registrada en el catálogo.
pub fn tool_meta(name: &str) -> Option<&'static ToolMeta> {
    TOOL_CATALOG.iter().find(|m| m.name == name)
}

/// Schema del `config` del agente para una tool dada. Sólo aplica a tools que
/// tienen configuración por agente (ej: `transfer_to_agent.allowed_targets`).
/// Devuelve `None` para tools sin config.
pub fn tool_config_schema(name: &str) -> Option<Value> {
    match name {
        T_TRANSFER_AGENT => Some(json!({
            "type": "object",
            "required": ["allowed_targets"],
            "properties": {
                "allowed_targets": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "ui_widget": "ai_agent_multiselect",
                    "description": "ObjectId hex de cada agente IA destino. El front filtra excluyendo el id del agente que se está editando."
                }
            }
        })),
        _ => None,
    }
}

/// Mapea el `tool_name` al ToolCategory leyendo del catálogo. Default safe:
/// `InfoLookup` para nombres no registrados (emite `warn!` para que el dev
/// agregue la tool nueva al `TOOL_CATALOG`).
pub fn tool_category(tool_name: &str) -> ToolCategory {
    match tool_meta(tool_name) {
        Some(m) => m.operational_category,
        None => {
            tracing::warn!(
                "[ai_agent.tools] tool_category: unknown tool name '{}' — defaulting to InfoLookup. Add it to TOOL_CATALOG in tools.rs.",
                tool_name
            );
            ToolCategory::InfoLookup
        }
    }
}

/// Cache TTL para `list_plans` y `check_coverage`. Admins editan poco; en cada
/// write se invalida explícitamente.
const AI_BUSINESS_CACHE_TTL_SECS: u64 = 300;

/// Cache TTL para `get_payment_methods`. Más corto que planes porque un admin
/// que corrige el método de pago debe ver el efecto en < 1 min.
const AI_PAYMENT_METHODS_CACHE_TTL_SECS: u64 = 60;

/// Normaliza un string para matchear zonas: lowercase, sin tildes, trim.
pub(crate) fn normalize_zone(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

/// Lista de los 4 tools de PR 2 con descriptions default y schemas. El SUPERADMIN
/// puede sobreescribir el `description` desde el front (`description_override`),
/// pero no el schema — eso es contrato.
fn tool_default(name: &str) -> Option<(&'static str, Value)> {
    match name {
        T_LOOKUP_CUSTOMER => Some((
            "Busca uno o varios clientes ISP en la base por número de teléfono o cédula. \
             Llamar PRIMERO antes de hablar de datos del cliente. Si devuelve múltiples \
             resultados, preguntar al cliente cuál servicio quiere consultar.",
            json!({
                "type": "object",
                "properties": {
                    "phone": { "type": "string", "description": "Teléfono en cualquier formato (E.164, local, con o sin +). Opcional." },
                    "identification": { "type": "string", "description": "Cédula (V-12345678) o RIF (J-...). Opcional." }
                }
            }),
        )),
        T_GET_INVOICES => Some((
            "Lista las deudas activas del cliente con el SALDO PENDIENTE de cada una \
             (monto original menos abonos parciales ya recibidos), YA CONVERTIDO a bolívares \
             aplicando la tasa BCV vigente y el IVA configurado. Usar después de \
             lookup_customer para responder consultas de saldo, monto a pagar o estado de \
             cobranza. El campo `amount_bs` es Bs listos para mostrar al cliente — NO lo \
             conviertas, NO le agregues IVA, NO lo trates como USD. NUNCA inventar números \
             — siempre llamar este tool. NUNCA respondas un saldo sin haber llamado este tool \
             antes en la conversación.",
            json!({
                "type": "object",
                "properties": {
                    "client_id": { "type": "string", "description": "ID hex del cliente devuelto por lookup_customer." },
                    "limit": { "type": "integer", "description": "Cuántas facturas devolver. Default 5." }
                },
                "required": ["client_id"]
            }),
        )),
        T_REQUEST_HUMAN => Some((
            "Marca la conversación para que la atienda un humano. Usar cuando el cliente \
             pide hablar con una persona pero no hay un problema concreto que requiera ticket. \
             No crea ticket — solo libera la conversación.",
            json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "description": "Por qué se está derivando." }
                },
                "required": ["reason"]
            }),
        )),
        T_CREATE_TICKET => Some((
            "Crea un ticket de soporte y cierra la conversación, escalando a un agente humano. \
             Usar cuando el cliente reporta un problema concreto fuera del scope de la IA \
             (cambio de plan, queja formal, falla técnica que requiere intervención).",
            json!({
                "type": "object",
                "properties": {
                    "category_id": {
                        "type": "string",
                        "description": "Una de: ventas_contrataciones, cobranzas_facturacion, gestion_planes, bajas_retencion, actualizacion_datos, soporte_primer_segundo_nivel, configuraciones_tecnicas, mantenimiento_red, despacho_tecnico, aprovisionamiento."
                    },
                    "reason": { "type": "string", "description": "Resumen del motivo (1-500 chars)." },
                    "summary": { "type": "string", "description": "Contexto adicional del caso para el agente humano." }
                },
                "required": ["category_id", "reason"]
            }),
        )),
        T_LIST_PLANS => Some((
            "Lista los planes de internet residenciales disponibles (nombre, velocidad, dispositivos recomendados, beneficios). \
             NO incluye precios — el back filtra esa info. Si el cliente pregunta el costo, decirle que el precio se confirma con el equipo comercial.",
            json!({
                "type": "object",
                "properties": {}
            }),
        )),
        T_CHECK_COVERAGE => Some((
            "Verifica si una zona/sector/municipio tiene cobertura. \
             SOLO llamar cuando el cliente DIJO EXPLÍCITAMENTE dónde vive en su último mensaje o en la conversación. \
             NUNCA inventes la zona ni la infieras del prior estadístico (ej: 'es venezolano → debe ser Naguanagua'). \
             Si el cliente NO mencionó la zona: NO llames este tool — preguntale primero '¿de qué zona/municipio nos escribís?'. \
             La verificación es por nombre de municipio o sector — pasá el texto que mencionó el cliente tal cual.",
            json!({
                "type": "object",
                "properties": {
                    "zone": {
                        "type": "string",
                        "description": "Nombre de la zona, municipio o sector mencionado por el cliente. Ej: 'Valencia', 'San Diego', 'los guayos'."
                    }
                },
                "required": ["zone"]
            }),
        )),
        T_CALCULATE_AMOUNT_BS => Some((
            "Convierte un monto en USD a bolívares aplicando la tasa BCV vigente \
             y el IVA configurado en el sistema. Llamar SIEMPRE que el cliente \
             pregunte un precio en Bs — NUNCA inventes la tasa ni el total. \
             Devuelve `amount_bs` (con IVA ya aplicado, este es el monto que \
             debés mostrarle al cliente) más `bcv_rate` y `iva_percent` como \
             info de transparencia.",
            json!({
                "type": "object",
                "properties": {
                    "amount_usd": {
                        "type": "number",
                        "description": "Monto en dólares a convertir. Debe ser mayor a 0."
                    }
                },
                "required": ["amount_usd"]
            }),
        )),
        T_GET_INSTALLATION_INFO => Some((
            "Retorna el costo y detalles de instalación para un tipo de conexión. \
             Llamar cuando el cliente pregunta cuánto cuesta instalar o qué incluye la instalación. \
             El parámetro `connection_type` debe ser 'fibra' o 'antena'. \
             Si la zona soporta ambos tipos, preguntar al cliente cuál prefiere antes de llamar.",
            json!({
                "type": "object",
                "properties": {
                    "connection_type": {
                        "type": "string",
                        "enum": ["fibra", "antena"],
                        "description": "Tipo de conexión a consultar."
                    }
                },
                "required": ["connection_type"]
            }),
        )),
        T_GET_ACTIVE_PROMOTIONS => Some((
            "Lista las promociones vigentes de contratación. \
             Llamar cuando el cliente pregunta por descuentos, promociones u ofertas. \
             También llamar automáticamente después de `list_plans` o `get_installation_info` \
             para informar si hay promo aplicable.",
            json!({
                "type": "object",
                "properties": {}
            }),
        )),
        T_TRANSFER_AGENT => Some((
            "Deriva la conversación a OTRO agente IA especializado (Soporte, Pagos, etc). \
             Usar cuando este agente no es el indicado para el caso pero sí lo es alguno de \
             los listados en `target_agent_id`. No interrumpe la conversación: el cliente sigue \
             hablando con la IA pero a partir del próximo turno responde el agente destino.",
            json!({
                "type": "object",
                "properties": {
                    "target_agent_id": {
                        "type": "string",
                        "description": "ObjectId hex del agente IA destino. Debe estar en la whitelist configurada (allowed_targets)."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Por qué se está transfiriendo. El agente destino lo recibe en el próximo turno."
                    }
                },
                "required": ["target_agent_id", "reason"]
            }),
        )),
        T_GET_PAYMENT_METHODS => Some((
            "Devuelve los métodos de pago del proveedor que atiende al cliente \
             (banco, número de cédula, teléfono Pago Móvil). \
             PRECONDICIONES: (1) llamá `lookup_customer` ANTES y confirmá con el cliente \
             cuál servicio si hay varios. (2) Pasá el `client_id` que el cliente CONFIRMÓ \
             — si quedaron varios sin confirmar, preguntale antes. \
             Si la respuesta viene con `items: []` o error `methods_not_configured`, \
             decí al cliente que falta configurar los datos de pago y escalá a humano.",
            json!({
                "type": "object",
                "properties": {
                    "client_id": {
                        "type": "string",
                        "description": "ObjectId hex del cliente devuelto por lookup_customer."
                    }
                },
                "required": ["client_id"]
            }),
        )),
        T_REPORT_PAYMENT => Some((
            "Registra un reporte de pago del cliente (referencia + monto + comprobante). \
             Crea un documento en PaymentReports en estado \"Pendiente\" y abre un ticket en \
             cobranzas/facturación para que el equipo lo revise. La aprobación y el ajuste de \
             saldo/deuda los hace un humano desde el panel. \
             PRECONDICIONES: (1) llamá `lookup_customer` ANTES y confirmá con el cliente \
             cuál servicio si hay varios. (2) Pedile la foto del comprobante por WhatsApp \
             — sin imagen el tool falla. (3) Pasá `amount_bs` O `amount_usd`, NUNCA ambos: \
             el sistema deriva el otro con la tasa BCV vigente. \
             (4) Llamá `list_banks` ANTES y pasá el id elegido en `issuing_bank_id`. \
             El campo `bank` (texto libre) queda DEPRECADO — usá `issuing_bank_id` cuando puedas. \
             La referencia puede venir como texto descriptivo (ej: 'Pago Móvil ref 5678') \
             — el sistema extrae el número canónico automáticamente.",
            json!({
                "type": "object",
                "properties": {
                    "client_id":        { "type": "string", "description": "ObjectId hex devuelto por lookup_customer." },
                    "reference":        { "type": "string", "description": "Referencia bancaria del comprobante. Puede venir como texto libre — el sistema extrae el número canónico automáticamente." },
                    "media_id":         { "type": "string", "description": "ID exacto del media de WhatsApp de la foto del comprobante. DEBE ser uno de los IDs listados en `[turn_state] available_media_ids` (numérico, ej: '1281788957402373'). PROHIBIDO inventar, usar placeholders ('...', 'image_0', 'media_X'), o pasar IDs que no estén en esa lista — el tool rechaza con `media_id_not_in_conversation`. Si el cliente no envió comprobante todavía, NO llames esta tool: pedile la foto primero." },
                    "amount_bs":        { "type": "number", "description": "Monto en bolívares. Mutuamente excluyente con amount_usd." },
                    "amount_usd":       { "type": "number", "description": "Monto en dólares. Mutuamente excluyente con amount_bs." },
                    "issuing_bank_id":  { "type": "string", "description": "ObjectId hex del banco emisor devuelto por list_banks (recomendado: ej '65a7f8d9c3e2a1b4d6f8e0c5'). El backend tolera nombre o código si el LLM no llamó list_banks (ej: 'Banesco' o '0134') y resuelve al ObjectId server-side; si el match es ambiguo o no existe, el tool devuelve error rico con la lista de candidatos. Llamar list_banks ANTES sigue siendo lo correcto." },
                    "bank":             { "type": "string", "description": "[DEPRECATED] Nombre libre del banco origen. Usar issuing_bank_id en su lugar." },
                    "phone":            { "type": "string", "description": "Teléfono asociado al pago móvil. Opcional." },
                    "debt_id":          { "type": "string", "description": "ObjectId hex de la deuda específica si el cliente la mencionó. Opcional — si falta, el reporte queda como abono a cuenta." },
                    "payment_date":     { "type": "string", "description": "Fecha del pago en RFC3339 (ej: 2026-05-04T15:30:00Z). Opcional — default: ahora." }
                },
                "required": ["client_id", "reference", "media_id"]
            }),
        )),
        T_LIST_BANKS => Some((
            "Lista los bancos del catálogo nacional. Usar ANTES de report_payment para que \
             el cliente elija el banco emisor (de dónde salió la transferencia). \
             Pasar el id devuelto al campo issuing_bank_id de report_payment. \
             Argumentos: ninguno.",
            json!({
                "type": "object",
                "properties": {}
            }),
        )),
        _ => None,
    }
}

/// Construye los `FunctionDeclaration` que viajan a Gemini. Filtra por
/// `enabled = true` y aplica `description_override` cuando esté seteado.
///
/// Para `transfer_to_agent` además inyecta:
/// - `enum` con los IDs hex de `allowed_targets` (whitelist de IDs)
/// - `description` enriquecida con el mapping `id → label` para que el modelo
///   sepa qué especialidad representa cada hex. Sin esto Gemini elige IDs al
///   azar (aunque estén en el enum) y la transferencia cae en el agente
///   equivocado.
pub fn build_function_declarations(
    agent: &AiAgent,
    transfer_target_labels: &[(ObjectId, String)],
) -> Vec<Tool> {
    let allowed_transfer_targets = extract_allowed_transfer_targets(&agent.tools);
    agent
        .tools
        .iter()
        .filter(|t| t.enabled)
        .filter_map(|t| {
            // `transfer_to_agent` sin `allowed_targets` configurados =
            // tool inválido, no la mostramos al LLM (la validación de
            // back ya bloquea guardar en ese estado, esto es defensivo).
            if t.name == T_TRANSFER_AGENT && allowed_transfer_targets.is_empty() {
                return None;
            }
            tool_default(&t.name).map(|(default_desc, params)| {
                let parameters = if t.name == T_TRANSFER_AGENT {
                    inject_target_enum(params, &allowed_transfer_targets, transfer_target_labels)
                } else {
                    params
                };
                Tool {
                    kind: "function".to_string(),
                    function: ToolFunction {
                        name: t.name.clone(),
                        description: t
                            .description_override
                            .clone()
                            .unwrap_or_else(|| default_desc.to_string()),
                        parameters,
                    },
                }
            })
        })
        .collect()
}

/// Lee `tools[transfer_to_agent].config.allowed_targets` como `Vec<ObjectId>`.
/// Devuelve vacío cuando el tool no está, no está habilitado, no tiene
/// `config`, o `allowed_targets` está mal formado.
pub fn extract_allowed_transfer_targets(tools: &[AiToolConfig]) -> Vec<ObjectId> {
    let Some(t) = tools.iter().find(|t| t.name == T_TRANSFER_AGENT) else {
        return Vec::new();
    };
    if !t.enabled {
        return Vec::new();
    }
    let Some(cfg) = t.config.as_ref() else {
        return Vec::new();
    };
    let Some(arr) = cfg.get("allowed_targets").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str())
        .filter_map(|s| ObjectId::parse_str(s).ok())
        .collect()
}

fn inject_target_enum(
    mut params: Value,
    allowed_targets: &[ObjectId],
    target_labels: &[(ObjectId, String)],
) -> Value {
    let hexes: Vec<Value> = allowed_targets
        .iter()
        .map(|o| Value::String(o.to_hex()))
        .collect();

    // Construye el bloque `id → label` que se appendea a la description.
    // Solo incluye IDs que estén en `allowed_targets` Y tengan label resuelto
    // (por si la DB perdió alguno entre el config del agente y el dispatch).
    let mapping_lines: Vec<String> = allowed_targets
        .iter()
        .filter_map(|id| {
            target_labels
                .iter()
                .find(|(lid, _)| lid == id)
                .map(|(_, label)| format!("- {} → {}", id.to_hex(), label))
        })
        .collect();

    if let Some(props) = params.get_mut("properties").and_then(|v| v.as_object_mut()) {
        if let Some(target) = props
            .get_mut("target_agent_id")
            .and_then(|v| v.as_object_mut())
        {
            target.insert("enum".to_string(), Value::Array(hexes));
            if !mapping_lines.is_empty() {
                let base = target
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let enriched = format!(
                    "{}\nAgentes destino disponibles (elegí el ID que corresponda al área):\n{}",
                    base.trim_end(),
                    mapping_lines.join("\n")
                );
                target.insert(
                    "description".to_string(),
                    Value::String(enriched.trim_start().to_string()),
                );
            }
        }
    }
    params
}

// ============================================
// Dispatch
// ============================================

pub async fn execute_tool(name: &str, args: Value, ctx: &ToolContext) -> ToolResult {
    let started = Instant::now();
    match name {
        T_LOOKUP_CUSTOMER => exec_lookup_customer(args, ctx, started).await,
        T_GET_INVOICES => exec_get_invoices(args, ctx, started).await,
        T_REQUEST_HUMAN => exec_request_human(args, ctx, started).await,
        T_CREATE_TICKET => exec_create_ticket(args, ctx, started).await,
        T_TRANSFER_AGENT => exec_transfer_to_agent(args, ctx, started).await,
        T_LIST_PLANS => exec_list_plans(args, ctx, started).await,
        T_CHECK_COVERAGE => exec_check_coverage(args, ctx, started).await,
        T_CALCULATE_AMOUNT_BS => exec_calculate_amount_bs(args, ctx, started).await,
        T_REPORT_PAYMENT => exec_report_payment(args, ctx, started).await,
        T_GET_INSTALLATION_INFO => exec_get_installation_info(args, ctx, started).await,
        T_GET_ACTIVE_PROMOTIONS => exec_get_active_promotions(args, ctx, started).await,
        T_GET_PAYMENT_METHODS => exec_get_payment_methods(args, ctx, started).await,
        T_LIST_BANKS => exec_list_banks(args, ctx, started).await,
        other => ToolResult::err(format!("unknown_tool:{}", other), started),
    }
}

// ============================================
// Tool: lookup_customer
// ============================================

#[derive(Deserialize)]
struct LookupCustomerArgs {
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    identification: Option<String>,
}

async fn exec_lookup_customer(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    let parsed: LookupCustomerArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };

    let phone = parsed.phone.as_deref();
    let id = parsed.identification.as_deref();

    match ctx.state.db.find_clients_for_ai_lookup(phone, id).await {
        Ok(items) => {
            // Patch: si hay al menos un resultado, colectar el client_id del primero.
            let patches = if let Some(first) = items.first() {
                let cid = first.client_id.clone();
                vec![
                    StatePatch::SetCollectedData {
                        key: "client_id".into(),
                        value: cid,
                    },
                    StatePatch::AddCompletedAction("lookup_customer".into()),
                ]
            } else {
                vec![StatePatch::AddCompletedAction("lookup_customer".into())]
            };
            ToolResult::ok(json!({ "items": items }), started).with_patches(patches)
        }
        Err(e) => ToolResult::err(format!("db_error:{}", e), started),
    }
}

// ============================================
// Tool: get_invoices
// ============================================

#[derive(Deserialize)]
struct GetInvoicesArgs {
    client_id: String,
    #[serde(default)]
    limit: Option<u32>,
}

async fn exec_get_invoices(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    let parsed: GetInvoicesArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };

    let client_oid = match ObjectId::parse_str(&parsed.client_id) {
        Ok(o) => o,
        Err(_) => return ToolResult::err("invalid_client_id", started),
    };

    // Step 1: fetch active debts (already sorted dCreation ASC by find_active_debts_by_client_ids).
    // `find_active_debts_by_client_ids` ya recorta a `sState != "Pagada"`.
    let debts = match ctx
        .state
        .db
        .find_active_debts_by_client_ids(&[client_oid])
        .await
    {
        Ok(d) => d,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    if debts.is_empty() {
        return ToolResult::ok(json!({ "items": [] }), started)
            .with_patches(vec![StatePatch::AddCompletedAction("get_invoices".into())]);
    }

    // Step 2: gather debt_ids and fetch part_payments
    let debt_ids: Vec<ObjectId> = debts.iter().map(|d| d._id).collect();
    let part_payments = match ctx.state.db.find_part_payments_by_debt_ids(&debt_ids).await {
        Ok(pp) => pp,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    // Step 3: fetch payments by id and build active_payment_ids HashSet
    // Filter to sState == "Activo" in Rust (preserves helper neutrality — same pattern as receivables/handler.rs:124)
    let payment_ids: Vec<ObjectId> = part_payments.iter().map(|pp| pp.id_payment).collect();
    let payments = match ctx.state.db.find_payments_by_ids(&payment_ids).await {
        Ok(p) => p,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    let active_payment_ids: HashSet<ObjectId> = payments
        .iter()
        .filter(|p| p.s_state.eq_ignore_ascii_case("Activo"))
        .map(|p| p._id)
        .collect();

    // Step 4: sum partPayments per debt, only for active payments
    let mut paid_by_debt: HashMap<ObjectId, f64> = HashMap::new();
    for pp in &part_payments {
        if active_payment_ids.contains(&pp.id_payment) {
            *paid_by_debt.entry(pp.id_debt).or_insert(0.0) += pp.n_amount;
        }
    }

    // Step 5: resolve BCV rate + IVA (DEFAULT) — mismo contrato que
    // `exec_calculate_amount_bs` y que `/v2/utils/calculate`. Si falla, abortamos:
    // el LLM tiene la regla "Tool falla → request_human" y nunca debe inventar
    // montos. Devolver USD como fallback abriría el bug que estamos cerrando.
    let rate: f64 = match ctx.state.redis.get_exchange_rate().await {
        Ok(Some(r)) => r,
        _ => match ctx.state.db.get_latest_exchange_rate().await {
            Ok(r) => r,
            Err(_) => return ToolResult::err("exchange_rate_unavailable", started),
        },
    };
    if rate == 0.0 {
        return ToolResult::err("exchange_rate_zero", started);
    }
    let iva_factor = match ctx.state.db.find_tax_by_id(None).await {
        Ok(Some(t)) => t.iva,
        Ok(None) => return ToolResult::err("tax_config_missing", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    // Step 6: compute remaining balance (USD), convert to Bs, filter, then take(limit).
    // CRITICAL: take AFTER filter — fully-paid debts must not steal slots (WI-6 fix).
    let epsilon = 0.001_f64;
    let limit = parsed.limit.unwrap_or(5).clamp(1, 20) as usize;

    let invoices: Vec<AiInvoice> = debts
        .into_iter()
        .filter_map(|d| {
            let paid = paid_by_debt.get(&d._id).copied().unwrap_or(0.0);
            // Round each side to centavos before subtracting — same rounding as receivables/handler.rs:156-157
            let debt_rounded = (d.n_amount * 100.0).round() / 100.0;
            let paid_rounded = (paid * 100.0).round() / 100.0;
            let remaining_usd = debt_rounded - paid_rounded;

            if remaining_usd <= epsilon {
                return None;
            }

            let amount_bs = round2(remaining_usd * rate * iva_factor);

            Some(AiInvoice {
                id: d._id.to_hex(),
                amount_bs, // WI-6: remaining balance, ya convertido a Bs con IVA
                reason: d.s_reason,
                state: d.s_state,
                due_date: d.d_creation.try_to_rfc3339_string().unwrap_or_default(),
            })
        })
        .take(limit)
        .collect();

    ToolResult::ok(json!({ "items": invoices }), started)
        .with_patches(vec![StatePatch::AddCompletedAction("get_invoices".into())])
}

// ============================================
// Tool: request_human
// ============================================

#[derive(Deserialize)]
struct RequestHumanArgs {
    #[serde(default)]
    reason: Option<String>,
}

async fn exec_request_human(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    let parsed: RequestHumanArgs =
        serde_json::from_value(args).unwrap_or(RequestHumanArgs { reason: None });
    let reason = parsed.reason.unwrap_or_default();

    if ctx.is_sandbox || ctx.conversation_id.is_none() {
        return ToolResult::ok(
            json!({
                "ok": true,
                "mode": "sandbox",
                "reason": reason,
                "note": "En sandbox no se altera la conversación. En vivo se libera la conv para humanos."
            }),
            started,
        );
    }

    let conv_id = ctx.conversation_id.unwrap();
    let trimmed_reason = reason.trim();
    let note = if trimmed_reason.is_empty() {
        None
    } else {
        Some(trimmed_reason)
    };

    // El runner enviará el texto final del modelo como respuesta a este turno;
    // el helper NO manda farewell para no duplicar mensajes (el modelo va a
    // armar la despedida basado en `farewell_to_human` que ve en personality).
    escalation::auto_escalate(
        &ctx.state,
        &conv_id,
        &ctx.agent_snapshot,
        escalation::REASON_REQUEST_HUMAN,
        note,
        false,
    )
    .await;

    ToolResult::ok(
        json!({
            "ok": true,
            "mode": "live",
            "reason": reason,
            "ai_disabled": true,
        }),
        started,
    )
    .with_patches(vec![
        StatePatch::AddCompletedAction("request_human".into()),
        StatePatch::SetCurrentStep("transferred_to_human".into()),
    ])
}

// ============================================
// Tool: list_plans
// ============================================

async fn exec_list_plans(_args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    if let Some(cached) = ctx.state.redis.get_ai_plans_cache().await {
        if let Ok(parsed) = serde_json::from_str::<Value>(&cached) {
            return ToolResult::ok(parsed, started)
                .with_patches(vec![StatePatch::AddCompletedAction("list_plans".into())]);
        }
    }

    let plans = match ctx.state.db.list_ai_plans(true).await {
        Ok(p) => p,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let items: Vec<Value> = plans
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "mbps": p.mbps,
                "devices_recommendation": p.devices_recommendation,
                "benefits": p.benefits,
                "price_usd": p.price_usd,
            })
        })
        .collect();
    let response = json!({
        "items": items,
        "note": "price_usd es el precio mensual en USD. Usá calculate_amount_bs para convertir a Bs con la tasa BCV + IVA vigente.",
    });
    if let Ok(s) = serde_json::to_string(&response) {
        ctx.state
            .redis
            .set_ai_plans_cache(&s, AI_BUSINESS_CACHE_TTL_SECS)
            .await;
    }
    ToolResult::ok(response, started)
        .with_patches(vec![StatePatch::AddCompletedAction("list_plans".into())])
}

// ============================================
// Tool: check_coverage
// ============================================

#[derive(Deserialize)]
struct CheckCoverageArgs {
    zone: String,
}

/// Zona cacheada en Redis para uso por `check_coverage`. Sin `id` — Gemini
/// no lo necesita y reducir datos en el contexto del LLM es el objetivo.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct CachedZone {
    display_name: String,
    state: String,
    municipality: String,
    parish: Option<String>,
    sector: Option<String>,
    aliases: Vec<String>,
    #[serde(default)]
    connection_types: Vec<ConnectionType>,
}

/// Carga las zonas activas desde Redis (v2) o DB, y repuebla el caché.
async fn load_active_zones(ctx: &ToolContext) -> Result<Vec<CachedZone>, String> {
    if let Some(cached) = ctx.state.redis.get_ai_coverage_cache_v2().await {
        if let Ok(parsed) = serde_json::from_str::<Vec<CachedZone>>(&cached) {
            return Ok(parsed);
        }
    }
    let zones = ctx.state.db.list_ai_coverage_zones(true).await?;
    let cached: Vec<CachedZone> = zones
        .into_iter()
        .map(|z| CachedZone {
            display_name: z.display_name,
            state: z.state,
            municipality: z.municipality,
            parish: z.parish,
            sector: z.sector,
            aliases: z.aliases,
            connection_types: z.connection_types,
        })
        .collect();
    if let Ok(s) = serde_json::to_string(&cached) {
        ctx.state
            .redis
            .set_ai_coverage_cache_v2(&s, AI_BUSINESS_CACHE_TTL_SECS)
            .await;
    }
    Ok(cached)
}

/// Tier de especificidad de un match. Mayor = más específico. Cuando varias
/// zonas matchean a tiers distintos, se devuelven SÓLO las del tier máximo —
/// así "centro de güigüe" prefiere sector="Centro Güigüe" (SECTOR) sobre
/// otra zona que comparte parish="Güigüe" pero no contiene "centro".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchTier {
    State = 1,
    Municipality = 2,
    Parish = 3,
    Display = 4,
    Sector = 5,
}

/// Tokens cortos que filtramos al comparar nombres de zonas. Conjunciones,
/// preposiciones, artículos + palabras estructurales geográficas que la gente
/// agrega para dar contexto pero NO son identificadoras (ej: "municipio
/// Carlos Arvelo" — solo "carlos arvelo" identifica). Si el cliente dice
/// "centro de güigüe", "de" tampoco debería contar.
const TOKEN_STOPWORDS: &[&str] = &[
    // Conectores
    "de",
    "del",
    "la",
    "el",
    "los",
    "las",
    "y",
    "o",
    "en",
    // Estructurales geográficas (después de normalize_zone, sin tildes)
    "municipio",
    "municip",
    "municipalidad",
    "estado",
    "estados",
    "parroquia",
    "parroquias",
    "sector",
    "sectores",
    "barrio",
    "barrios",
    "urbanizacion",
    "urbanizaciones",
    "urb",
    "zona",
    "zonas",
    "calle",
    "avenida",
    "avenidas",
    "carretera",
    "vereda",
];
const TOKEN_MIN_LEN: usize = 3;

fn tokenize_zone(s: &str) -> Vec<String> {
    normalize_zone(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .filter(|t| t.chars().count() >= TOKEN_MIN_LEN)
        .filter(|t| !TOKEN_STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// Distancia Levenshtein clásica (DP iterativo). Cuenta inserciones,
/// borrados y sustituciones para transformar `a` en `b`.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Match tolerante a typos cortos comunes en WhatsApp venezolano.
/// Reglas (afinadas para minimizar falsos positivos en zonas cortas):
///   - Match exacto → true
///   - Tokens < 4 chars → solo exacto
///   - Plural/sufijo: uno es prefijo del otro y diferencia ≤ 2 chars
///     (cubre "arvelo"/"arvelos", "centro"/"centros")
///   - Levenshtein ≤ 1 si el token largo tiene ≥ 5 chars (1 typo)
///   - Levenshtein ≤ 2 si el token largo tiene ≥ 6 chars (cubre "guigue"/"gugiue"
///     y transposiciones)
///
/// Tokens de 4 chars exigen prefix-match o exacto. Eso evita falsos positivos
/// como "loro"/"lora" — geografías chicas distintas que solo difieren en 1 char.
pub(crate) fn fuzzy_token_match(query: &str, target: &str) -> bool {
    if query == target {
        return true;
    }
    let q_len = query.chars().count();
    let t_len = target.chars().count();
    if q_len < 4 || t_len < 4 {
        return false;
    }

    // Prefix tolerance — captura plurales y sufijos "s", "es", "ito"
    let (shorter, longer, short_len, long_len) = if q_len <= t_len {
        (query, target, q_len, t_len)
    } else {
        (target, query, t_len, q_len)
    };
    if longer.starts_with(shorter) && long_len - short_len <= 2 {
        return true;
    }

    // Levenshtein graduated thresholds — solo desde 5 chars para minimizar
    // falsos positivos en geografías chicas.
    let dist = levenshtein(query, target);
    let max_len = q_len.max(t_len);
    if max_len >= 6 && dist <= 2 {
        return true;
    }
    if max_len >= 5 && dist <= 1 {
        return true;
    }
    false
}

/// `true` si algún token en `tokens` matchea fuzzy con `query_token`.
fn fuzzy_contains(tokens: &[String], query_token: &str) -> bool {
    tokens.iter().any(|t| fuzzy_token_match(query_token, t))
}

/// Construye el "fingerprint" de la zona: union de tokens de TODOS sus campos
/// (sector, parish, display_name, aliases, municipality, state). Usado para
/// constraint-1: la zona explica todos los tokens significativos del cliente.
fn zone_fingerprint(z: &CachedZone) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    if let Some(s) = z.sector.as_deref() {
        tokens.extend(tokenize_zone(s));
    }
    if let Some(p) = z.parish.as_deref() {
        tokens.extend(tokenize_zone(p));
    }
    tokens.extend(tokenize_zone(&z.display_name));
    for a in &z.aliases {
        tokens.extend(tokenize_zone(a));
    }
    tokens.extend(tokenize_zone(&z.municipality));
    tokens.extend(tokenize_zone(&z.state));
    tokens.sort();
    tokens.dedup();
    tokens
}

/// `true` si todos los tokens del cliente aparecen (con tolerancia a typos
/// cortos vía `fuzzy_token_match`) en algún campo de la zona. Sin esto, una
/// zona con sector="Centro" matchearía "centro de Güigüe" aunque "guigue"
/// no exista en NINGÚN campo de esa zona — falso positivo.
///
/// El matcher tolera typos comunes en WhatsApp venezolano:
/// - "arvelos" ↔ "arvelo" (plural/sufijo)
/// - "guigue" ↔ "gugiue" (transposición)
/// - "carabobo" ↔ "carabovo" (1 typo)
fn zone_explains_all_query_tokens(z: &CachedZone, query_tokens: &[String]) -> bool {
    let fp = zone_fingerprint(z);
    query_tokens.iter().all(|qt| fuzzy_contains(&fp, qt))
}

/// Devuelve los tokens del campo que NO aparecen en `less_specific_tokens`.
/// Eso evita que un token redundante (ej: "guigue" presente tanto en sector
/// "Centro Güigüe" como en parish "Güigüe") cuente como especificidad
/// adicional — sólo los tokens UNICOS al campo específico aportan tier-up.
fn distinguishing_tokens(field_tokens: &[String], less_specific: &[String]) -> Vec<String> {
    field_tokens
        .iter()
        .filter(|t| !less_specific.contains(t))
        .cloned()
        .collect()
}

/// Devuelve el tier más específico al que la zona matchea la query, o `None`.
///
/// Constraint 1: la zona debe explicar TODOS los tokens significativos del
/// cliente en algún campo (`zone_explains_all_query_tokens`). Si el cliente
/// dice "centro de güigüe" y la zona no tiene "guigue" en ningún campo, la
/// zona queda fuera — no es honesto explicar solo "centro".
///
/// Constraint 2: clasificar por el campo más específico que comparte al menos
/// un token DISTINGUISHING (no presente en campos menos específicos) con la
/// query. Si la única coincidencia de un sector con la query es un token que
/// también está en su parish, ese match no aporta especificidad sobre parish.
fn zone_match_tier(z: &CachedZone, query_tokens: &[String]) -> Option<MatchTier> {
    if !zone_explains_all_query_tokens(z, query_tokens) {
        return None;
    }

    let parish_tokens: Vec<String> = z.parish.as_deref().map(tokenize_zone).unwrap_or_default();
    let municipality_tokens = tokenize_zone(&z.municipality);
    let state_tokens = tokenize_zone(&z.state);
    let display_tokens = tokenize_zone(&z.display_name);
    let alias_tokens: Vec<String> = z.aliases.iter().flat_map(|a| tokenize_zone(a)).collect();

    let mut display_and_alias = display_tokens.clone();
    display_and_alias.extend(alias_tokens);

    let any_in_query =
        |tokens: &[String]| -> bool { tokens.iter().any(|t| fuzzy_contains(query_tokens, t)) };

    // SECTOR: tokens en sector pero no en parish/muni/state. (No restamos display
    // porque display puede coincidir con sector en zonas chicas y eso sigue siendo
    // legítima especificidad de sector).
    if let Some(ref s) = z.sector {
        let sector_tokens = tokenize_zone(s);
        let mut less = parish_tokens.clone();
        less.extend(municipality_tokens.clone());
        less.extend(state_tokens.clone());
        let dist = distinguishing_tokens(&sector_tokens, &less);
        if any_in_query(&dist) {
            return Some(MatchTier::Sector);
        }
    }

    // DISPLAY/ALIAS: tokens en display o alias pero no en parish/muni/state.
    {
        let mut less = parish_tokens.clone();
        less.extend(municipality_tokens.clone());
        less.extend(state_tokens.clone());
        let dist = distinguishing_tokens(&display_and_alias, &less);
        if any_in_query(&dist) {
            return Some(MatchTier::Display);
        }
    }

    // PARISH: tokens en parish pero no en muni/state.
    if !parish_tokens.is_empty() {
        let mut less = municipality_tokens.clone();
        less.extend(state_tokens.clone());
        let dist = distinguishing_tokens(&parish_tokens, &less);
        if any_in_query(&dist) {
            return Some(MatchTier::Parish);
        }
    }

    // MUNICIPALITY: tokens en muni pero no en state.
    {
        let dist = distinguishing_tokens(&municipality_tokens, &state_tokens);
        if any_in_query(&dist) {
            return Some(MatchTier::Municipality);
        }
    }

    // STATE: cualquier token de state que esté en query.
    if any_in_query(&state_tokens) {
        return Some(MatchTier::State);
    }

    None
}

/// Función pura para testabilidad — devuelve las zonas matcheadas en el
/// tier de especificidad MÁXIMO. Si varias zonas matchean a tiers distintos,
/// las del tier menor se descartan (especificidad gana).
/// `q` debe estar ya normalizado con `normalize_zone`.
fn match_zones<'a>(zones: &'a [CachedZone], q: &str) -> Vec<&'a CachedZone> {
    let q_tokens = tokenize_zone(q);
    if q_tokens.is_empty() {
        return Vec::new();
    }
    let scored: Vec<(MatchTier, &CachedZone)> = zones
        .iter()
        .filter_map(|z| zone_match_tier(z, &q_tokens).map(|t| (t, z)))
        .collect();
    if scored.is_empty() {
        return Vec::new();
    }
    let max_tier = scored.iter().map(|(t, _)| *t).max().unwrap();
    scored
        .into_iter()
        .filter(|(t, _)| *t == max_tier)
        .map(|(_, z)| z)
        .collect()
}

async fn exec_check_coverage(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    let parsed: CheckCoverageArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };
    let raw = parsed.zone.trim();
    if raw.is_empty() {
        return ToolResult::err("missing_zone", started);
    }

    // ── GUARDRAIL: zona debe haber sido mencionada por el cliente ──────────
    if ctx.workspace_enable_guardrails && !ctx.is_sandbox {
        if !crate::modules::ai_agent::guardrails::validate_zone_mentioned(
            raw,
            &ctx.customer_explicit_zones,
        ) {
            return ToolResult::err("zone_not_mentioned_by_customer", started);
        }
    }
    // ── /GUARDRAIL ──────────────────────────────────────────────────────────

    let zones = match load_active_zones(ctx).await {
        Ok(z) => z,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    let q = normalize_zone(raw);
    let matches = match_zones(&zones, &q);

    match matches.len() {
        0 => ToolResult::ok(
            json!({
                "covered": false,
                "disambiguation_required": false,
                "queried_zone": raw,
            }),
            started,
        )
        .with_patches(vec![StatePatch::AddCompletedAction(
            "check_coverage".into(),
        )]),
        1 => {
            let z = matches[0];
            let available_types: Vec<&str> =
                z.connection_types.iter().map(|t| t.as_slug()).collect();
            ToolResult::ok(
                json!({
                    "covered": true,
                    "matched_zone": {
                        "display_name": z.display_name,
                        "state": z.state,
                        "municipality": z.municipality,
                    },
                    "available_types": available_types,
                    "queried_zone": raw,
                }),
                started,
            )
            .with_patches(vec![
                StatePatch::SetCollectedData {
                    key: "zone".into(),
                    value: z.display_name.clone(),
                },
                StatePatch::AddCompletedAction("check_coverage".into()),
            ])
        }
        _ => {
            let summarized: Vec<_> = matches
                .iter()
                .map(|z| {
                    json!({
                        "display_name": z.display_name,
                        "state": z.state,
                        "municipality": z.municipality,
                    })
                })
                .collect();
            ToolResult::ok(
                json!({
                    "covered": null,
                    "disambiguation_required": true,
                    "queried_zone": raw,
                    "matches": summarized,
                    "suggested_question": "¿En qué estado o municipio te encontrás?",
                }),
                started,
            )
            .with_patches(vec![StatePatch::AddCompletedAction(
                "check_coverage".into(),
            )])
        }
    }
}

// ============================================
// Tool: get_installation_info
// ============================================

#[derive(Deserialize)]
struct GetInstallationInfoArgs {
    connection_type: String,
}

async fn exec_get_installation_info(
    args: Value,
    ctx: &ToolContext,
    started: Instant,
) -> ToolResult {
    let parsed: GetInstallationInfoArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };

    let ct = match ConnectionType::from_slug(&parsed.connection_type) {
        Some(t) => t,
        None => {
            return ToolResult::err(
                format!(
                    "invalid_connection_type: '{}'. Usar 'fibra' o 'antena'",
                    parsed.connection_type
                ),
                started,
            )
        }
    };

    let config = match ctx.state.db.get_ai_installation(ct).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return ToolResult::ok(
                json!({
                    "connection_type": ct.as_slug(),
                    "available": false,
                    "note": "No hay configuración de instalación cargada aún. El asesor confirmará los costos.",
                }),
                started,
            );
        }
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    ToolResult::ok(
        json!({
            "connection_type": config.connection_type.as_slug(),
            "base_cost_usd": config.base_cost_usd,
            "includes": config.includes,
            "excedente_per_meter_usd": config.excedente_per_meter_usd,
            "excedente_notes": config.excedente_notes,
            "notes": config.notes,
        }),
        started,
    )
    .with_patches(vec![StatePatch::AddCompletedAction(
        "get_installation_info".into(),
    )])
}

// ============================================
// Tool: get_active_promotions
// ============================================

async fn exec_get_active_promotions(
    _args: Value,
    ctx: &ToolContext,
    started: Instant,
) -> ToolResult {
    let now = mongodb::bson::DateTime::now();
    let promos = match ctx.state.db.list_active_ai_promotions(now).await {
        Ok(p) => p,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    let items: Vec<Value> = promos
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "description": p.description,
                "conditions": p.conditions,
                "benefit": p.benefit,
                "ends_at": p.ends_at.try_to_rfc3339_string().unwrap_or_default(),
            })
        })
        .collect();

    ToolResult::ok(json!({ "items": items }), started).with_patches(vec![
        StatePatch::AddCompletedAction("get_active_promotions".into()),
    ])
}

// ============================================
// Tool: get_payment_methods
// ============================================

async fn exec_get_payment_methods(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    #[derive(Deserialize)]
    struct Args {
        client_id: String,
    }

    let parsed: Args = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };

    // 1. Validate ObjectId format
    let client_oid = match ObjectId::parse_str(parsed.client_id.trim()) {
        Ok(o) => o,
        Err(_) => return ToolResult::err("invalid_client_id", started),
    };

    // 2. Sandbox short-circuit (before any DB call)
    if ctx.is_sandbox {
        return ToolResult::ok(
            json!({
                "mode": "sandbox",
                "items": [{
                    "type": "pago_movil",
                    "bank_name": "Banesco",
                    "id_number": "V-12345678",
                    "phone": "04141234567"
                }],
                "note": "datos de prueba"
            }),
            started,
        )
        .with_patches(vec![StatePatch::AddCompletedAction(
            "get_payment_methods".into(),
        )]);
    }

    // 3. Find client → owner_id
    let owner = match ctx.state.db.find_client_owner_by_id(&client_oid).await {
        Ok(Some(o)) => o,
        Ok(None) => return ToolResult::err("client_not_found", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let owner_id = owner.id_owner;

    // 4. Redis cache hit?
    if let Some(cached) = ctx
        .state
        .redis
        .get_ai_payment_methods_cache(&owner_id)
        .await
    {
        if let Ok(parsed_val) = serde_json::from_str::<Value>(&cached) {
            tracing::info!(
                "[ai_agent.get_payment_methods] cache hit for owner {}",
                owner_id
            );
            return ToolResult::ok(parsed_val, started).with_patches(vec![
                StatePatch::AddCompletedAction("get_payment_methods".into()),
            ]);
        }
    }

    // 5. Resolve owner → payment method ID
    let user_info = match ctx.state.db.find_user_payment_info_by_id(&owner_id).await {
        Ok(Some(u)) => u,
        Ok(None) => return ToolResult::err("methods_not_configured", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    // 6. No payment method configured → return empty (not cached — transient state)
    let pm_id =
        match user_info.id_payment_method {
            Some(id) => id,
            None => {
                return ToolResult::ok(
                json!({
                    "items": [],
                    "note": "El proveedor no tiene métodos de pago configurados, deriva a humano."
                }),
                started,
            )
            .with_patches(vec![StatePatch::AddCompletedAction("get_payment_methods".into())]);
            }
        };

    // 7. Fetch payment method
    let pm =
        match ctx.state.db.find_payment_method_by_id(&pm_id).await {
            Ok(Some(p)) if p.is_active => p,
            Ok(_) => {
                // method missing or inactive — same response as no method
                return ToolResult::ok(
                json!({
                    "items": [],
                    "note": "El proveedor no tiene métodos de pago configurados, deriva a humano."
                }),
                started,
            )
            .with_patches(vec![StatePatch::AddCompletedAction("get_payment_methods".into())]);
            }
            Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
        };

    // 8. Build response — never leak _id, owner_id, bActive, ni account_name
    //    (titular). El titular no se muestra al cliente: para Pago Móvil
    //    alcanza con banco + cédula + teléfono, y exponerlo invita al LLM a
    //    pintarlo en el mensaje.
    let response = json!({
        "items": [{
            "type": "pago_movil",
            "bank_name": pm.bank_name,
            "id_number": pm.id_number,
            "phone": pm.phone
        }],
        "note": "Pago Móvil. Pedile al cliente la referencia y comprobante después de pagar para llamar `report_payment`."
    });

    // 9. Cache result
    if let Ok(s) = serde_json::to_string(&response) {
        ctx.state
            .redis
            .set_ai_payment_methods_cache(&owner_id, &s, AI_PAYMENT_METHODS_CACHE_TTL_SECS)
            .await;
    }

    tracing::info!(
        "[ai_agent.get_payment_methods] live result for owner {}",
        owner_id
    );
    ToolResult::ok(response, started).with_patches(vec![
        StatePatch::AddCompletedAction("get_payment_methods".into()),
        StatePatch::SetCollectedData {
            key: "payment_methods_shown".into(),
            value: "true".into(),
        },
    ])
}

// ============================================
// Tool: list_banks
// ============================================

/// Carga la lista de bancos para lookup interno (cache Redis → DB fallback).
/// Devuelve `(ObjectId, bank_name, bank_code)`. La cache se popula como efecto
/// secundario en el cache miss para que llamadas siguientes sean baratas.
///
/// Usado por `exec_report_payment` para resolver `issuing_bank_id` cuando el
/// LLM manda un nombre/código en vez de un ObjectId hex (fallback robusto).
async fn load_banks_for_lookup(
    ctx: &ToolContext,
) -> Result<Vec<(ObjectId, String, String)>, String> {
    if let Some(cached_str) = ctx.state.redis.get_ai_list_banks_cache().await {
        if let Ok(cached_val) = serde_json::from_str::<Value>(&cached_str) {
            if let Some(items) = cached_val.get("items").and_then(|v| v.as_array()) {
                let parsed: Vec<(ObjectId, String, String)> = items
                    .iter()
                    .filter_map(|item| {
                        let id = item.get("id")?.as_str()?;
                        let oid = ObjectId::parse_str(id).ok()?;
                        let name = item.get("bank_name")?.as_str()?.to_string();
                        let code = item.get("bank_code")?.as_str()?.to_string();
                        Some((oid, name, code))
                    })
                    .collect();
                if !parsed.is_empty() {
                    return Ok(parsed);
                }
            }
        }
    }
    let banks = ctx
        .state
        .db
        .find_bank_list()
        .await
        .map_err(|e| format!("db_error:{}", e))?;
    let items: Vec<Value> = banks
        .iter()
        .map(|b| {
            json!({
                "id": b.id.to_hex(),
                "bank_name": b.bank_name,
                "bank_code": b.bank_code
            })
        })
        .collect();
    if let Ok(s) = serde_json::to_string(&json!({ "items": items })) {
        ctx.state.redis.set_ai_list_banks_cache(&s, 86_400).await;
    }
    Ok(banks
        .into_iter()
        .map(|b| (b.id, b.bank_name, b.bank_code))
        .collect())
}

async fn exec_list_banks(_args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    // 1. Sandbox short-circuit — return fixture banks before any DB call
    if ctx.is_sandbox {
        return ToolResult::ok(
            json!({
                "items": [
                    { "id": "000000000000000000000001", "bank_name": "Banesco", "bank_code": "0134" },
                    { "id": "000000000000000000000002", "bank_name": "Banco de Venezuela", "bank_code": "0102" },
                    { "id": "000000000000000000000003", "bank_name": "Mercantil", "bank_code": "0105" }
                ]
            }),
            started,
        );
    }

    // 2. Redis cache hit?
    if let Some(cached) = ctx.state.redis.get_ai_list_banks_cache().await {
        if let Ok(parsed_val) = serde_json::from_str::<Value>(&cached) {
            tracing::info!("[ai_agent.list_banks] cache hit");
            return ToolResult::ok(parsed_val, started);
        }
    }

    // 3. DB fetch
    let banks = match ctx.state.db.find_bank_list().await {
        Ok(b) => b,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    // 4. Build response — only id, bank_name, bank_code (lean context per spec LB-1)
    let items: Vec<Value> = banks
        .iter()
        .map(|b| {
            json!({
                "id": b.id.to_hex(),
                "bank_name": b.bank_name,
                "bank_code": b.bank_code
            })
        })
        .collect();

    let response = json!({ "items": items });

    // 5. Cache result — 24h TTL (catálogo BCV cambia rarísimo)
    if let Ok(s) = serde_json::to_string(&response) {
        ctx.state.redis.set_ai_list_banks_cache(&s, 86_400).await;
    }

    tracing::info!("[ai_agent.list_banks] live result, {} banks", banks.len());
    ToolResult::ok(response, started)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zone(
        display_name: &str,
        state: &str,
        municipality: &str,
        parish: Option<&str>,
        sector: Option<&str>,
        aliases: &[&str],
    ) -> CachedZone {
        CachedZone {
            display_name: display_name.to_string(),
            state: state.to_string(),
            municipality: municipality.to_string(),
            connection_types: vec![ConnectionType::Fibra],
            parish: parish.map(str::to_string),
            sector: sector.map(str::to_string),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn fixture_zones() -> Vec<CachedZone> {
        // Nota: estas zonas simulan zonas ya pre-filtradas con is_active=true
        // (load_active_zones usa list_ai_coverage_zones(true) — las inactivas
        // nunca llegan al matcher).
        vec![
            make_zone("Valencia Centro", "Carabobo", "Valencia", None, None, &[]),
            make_zone(
                "Naguanaguá",
                "Carabobo",
                "Naguanagua",
                None,
                None,
                &["Naguanagua", "Naguanagüa"],
            ),
            make_zone(
                "Loro Pedernales",
                "Carabobo",
                "Valencia",
                None,
                Some("Loro"),
                &[],
            ),
            make_zone("Las Vegas", "Carabobo", "Valencia", None, None, &[]),
            make_zone("Las Vegas Norte", "Miranda", "Baruta", None, None, &[]),
        ]
    }

    #[test]
    fn test_match_exact_display_name() {
        // "Valencia Centro" matchea por display_name exacto.
        // Nota: dado que "valencia" (municipio) está contenido en "valencia centro",
        // otras zonas de Valencia también pueden matchear — el algoritmo de
        // contains-bidireccional es por diseño (spec §4, capability 4).
        // Este test verifica que la zona con ese display_name esté en los resultados.
        let zones = fixture_zones();
        let q = normalize_zone("Valencia Centro");
        let result = match_zones(&zones, &q);
        assert!(
            !result.is_empty(),
            "Debe haber al menos un match para 'Valencia Centro'"
        );
        assert!(
            result.iter().any(|z| z.display_name == "Valencia Centro"),
            "La zona con display_name 'Valencia Centro' debe estar en los resultados"
        );
    }

    #[test]
    fn test_match_unique_display_name() {
        // Zona con display_name único que no comparte substrings con otras.
        // Usar "Naguanaguá" — no es municipio de ninguna otra zona del fixture.
        let zones = vec![
            make_zone(
                "Naguanaguá",
                "Carabobo",
                "Naguanagua",
                None,
                None,
                &["Naguanagua"],
            ),
            make_zone("Valencia Sur", "Carabobo", "Valencia", None, None, &[]),
        ];
        let q = normalize_zone("Naguanaguá");
        let result = match_zones(&zones, &q);
        assert_eq!(result.len(), 1, "Debe ser un match único para Naguanaguá");
        assert_eq!(result[0].display_name, "Naguanaguá");
    }

    #[test]
    fn test_match_alias() {
        let zones = fixture_zones();
        let q = normalize_zone("Naguanagua");
        let result = match_zones(&zones, &q);
        // Debe matchear por alias "Naguanagua" (normalizado == normalizado del display_name también)
        assert!(!result.is_empty());
        assert_eq!(result[0].display_name, "Naguanaguá");
    }

    #[test]
    fn test_match_sector_substring() {
        let zones = fixture_zones();
        let q = normalize_zone("Loro");
        let result = match_zones(&zones, &q);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].display_name, "Loro Pedernales");
    }

    #[test]
    fn test_no_match_returns_empty() {
        let zones = fixture_zones();
        let q = normalize_zone("El Limón");
        let result = match_zones(&zones, &q);
        assert!(result.is_empty());
    }

    #[test]
    fn test_disambiguation_two_matches() {
        let zones = fixture_zones();
        let q = normalize_zone("Las Vegas");
        let result = match_zones(&zones, &q);
        assert_eq!(
            result.len(),
            2,
            "Debe haber ambigüedad con dos zonas 'Las Vegas'"
        );
    }

    #[test]
    fn test_state_match_yields_multiple() {
        let zones = fixture_zones();
        // "carabobo" matchea todas las zonas con state="Carabobo"
        let q = normalize_zone("Carabobo");
        let result = match_zones(&zones, &q);
        // Deben ser 4 (todas las de Carabobo)
        assert!(
            result.len() > 1,
            "Estado 'Carabobo' debe matchear múltiples zonas → disambiguation"
        );
    }

    #[test]
    fn test_sector_specificity_beats_shared_parish() {
        // Caso real prod: dos zonas en el mismo parish "Güigüe", una con
        // sector "Centro Güigüe" y otra con sector "Loro Pedernales".
        // Cliente dice "centro de güigüe" — sólo Centro Güigüe debe matchear.
        let zones = vec![
            make_zone(
                "Pedernales",
                "Carabobo",
                "Carlos Arvelo",
                Some("Güigüe"),
                Some("Loro Pedernales"),
                &[],
            ),
            make_zone(
                "Carlos Arvelo",
                "Carabobo",
                "Carlos Arvelo",
                Some("Güigüe"),
                Some("Centro Güigüe"),
                &[],
            ),
        ];
        let q = normalize_zone("centro de Güigüe");
        let result = match_zones(&zones, &q);
        assert_eq!(
            result.len(),
            1,
            "Sólo la zona con sector 'Centro Güigüe' debe matchear"
        );
        assert_eq!(result[0].sector.as_deref(), Some("Centro Güigüe"));
    }

    #[test]
    fn test_shared_parish_alone_yields_disambiguation() {
        // Mismo escenario, pero el cliente sólo dice "güigüe" sin discriminador.
        // Ambas zonas matchean al mismo tier (PARISH) → disambiguation.
        let zones = vec![
            make_zone(
                "Pedernales",
                "Carabobo",
                "Carlos Arvelo",
                Some("Güigüe"),
                Some("Loro Pedernales"),
                &[],
            ),
            make_zone(
                "Carlos Arvelo",
                "Carabobo",
                "Carlos Arvelo",
                Some("Güigüe"),
                Some("Centro Güigüe"),
                &[],
            ),
        ];
        let q = normalize_zone("güigüe");
        let result = match_zones(&zones, &q);
        assert_eq!(
            result.len(),
            2,
            "Sin discriminador, ambas zonas en parish 'Güigüe' deben matchear"
        );
    }

    #[test]
    fn test_zone_must_explain_all_query_tokens() {
        // Caso real prod: cliente dice "centro de Güigüe".
        // Zone X tiene sector="Centro" en municipio "Libertador" — su
        // fingerprint NO contiene "guigue". Aunque su sector matchee el
        // token "centro" de la query, debe ser RECHAZADA porque la zona
        // no explica el token "guigue" que el cliente dijo.
        // Zone Y tiene sector="Centro Güigüe" — explica ambos tokens.
        let zones = vec![
            make_zone(
                "Libertador",
                "Carabobo",
                "Libertador",
                None,
                Some("Centro"),
                &[],
            ),
            make_zone(
                "Carlos Arvelo",
                "Carabobo",
                "Carlos Arvelo",
                Some("Güigüe"),
                Some("Centro Güigüe"),
                &[],
            ),
        ];
        let q = normalize_zone("centro de Güigüe");
        let result = match_zones(&zones, &q);
        assert_eq!(
            result.len(),
            1,
            "Sólo la zona que explica TODOS los tokens del cliente debe matchear"
        );
        assert_eq!(result[0].sector.as_deref(), Some("Centro Güigüe"));
    }

    #[test]
    fn test_generic_centro_alone_yields_disambiguation() {
        // Si el cliente dice solo "centro" sin discriminador, ambas zonas
        // con "centro" en algún campo deben matchear → disambiguation.
        let zones = vec![
            make_zone(
                "Libertador",
                "Carabobo",
                "Libertador",
                None,
                Some("Centro"),
                &[],
            ),
            make_zone(
                "Carlos Arvelo",
                "Carabobo",
                "Carlos Arvelo",
                Some("Güigüe"),
                Some("Centro Güigüe"),
                &[],
            ),
        ];
        let q = normalize_zone("centro");
        let result = match_zones(&zones, &q);
        assert_eq!(result.len(), 2, "Cliente dice solo 'centro' → ambas zonas con sector centro deben matchear → disambiguation");
    }

    #[test]
    fn test_full_sector_query_matches_uniquely() {
        // Cliente dice el sector exacto.
        let zones = vec![
            make_zone(
                "Pedernales",
                "Carabobo",
                "Carlos Arvelo",
                Some("Güigüe"),
                Some("Loro Pedernales"),
                &[],
            ),
            make_zone(
                "Carlos Arvelo",
                "Carabobo",
                "Carlos Arvelo",
                Some("Güigüe"),
                Some("Centro Güigüe"),
                &[],
            ),
        ];
        let q = normalize_zone("Loro Pedernales");
        let result = match_zones(&zones, &q);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].sector.as_deref(), Some("Loro Pedernales"));
    }

    // ─── Fuzzy matching: typos comunes en WhatsApp venezolano ─────────────

    #[test]
    fn test_fuzzy_plural_suffix() {
        // Cliente escribe "Carlos Arvelos" (con "s" extra), DB tiene "Carlos Arvelo"
        let zones = vec![make_zone(
            "Carlos Arvelo",
            "Carabobo",
            "Carlos Arvelo",
            Some("Güigüe"),
            Some("Centro Güigüe"),
            &[],
        )];
        let q = normalize_zone("Carlos Arvelos");
        let result = match_zones(&zones, &q);
        assert_eq!(result.len(), 1, "Plural 'Arvelos' debe matchear 'Arvelo'");
    }

    #[test]
    fn test_fuzzy_transposition_in_sector() {
        // Bug real reportado: sector en DB tiene typo "Centro Gugiue"
        // (i↔u swap) y cliente escribe "centro de guigue".
        let zones = vec![make_zone(
            "Carlos Arvelo",
            "Carabobo",
            "Carlos Arvelo",
            Some("Guigue"),
            Some("Centro Gugiue"),
            &[],
        )];
        let q = normalize_zone("centro de guigue");
        let result = match_zones(&zones, &q);
        assert_eq!(result.len(), 1, "guigue debe matchear gugiue (1-2 typos)");
    }

    #[test]
    fn test_fuzzy_query_with_geographic_stopwords() {
        // Cliente escribe "centro de guigue municip carlos arvelos" — la
        // palabra "municip" debe ignorarse como stopword estructural.
        let zones = vec![make_zone(
            "Carlos Arvelo",
            "Carabobo",
            "Carlos Arvelo",
            Some("Guigue"),
            Some("Centro Guigue"),
            &[],
        )];
        let q = normalize_zone("centro de guigue municip carlos arvelos");
        let result = match_zones(&zones, &q);
        assert!(
            !result.is_empty(),
            "Query con 'municip' (stopword) + 'arvelos' (plural) debe matchear"
        );
    }

    #[test]
    fn test_fuzzy_does_not_match_unrelated() {
        // Falsa amistad: "Valencia" vs "Valera" — son distintas zonas.
        let zones = vec![
            make_zone("Valencia Centro", "Carabobo", "Valencia", None, None, &[]),
            make_zone("Valera", "Trujillo", "Valera", None, None, &[]),
        ];
        let q = normalize_zone("valencia");
        let result = match_zones(&zones, &q);
        assert_eq!(
            result.len(),
            1,
            "valencia NO debe matchear valera por distancia (lev=3)"
        );
        assert_eq!(result[0].display_name, "Valencia Centro");
    }

    #[test]
    fn test_fuzzy_token_match_thresholds() {
        // Tests directos sobre la función helper.
        assert!(fuzzy_token_match("arvelo", "arvelo"), "exacto");
        assert!(fuzzy_token_match("arvelos", "arvelo"), "plural sufijo");
        assert!(
            fuzzy_token_match("arvelo", "arvelos"),
            "plural sufijo invertido"
        );
        assert!(fuzzy_token_match("guigue", "gugiue"), "transposición lev=2");
        assert!(
            fuzzy_token_match("carabobo", "carabovo"),
            "1 typo en palabra larga"
        );
        assert!(
            !fuzzy_token_match("loro", "lora"),
            "tokens cortos solo exacto"
        );
        assert!(
            !fuzzy_token_match("valencia", "valera"),
            "distancia 3 = no match"
        );
        assert!(
            !fuzzy_token_match("abc", "abd"),
            "menos de 4 chars = no fuzzy"
        );
    }
}

// ============================================
// Tool: transfer_to_agent
// ============================================

#[derive(Deserialize)]
struct TransferAgentArgs {
    target_agent_id: String,
    #[serde(default)]
    reason: Option<String>,
}

async fn exec_transfer_to_agent(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    let parsed: TransferAgentArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };

    let target_oid = match ObjectId::parse_str(parsed.target_agent_id.trim()) {
        Ok(o) => o,
        Err(_) => return ToolResult::err("invalid_target_agent_id", started),
    };

    if target_oid == ctx.agent_id {
        return ToolResult::err("target_is_self", started);
    }

    if !ctx.allowed_transfer_targets.contains(&target_oid) {
        return ToolResult::err("target_not_in_allowlist", started);
    }

    // Validar que el agente destino existe (puede haberse borrado entre
    // configurar y ejecutar). Si no existe, no transferimos.
    let target = match ctx.state.db.find_ai_agent_by_id(&target_oid).await {
        Ok(Some(a)) => a,
        Ok(None) => return ToolResult::err("target_agent_not_found", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    let reason = parsed.reason.unwrap_or_default();

    // Sin conv asociada (endpoint sandbox de prueba "en frío") devolvemos
    // respuesta sintética. En agente Shadow CON conv real SÍ persistimos el
    // routing — transfer es decisión de routing pura, no genera outbound al
    // cliente. Esto permite simular el handoff a Carla/Gabriel/Andrea en
    // Shadow y ver el siguiente turno atendido por el target.
    if ctx.conversation_id.is_none() {
        return ToolResult::ok(
            json!({
                "ok": true,
                "mode": "sandbox",
                "target_agent_id": target_oid.to_hex(),
                "target_label": target.label,
                "reason": reason,
            }),
            started,
        );
    }

    let conv_id = ctx.conversation_id.unwrap();

    // Distinguir mismo vs otro workspace. Si el target atiende este mismo
    // workspace, es un handoff interno (silencioso) — el siguiente turno lo
    // genera el target en la misma conv. Si NO atiende este workspace, NO se
    // puede transferir la conv (cliente está chateando contra otro número de
    // WhatsApp): le decimos al cliente que escriba al número del target.
    let same_workspace = target.workspace_ids.contains(&ctx.workspace_id);

    if !same_workspace {
        // Cross-workspace: NO tocamos `ai_active_agent_id` de esta conv. El
        // cliente sigue acá hasta que decida moverse. Buscamos el wa_settings
        // del primer workspace del target para devolver el número.
        let target_workspace = target.workspace_ids.first().copied();
        let (target_phone, target_workspace_name) = match target_workspace {
            Some(wid) => match ctx.state.db.find_wa_settings_by_id(&wid).await {
                Ok(Some(s)) => (Some(s.phone), Some(s.workspace_name)),
                _ => (None, None),
            },
            None => (None, None),
        };

        let phone_pretty = target_phone
            .as_deref()
            .map(format_phone_pretty)
            .unwrap_or_else(|| "(número no disponible)".to_string());
        let area = if !target.label.trim().is_empty() {
            target.label.clone()
        } else {
            "el área correspondiente".to_string()
        };
        let client_message = format!(
            "Por temas de {} te atienden mejor desde nuestro número {}. Escribinos por allá y te respondemos enseguida.",
            area, phone_pretty
        );

        return ToolResult::ok(
            json!({
                "ok": true,
                "mode": "cross_workspace",
                "target_agent_id": target_oid.to_hex(),
                "target_label": target.label,
                "target_workspace_name": target_workspace_name,
                "target_phone": target_phone,
                "client_message": client_message,
                "reason": reason,
            }),
            started,
        )
        .with_patches(vec![
            StatePatch::AddCompletedAction("transfer_to_agent".into()),
            StatePatch::SetCurrentStep("cross_workspace_redirect".into()),
        ]);
    }

    // Mismo workspace: persistir routing para que el próximo turno (en la
    // misma conv) lo atienda el target. El dispatch además re-corre el turno
    // EN MEMORIA con el target para que el cliente reciba directo la respuesta
    // del agente destino sin tener que enviar otro mensaje.
    let trimmed_reason = reason.trim();
    let ctx_to_persist: Option<&str> = if trimmed_reason.is_empty() {
        None
    } else {
        Some(trimmed_reason)
    };
    let patch = ConversationAiPatch {
        ai_active_agent_id: Some(Some(&target_oid)),
        ai_disabled: Some(false),
        ai_transfer_context: Some(ctx_to_persist),
    };
    if let Err(e) = ctx
        .state
        .db
        .update_conversation_ai_state(&conv_id, patch)
        .await
    {
        return ToolResult::err(format!("db_error:{}", e), started);
    }

    // Reset de counters per-conv: el agente destino debe arrancar limpio,
    // sin heredar `no_resolution`, `id_attempts`, `turns_conv` del origen.
    // Lo hacemos acá (en el tool) en vez del dispatch para que el reset
    // ocurra SIEMPRE que se persiste el handoff — incluso si el chain en
    // memoria falla a mitad por error transient de Gemini, el target ya
    // tiene counters limpios cuando el cliente reescriba.
    ctx.state
        .redis
        .clear_ai_conv_counters(&conv_id.to_hex())
        .await;

    let event = crate::modules::whatsapp::ws::WsServerEvent::IaReactivada {
        conversation_id: conv_id.to_hex(),
        reason: "transfer_to_agent".to_string(),
        by: "ai_agent".to_string(),
        to_agent_id: Some(target_oid.to_hex()),
    };
    crate::modules::whatsapp::ws::broadcast_all(&ctx.state.ws_registry, &event).await;

    let step = format!("transferred_to_{}", slugify_label(&target.label));
    ToolResult::ok(
        json!({
            "ok": true,
            "mode": "live",
            "target_agent_id": target_oid.to_hex(),
            "target_label": target.label,
            "reason": reason,
        }),
        started,
    )
    .with_patches(vec![
        StatePatch::AddCompletedAction("transfer_to_agent".into()),
        StatePatch::SetCurrentStep(step),
    ])
}

/// "584125403745" → "+58 412 540 3745". Defensivo: si el formato no matchea
/// devolvemos el original.
fn format_phone_pretty(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 12 && digits.starts_with("58") {
        format!("+58 {} {} {}", &digits[2..5], &digits[5..8], &digits[8..12])
    } else {
        format!("+{}", digits)
    }
}

// ============================================
// Tool: create_ticket
// ============================================

#[derive(Deserialize)]
struct CreateTicketArgs {
    /// Si no viene, se usa `escalation.default_ticket_category_id` del agente.
    /// Si tampoco hay default, error `invalid_category`.
    #[serde(default)]
    category_id: Option<String>,
    reason: String,
    #[serde(default)]
    summary: Option<String>,
}

/// Catálogo conocido — copia local del que vive en `whatsapp/tickets.rs`. Si
/// se desincronizan, el tool acepta categorías que el resto del sistema no
/// reconoce. TODO: extraer a una const compartida en una iteración futura.
const KNOWN_CATEGORIES: &[(&str, &str)] = &[
    ("ventas_contrataciones", "Ventas y Contrataciones"),
    ("cobranzas_facturacion", "Cobranzas y Facturación"),
    ("gestion_planes", "Gestión de Planes"),
    ("bajas_retencion", "Bajas y Retención"),
    ("actualizacion_datos", "Actualización de Datos"),
    (
        "soporte_primer_segundo_nivel",
        "Soporte de Primer y Segundo Nivel",
    ),
    ("configuraciones_tecnicas", "Configuraciones Técnicas"),
    ("mantenimiento_red", "Mantenimiento de Red"),
    ("despacho_tecnico", "Despacho Técnico (Campo)"),
    ("aprovisionamiento", "Aprovisionamiento"),
];

fn category_label(id: &str) -> Option<&'static str> {
    KNOWN_CATEGORIES
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, l)| *l)
}

async fn exec_create_ticket(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    let parsed: CreateTicketArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };

    let reason = parsed.reason.trim().to_string();
    if reason.is_empty() {
        return ToolResult::err("reason_required", started);
    }
    let category_id = match parsed
        .category_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(c) => c.to_string(),
        None => match ctx.default_ticket_category_id.as_ref() {
            Some(d) if !d.trim().is_empty() => d.trim().to_string(),
            _ => return ToolResult::err("category_id_required", started),
        },
    };
    let label = match category_label(&category_id) {
        Some(l) => l.to_string(),
        None => return ToolResult::err(format!("invalid_category:{}", category_id), started),
    };

    // Sandbox: no persiste, devuelve fake id estable.
    if ctx.is_sandbox {
        return ToolResult::ok(
            json!({
                "ok": true,
                "mode": "sandbox",
                "ticket_id": "sandbox-fake-ticket",
                "category_id": category_id,
                "category_label": label,
                "summary": parsed.summary,
            }),
            started,
        );
    }

    // Live: requerimos conversation_id (sin él no podemos amarrar el ticket).
    let conv_id = match ctx.conversation_id {
        Some(c) => c,
        None => return ToolResult::err("conversation_id_missing", started),
    };

    // Snapshot del cliente — best effort.
    let conv_doc = match ctx.state.db.find_conversation_by_id(&conv_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return ToolResult::err("conversation_not_found", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    let now = BsonDateTime::now();
    let summary_note = parsed
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let timeline = vec![WaTicketTimelineEntry {
        action: "created".into(),
        actor_id: ctx.ai_user_id.clone(),
        actor_name: ctx.ai_user_name.clone(),
        from_status: None,
        to_status: Some("open".into()),
        assigned_to_id: None,
        assigned_to_name: None,
        note: summary_note.clone(),
        created_at: now,
    }];

    let ticket = WaTicket {
        id: None,
        conversation_id: conv_id,
        customer_phone: conv_doc.phone.clone(),
        customer_name: conv_doc.name.clone(),
        customer_id: conv_doc.client_id,
        business_phone: conv_doc.business_phone.clone(),
        created_by_id: ctx.ai_user_id.clone(),
        created_by_name: ctx.ai_user_name.clone(),
        assigned_to_id: None,
        assigned_to_name: None,
        category_id: Some(category_id.clone()),
        category_label: Some(label.clone()),
        reason,
        status: "open".into(),
        resolution: None,
        resolved_at: None,
        closed_at: None,
        transferred_from_id: None,
        transferred_from_name: None,
        idempotency_key: None,
        // Tag automática para tickets escalados por IA (visible en filtros).
        tags: vec!["escalado_ia".into()],
        created_at: now,
        updated_at: now,
        timeline,
    };

    let saved = match ctx.state.db.create_ticket(ticket).await {
        Ok(t) => t,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    // Cierre best-effort de la conv. PR 3 cubrirá broadcast WS / audit event.
    if conv_doc.status != "closed" {
        if let Err(e) = ctx.state.db.close_conversation(&conv_id).await {
            tracing::warn!("[ai_agent] close_conversation tras create_ticket: {}", e);
        }
    }

    let ticket_id = saved.id.map(|o| o.to_hex()).unwrap_or_default();
    ToolResult::ok(
        json!({
            "ok": true,
            "mode": "live",
            "ticket_id": ticket_id,
            "category_id": category_id,
            "category_label": label,
        }),
        started,
    )
    .with_patches(vec![
        StatePatch::AddCompletedAction("create_ticket".into()),
        StatePatch::SetCurrentStep("ticket_created".into()),
        StatePatch::SetCollectedData {
            key: "ticket_id".into(),
            value: ticket_id,
        },
    ])
}

// ============================================
// Tool: calculate_amount_bs
// ============================================

#[derive(Deserialize)]
struct CalculateAmountBsArgs {
    amount_usd: f64,
}

#[inline]
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

async fn exec_calculate_amount_bs(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    // 1. Parse args
    let parsed: CalculateAmountBsArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };
    let amount_usd = parsed.amount_usd;

    // 2. Validate amount
    if !(amount_usd > 0.0) {
        // catches 0, negatives, NaN
        return ToolResult::err("invalid_amount", started);
    }

    // 3. Resolve BCV rate (Redis → DB fallback)
    let rate: f64 = match ctx.state.redis.get_exchange_rate().await {
        Ok(Some(r)) => r,
        _ => match ctx.state.db.get_latest_exchange_rate().await {
            Ok(r) => r,
            Err(_) => return ToolResult::err("exchange_rate_unavailable", started),
        },
    };
    if rate == 0.0 {
        return ToolResult::err("exchange_rate_zero", started);
    }

    // 4. Resolve tax — misma lógica que `/v2/utils/calculate`: sin id_tax,
    //    `find_tax_by_id(None)` cae automáticamente a `sTarget = "DEFAULT"`.
    //    Usar siempre DEFAULT mantiene un único contrato con el endpoint público
    //    y evita drift cuando el admin cambia la configuración del IVA.
    let tax = match ctx.state.db.find_tax_by_id(None).await {
        Ok(Some(t)) => t,
        Ok(None) => return ToolResult::err("tax_config_missing", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let iva_factor = tax.iva;

    // 5. Compute — un solo monto final con IVA aplicado (mismo cálculo que el
    //    endpoint v2). Devolver dos amounts (base + with_iva) en el JSON
    //    confundía al LLM y mostraba el sin-IVA al cliente.
    let amount_bs = round2(amount_usd * rate * iva_factor);
    let iva_percent = round2((iva_factor - 1.0) * 100.0);

    // 6. Date stamp (Caracas TZ — coherente con la clave diaria del cron BCV).
    let rate_date = crate::utils::timezone::VenezuelaDateTime::now().date_string_venezuela();

    // 7. Result
    ToolResult::ok(
        json!({
            "amount_usd":  amount_usd,
            "amount_bs":   amount_bs,
            "bcv_rate":    rate,
            "rate_date":   rate_date,
            "iva_percent": iva_percent,
        }),
        started,
    )
    .with_patches(vec![StatePatch::AddCompletedAction(
        "calculate_amount_bs".into(),
    )])
}

// ============================================
// Tool: report_payment
// ============================================

#[derive(Deserialize)]
struct ReportPaymentArgs {
    client_id: String,
    reference: String,
    media_id: String,
    #[serde(default)]
    amount_bs: Option<f64>,
    #[serde(default)]
    amount_usd: Option<f64>,
    #[serde(default)]
    bank: Option<String>,
    #[serde(default)]
    issuing_bank_id: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    debt_id: Option<String>,
    #[serde(default)]
    payment_date: Option<String>,
}

// Crea un WaTicket en categoría `cobranzas_facturacion` sin cerrar la conversación.
// Devuelve el ObjectId del ticket guardado, o un String de error.
// IMPORTANTE: NO llama `close_conversation` — diferencia crítica vs `exec_create_ticket`.
async fn create_cobranzas_ticket_internal(
    ctx: &ToolContext,
    payment_report_id: &ObjectId,
    bank: &str,
    reference: &str,
    amount_bs: f64,
    amount_usd: f64,
) -> Result<ObjectId, String> {
    let conv_id = ctx
        .conversation_id
        .ok_or_else(|| "conversation_id_missing".to_string())?;

    let conv_doc = ctx
        .state
        .db
        .find_conversation_by_id(&conv_id)
        .await
        .map_err(|e| format!("db_error:{}", e))?
        .ok_or_else(|| "conversation_not_found".to_string())?;

    let report_id_hex = payment_report_id.to_hex();
    let bank_display = if bank.is_empty() {
        "(no informado)"
    } else {
        bank
    };
    let reason = format!(
        "Reporte de pago pendiente de validación. Banco: {}, Ref: {}, Monto: {} Bs / {} USD. PaymentReport ID: {}",
        bank_display, reference, amount_bs, amount_usd, report_id_hex
    );

    let now = BsonDateTime::now();
    let timeline = vec![WaTicketTimelineEntry {
        action: "created".into(),
        actor_id: ctx.ai_user_id.clone(),
        actor_name: ctx.ai_user_name.clone(),
        from_status: None,
        to_status: Some("open".into()),
        assigned_to_id: None,
        assigned_to_name: None,
        note: Some(format!(
            "Auto-creado por report_payment. PaymentReport: {}",
            report_id_hex
        )),
        created_at: now,
    }];

    let ticket = WaTicket {
        id: None,
        conversation_id: conv_id,
        customer_phone: conv_doc.phone.clone(),
        customer_name: conv_doc.name.clone(),
        customer_id: conv_doc.client_id,
        business_phone: conv_doc.business_phone.clone(),
        created_by_id: ctx.ai_user_id.clone(),
        created_by_name: ctx.ai_user_name.clone(),
        assigned_to_id: None,
        assigned_to_name: None,
        category_id: Some("cobranzas_facturacion".into()),
        category_label: Some("Cobranzas y Facturación".into()),
        reason,
        status: "open".into(),
        resolution: None,
        resolved_at: None,
        closed_at: None,
        transferred_from_id: None,
        transferred_from_name: None,
        idempotency_key: None,
        tags: vec![
            "escalado_ia".into(),
            "auto_payment_report".into(),
            format!("payment_report:{}", report_id_hex),
        ],
        created_at: now,
        updated_at: now,
        timeline,
    };

    let saved = ctx
        .state
        .db
        .create_ticket(ticket)
        .await
        .map_err(|e| format!("db_error:{}", e))?;

    saved.id.ok_or_else(|| "ticket_id_missing".to_string())
}

async fn exec_report_payment(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    use chrono::{DateTime, Utc};
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;
    use uuid::Uuid;

    use super::ai_agent_secret;
    use crate::crypto::aes::decrypt_payload;

    // 1. Parse args
    let parsed: ReportPaymentArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };

    // 2. Validate media_id non-empty
    if parsed.media_id.trim().is_empty() {
        return ToolResult::err("image_required", started);
    }

    // 2.b GUARDRAIL: media_id debe ser uno que el cliente haya enviado en
    // los mensajes recientes (evita que la IA invente un ID).
    if ctx.workspace_enable_guardrails && !ctx.is_sandbox {
        let mid = parsed.media_id.trim();
        if !ctx.recent_media_ids.iter().any(|m| m == mid) {
            return ToolResult::err("media_id_not_in_conversation", started);
        }
    }

    // 3. Normalize reference — extract canonical numeric run (WI-5)
    let reference = match crate::modules::ai_agent::reference_normalize::extract_canonical_reference(
        parsed.reference.trim(),
    ) {
        Some(r) => r,
        None => return ToolResult::err("reference_not_found_in_input", started),
    };

    // 4. Validate amount XOR (catches NaN via !(x > 0.0))
    let (amount_input_bs, amount_input_usd) = match (parsed.amount_bs, parsed.amount_usd) {
        (None, None) => return ToolResult::err("amount_required", started),
        (Some(_), Some(_)) => return ToolResult::err("amount_conflict", started),
        (Some(b), None) if b > 0.0 => (Some(b), None),
        (None, Some(u)) if u > 0.0 => (None, Some(u)),
        _ => return ToolResult::err("invalid_amount", started),
    };

    // 5. Sandbox short-circuit — AFTER validations, BEFORE any side effect
    if ctx.is_sandbox {
        return ToolResult::ok(
            json!({
                "ok": true,
                "mode": "sandbox",
                "payment_id": "sandbox-fake-payment",
                "ticket_id": "sandbox-fake-ticket",
                "warning": null,
                "already_registered": false,
                "amount_bs": amount_input_bs,
                "amount_usd": amount_input_usd,
                "exchange_rate": 0.0,
                "iva_rate": 1.0,
            }),
            started,
        );
    }

    // 6. Parse client_id
    let client_oid = match ObjectId::parse_str(parsed.client_id.trim()) {
        Ok(o) => o,
        Err(_) => return ToolResult::err("invalid_client_id", started),
    };

    // 6b. Parse + validate issuing_bank_id (WI-3)
    // El LLM debería pasar el ObjectId hex devuelto por `list_banks`. Pero
    // gpt-4o-mini ocasionalmente manda el nombre del banco como string
    // (ej: "Banesco") porque lo lee del bank_name de get_payment_methods o
    // del comprobante. Si no parsea como ObjectId, hacemos fallback:
    // resolvemos por nombre/código contra el catálogo (cache → DB), match
    // único → usamos; ambiguo o ningún match → error rico con candidatos
    // para que el LLM pueda llamar list_banks o pedirle al cliente.
    let parsed_issuing_bank_oid: Option<ObjectId> = match parsed
        .issuing_bank_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => match ObjectId::parse_str(s) {
            Ok(oid) => Some(oid),
            Err(_) => {
                let banks = match load_banks_for_lookup(ctx).await {
                    Ok(b) => b,
                    Err(e) => return ToolResult::err(e, started),
                };
                let needle = s.to_lowercase();
                let exact: Vec<&(ObjectId, String, String)> = banks
                    .iter()
                    .filter(|(_, name, code)| {
                        name.to_lowercase() == needle || code.to_lowercase() == needle
                    })
                    .collect();
                let candidates: Vec<&(ObjectId, String, String)> = if !exact.is_empty() {
                    exact
                } else {
                    banks
                        .iter()
                        .filter(|(_, name, code)| {
                            name.to_lowercase().contains(&needle)
                                || code.to_lowercase().contains(&needle)
                        })
                        .collect()
                };
                match candidates.len() {
                    1 => Some(candidates[0].0),
                    0 => {
                        let preview = banks
                            .iter()
                            .take(8)
                            .map(|(_, n, c)| format!("{} ({})", n, c))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return ToolResult::err(
                            format!(
                                "issuing_bank_not_recognized: input='{}'. \
                                 Llamá list_banks para ver todos los bancos disponibles \
                                 y pasá el id exacto en issuing_bank_id. \
                                 Algunos ejemplos: {}",
                                s, preview
                            ),
                            started,
                        );
                    }
                    _ => {
                        let listing = candidates
                            .iter()
                            .take(8)
                            .map(|(id, n, _)| format!("{}={}", id.to_hex(), n))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return ToolResult::err(
                            format!(
                                "issuing_bank_ambiguous: input='{}' matchea varios bancos. \
                                 Preguntale al cliente cuál es y pasá el id exacto. Candidatos: {}",
                                s, listing
                            ),
                            started,
                        );
                    }
                }
            }
        },
        None => None,
    };

    // 6c. When Some: verify the bank exists in ListBanks (via Redis cache → DB)
    if let Some(bank_oid) = parsed_issuing_bank_oid {
        let bank_exists = {
            // Try cache first
            let cached_banks = ctx.state.redis.get_ai_list_banks_cache().await;
            if let Some(cached_str) = cached_banks {
                if let Ok(cached_val) = serde_json::from_str::<Value>(&cached_str) {
                    if let Some(items) = cached_val.get("items").and_then(|v| v.as_array()) {
                        items.iter().any(|item| {
                            item.get("id")
                                .and_then(|id| id.as_str())
                                .map(|id_str| id_str == bank_oid.to_hex())
                                .unwrap_or(false)
                        })
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                // Cache miss — load from DB and cache for next time
                match ctx.state.db.find_bank_list().await {
                    Ok(banks) => {
                        let items: Vec<Value> = banks
                            .iter()
                            .map(|b| {
                                json!({
                                    "id": b.id.to_hex(),
                                    "bank_name": b.bank_name,
                                    "bank_code": b.bank_code
                                })
                            })
                            .collect();
                        let payload = json!({ "items": items });
                        if let Ok(s) = serde_json::to_string(&payload) {
                            ctx.state.redis.set_ai_list_banks_cache(&s, 86_400).await;
                        }
                        banks.iter().any(|b| b.id == bank_oid)
                    }
                    Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
                }
            }
        };
        if !bank_exists {
            return ToolResult::err("issuing_bank_id_not_found", started);
        }
    }

    // 7. Find client (need id_tax).
    // NOTE: find_client_by_id returns Ok(fake_client) on "not found" — detect
    // by comparing returned _id to the queried _id.
    let client = match ctx.state.db.find_client_by_id(&client_oid.to_hex()).await {
        Ok(c) if c._id == client_oid => c,
        Ok(_) => return ToolResult::err("client_not_found", started),
        Err(e) => {
            tracing::warn!("[ai_agent.report_payment] find_client_by_id error: {}", e);
            return ToolResult::err("client_not_found", started);
        }
    };

    // 8. Parse optional debt_id and validate existence BEFORE idempotency check (W-1 fix)
    // Spec order: static arg validation (parse + existence) before live lookups (check_reference).
    let id_debt_oid: Option<ObjectId> = match parsed
        .debt_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(s) => match ObjectId::parse_str(s) {
            Ok(o) => Some(o),
            Err(_) => return ToolResult::err("invalid_debt_id", started),
        },
        None => None,
    };

    if let Some(ref debt_oid) = id_debt_oid {
        match ctx.state.db.find_debt_by_id(&debt_oid.to_hex()).await {
            Ok(Some(_)) => {} // exists, continue
            Ok(None) => return ToolResult::err("debt_id_not_found", started),
            Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
        }
    }

    // 9. Idempotency check — BEFORE any network or DB write
    match ctx
        .state
        .db
        .check_reference(&client_oid, &reference, parsed_issuing_bank_oid)
        .await
    {
        Ok(Some(match_info)) => {
            return ToolResult::ok(
                json!({
                    "ok": true,
                    "mode": "live",
                    "already_registered": true,
                    "source": match_info.source,
                    "is_same_client": match_info.is_same_client,
                    "matched_reference": match_info.s_reference,
                    "matched_state": match_info.s_state,
                    "matched_amount_bs": match_info.n_bs,
                    "matched_amount_usd": match_info.n_amount,
                }),
                started,
            )
            .with_patches(vec![StatePatch::SetCurrentStep(
                "payment_already_registered".into(),
            )]);
        }
        Ok(None) => {} // proceed
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    }

    // 9. Resolve owner → payment_method
    let owner = match ctx.state.db.find_client_owner_by_id(&client_oid).await {
        Ok(Some(o)) => o,
        Ok(None) => return ToolResult::err("payment_method_not_configured", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let user_info = match ctx
        .state
        .db
        .find_user_payment_info_by_id(&owner.id_owner)
        .await
    {
        Ok(Some(u)) => u,
        Ok(None) => return ToolResult::err("payment_method_not_configured", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let id_payment_method = match user_info.id_payment_method {
        Some(id) => id,
        None => return ToolResult::err("payment_method_not_configured", started),
    };

    // 10. Resolve exchange rate (Redis → DB fallback)
    let exchange_rate: f64 = match ctx.state.redis.get_exchange_rate().await {
        Ok(Some(r)) => r,
        _ => match ctx.state.db.get_latest_exchange_rate().await {
            Ok(r) => r,
            Err(_) => return ToolResult::err("exchange_rate_unavailable", started),
        },
    };
    if exchange_rate <= 0.0 {
        return ToolResult::err("exchange_rate_zero", started);
    }

    // 11. Resolve iva_rate (default 1.0 if id_tax missing/not found)
    let iva_rate: f64 = if let Some(tax_id) = client.id_tax {
        match ctx.state.db.find_tax_by_id(Some(tax_id)).await {
            Ok(Some(t)) => t.iva,
            _ => 1.0,
        }
    } else {
        1.0
    };

    // 12. Compute the missing amount
    let (amount_bs, amount_usd) = match (amount_input_bs, amount_input_usd) {
        (Some(bs), None) => {
            let bs_neto = bs / iva_rate;
            let usd = round2(bs_neto / exchange_rate);
            (round2(bs), usd)
        }
        (None, Some(usd)) => {
            let bs_neto = usd * exchange_rate;
            let bs = round2(bs_neto * iva_rate);
            (bs, round2(usd))
        }
        _ => unreachable!("amounts validated above"),
    };

    // 13. Resolve WaSettings → build WhatsAppService → download media
    let wa_settings = match ctx.state.db.find_wa_settings_by_id(&ctx.workspace_id).await {
        Ok(Some(s)) => s,
        _ => return ToolResult::err("wa_settings_not_found", started),
    };
    let token = match decrypt_payload(&ai_agent_secret(), &wa_settings.access_token) {
        Some(t) => t,
        None => return ToolResult::err("wa_token_decrypt_failed", started),
    };
    let mut svc = crate::modules::whatsapp::service::WhatsAppService::new(
        ctx.state.reqwest_client.clone(),
        wa_settings.phone_number_id.clone(),
        token,
    );
    if let (Some(url), Some(secret)) = (
        ctx.state.config.relay_url.as_ref(),
        ctx.state.config.relay_secret.as_ref(),
    ) {
        svc = svc.with_media_relay(crate::modules::whatsapp::service::MediaRelay {
            url: url.clone(),
            secret: secret.clone(),
        });
    }
    let (bytes, mime, _filename) = match svc.download_media(&parsed.media_id).await {
        Ok(t) => t,
        Err(e) => return ToolResult::err(format!("image_download_failed:{}", e), started),
    };
    if bytes.is_empty() {
        return ToolResult::err("image_empty", started);
    }

    // 14. Save to uploads/ (mirror payments::handler convention)
    let ext = match mime.as_str() {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "jpg",
    };
    let unique_name = format!("{}.{}", Uuid::new_v4(), ext);
    let file_path = format!("uploads/{}", unique_name);
    if let Err(e) = async {
        let mut file = File::create(&file_path).await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        Ok::<_, std::io::Error>(())
    }
    .await
    {
        return ToolResult::err(format!("image_save_failed:{}", e), started);
    }
    let image_url = format!("/uploads/{}", unique_name);

    // 15. Parse payment_date
    let payment_date: DateTime<Utc> = parsed
        .payment_date
        .as_deref()
        .and_then(|d| d.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    // 16. Build PaymentReport
    // Clone reference and bank before they move into the report struct so we
    // can pass them to create_cobranzas_ticket_internal afterwards.
    let ref_for_ticket = reference.clone();
    let bank_for_ticket = parsed.bank.clone().unwrap_or_default();
    let report = crate::models::payment::PaymentReport {
        id: None,
        id_client: Some(client_oid),
        id_debt: id_debt_oid,
        id_payment_method: Some(id_payment_method),
        reference: reference.clone(),
        payment_date,
        amount_bs,
        bank_origin: parsed.bank.unwrap_or_default(),
        phone_number: parsed.phone.unwrap_or_default(),
        image_url,
        amount_usd,
        exchange_rate,
        state: "Pendiente".to_string(),
        rejection_reason: None,
        id_creator: Some(ctx.ai_user_id.clone()),
        id_issuing_bank: parsed_issuing_bank_oid,
        created_at: Utc::now(),
    };

    // 17. Persist
    let inserted = match ctx.state.db.create_payment_report(report).await {
        Ok(r) => r,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let report_oid = inserted.inserted_id.as_object_id().unwrap_or_default();
    let payment_id = report_oid.to_hex();

    // 18. Best-effort ticket creation in cobranzas_facturacion.
    // If the PaymentReport was saved successfully but ticket creation fails, we
    // return ok=true with ticket_id=null and a warning. The orphaned PaymentReport
    // (state="Pendiente") stays in DB for manual recovery — no rollback attempted.
    let (ticket_id, ticket_warning) = match create_cobranzas_ticket_internal(
        ctx,
        &report_oid,
        &bank_for_ticket,
        &ref_for_ticket,
        amount_bs,
        amount_usd,
    )
    .await
    {
        Ok(tid) => (Some(tid.to_hex()), None::<String>),
        Err(e) => {
            tracing::warn!(
                "[ai_agent.report_payment] ticket auto-create failed (PaymentReport {} created): {}",
                payment_id, e
            );
            (None::<String>, Some("ticket_creation_failed".to_string()))
        }
    };

    ToolResult::ok(
        json!({
            "ok": true,
            "mode": "live",
            "payment_id": payment_id,
            "ticket_id": ticket_id,
            "warning": ticket_warning,
            "already_registered": false,
            "amount_bs": amount_bs,
            "amount_usd": amount_usd,
            "exchange_rate": exchange_rate,
            "iva_rate": iva_rate,
            "is_advance": id_debt_oid.is_none(),
        }),
        started,
    )
    .with_patches(vec![
        StatePatch::AddCompletedAction("report_payment".into()),
        StatePatch::SetCurrentStep("payment_reported".into()),
    ])
}
