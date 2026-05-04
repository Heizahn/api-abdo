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

use std::sync::Arc;
use std::time::Instant;

use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    db::{
        AiAgentRepository, ConversationAiPatch, ProfileRepository, SalesRepository,
        WaTicketRepository, WhatsAppRepository,
    },
    models::{
        ai_agent::{AiAgent, AiAgentMode, AiInvoice, AiToolConfig},
        whatsapp::{WaTicket, WaTicketTimelineEntry},
    },
    state::AppState,
};

use super::escalation;

use super::gemini::FunctionDeclaration;

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
}

impl ToolResult {
    fn ok(data: Value, started: Instant) -> Self {
        ToolResult {
            success: true,
            data,
            error: None,
            duration_ms: started.elapsed().as_millis() as u32,
        }
    }
    fn err(msg: impl Into<String>, started: Instant) -> Self {
        let m = msg.into();
        ToolResult {
            success: false,
            data: json!({ "error": &m }),
            error: Some(m),
            duration_ms: started.elapsed().as_millis() as u32,
        }
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

/// Segmento de IVA aplicado por el tool `calculate_amount_bs`. Hardcoded
/// porque hoy todos los quotes públicos por WhatsApp se cotizan en
/// EMPRESARIAL. Para cambiar a multi-segmento, abrir un change separado.
const TAX_TARGET_EMPRESARIAL: &str = "EMPRESARIAL";

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
/// Al agregar una tool nueva, se debe categorizar en `tool_category` en el
/// mismo PR. El default safe es `InfoLookup`, pero el `tracing::warn!` en el
/// arm `unknown =>` asegura visibilidad en logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    InfoLookup,
    Action,
}

/// Mapea el `tool_name` al ToolCategory. Default safe: `InfoLookup` para
/// nombres desconocidos (preserva el comportamiento "skip" actual y emite
/// `warn!` para que el dev categorice la tool nueva explícitamente).
pub fn tool_category(tool_name: &str) -> ToolCategory {
    match tool_name {
        T_LOOKUP_CUSTOMER
        | T_LIST_PLANS
        | T_CHECK_COVERAGE
        | T_GET_INVOICES
        | T_CALCULATE_AMOUNT_BS => ToolCategory::InfoLookup,

        T_CREATE_TICKET
        | T_REQUEST_HUMAN
        | T_TRANSFER_AGENT
        | T_REPORT_PAYMENT => ToolCategory::Action,

        unknown => {
            tracing::warn!(
                "[ai_agent.tools] tool_category: unknown tool name '{}' — defaulting to InfoLookup. Add explicit categorization in tools.rs.",
                unknown
            );
            ToolCategory::InfoLookup
        }
    }
}

/// Cache TTL para `list_plans` y `check_coverage`. Admins editan poco; en cada
/// write se invalida explícitamente.
const AI_BUSINESS_CACHE_TTL_SECS: u64 = 300;

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
            "Obtiene las deudas/facturas activas o recientes del cliente. Usar después de \
             lookup_customer para responder consultas de saldo, monto a pagar o estado de \
             cobranza. NUNCA inventar números — siempre llamar este tool.",
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
            "Verifica si una zona/sector/municipio tiene cobertura. Llamar SIEMPRE cuando el cliente menciona dónde vive, antes de recomendar plan. \
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
            "Calcula cuánto sale en bolívares un monto en USD aplicando la tasa BCV \
             vigente más IVA del 16% (segmento EMPRESARIAL). Llamar SIEMPRE que el \
             cliente pregunte un precio en Bs — NUNCA inventes la tasa ni el total. \
             La respuesta incluye el desglose: tasa, base sin IVA y monto final con IVA.",
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
        T_REPORT_PAYMENT => Some((
            "Registra un reporte de pago del cliente (referencia + monto + comprobante). \
             PRECONDICIONES: (1) llamá `lookup_customer` ANTES y confirmá con el cliente \
             cuál servicio si hay varios. (2) Pedile la foto del comprobante por WhatsApp \
             — sin imagen el tool falla. (3) Pasá `amount_bs` O `amount_usd`, NUNCA ambos: \
             el sistema deriva el otro con la tasa BCV vigente.",
            json!({
                "type": "object",
                "properties": {
                    "client_id":    { "type": "string", "description": "ObjectId hex devuelto por lookup_customer." },
                    "reference":    { "type": "string", "description": "Referencia bancaria del comprobante." },
                    "media_id":     { "type": "string", "description": "ID del media de WhatsApp (foto del comprobante). Lo recibís en el contexto del mensaje del cliente." },
                    "amount_bs":    { "type": "number", "description": "Monto en bolívares. Mutuamente excluyente con amount_usd." },
                    "amount_usd":   { "type": "number", "description": "Monto en dólares. Mutuamente excluyente con amount_bs." },
                    "bank":         { "type": "string", "description": "Nombre del banco origen del pago. Opcional." },
                    "phone":        { "type": "string", "description": "Teléfono asociado al pago móvil. Opcional." },
                    "debt_id":      { "type": "string", "description": "ObjectId hex de la deuda específica si el cliente la mencionó. Opcional — si falta, el reporte queda como abono a cuenta." },
                    "payment_date": { "type": "string", "description": "Fecha del pago en RFC3339 (ej: 2026-05-04T15:30:00Z). Opcional — default: ahora." }
                },
                "required": ["client_id", "reference", "media_id"]
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
) -> Vec<FunctionDeclaration> {
    let allowed_transfer_targets = extract_allowed_transfer_targets(&agent.tools);
    agent
        .tools
        .iter()
        .filter(|t| t.enabled)
        .filter_map(|t| {
            // `transfer_to_agent` sin `allowed_targets` configurados =
            // tool inválido, no la mostramos a Gemini (la validación de
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
                FunctionDeclaration {
                    name: t.name.clone(),
                    description: t
                        .description_override
                        .clone()
                        .unwrap_or_else(|| default_desc.to_string()),
                    parameters,
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
        if let Some(target) = props.get_mut("target_agent_id").and_then(|v| v.as_object_mut()) {
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

    match ctx
        .state
        .db
        .find_clients_for_ai_lookup(phone, id)
        .await
    {
        Ok(items) => ToolResult::ok(json!({ "items": items }), started),
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

    // Cap a `limit` (default 5).
    let limit = parsed.limit.unwrap_or(5).max(1).min(20) as usize;
    let invoices: Vec<AiInvoice> = debts
        .into_iter()
        .take(limit)
        .map(|d| AiInvoice {
            id: d._id.to_hex(),
            amount: d.n_amount,
            reason: d.s_reason,
            state: d.s_state,
            due_date: d.d_creation.try_to_rfc3339_string().unwrap_or_default(),
        })
        .collect();

    ToolResult::ok(json!({ "items": invoices }), started)
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
}

// ============================================
// Tool: list_plans
// ============================================

async fn exec_list_plans(_args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    if let Some(cached) = ctx.state.redis.get_ai_plans_cache().await {
        if let Ok(parsed) = serde_json::from_str::<Value>(&cached) {
            return ToolResult::ok(parsed, started);
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
            })
        })
        .collect();
    let response = json!({
        "items": items,
        "price_note": "Los precios no se exponen al asistente. Si el cliente pide costo, indicar que el equipo comercial confirma el monto al cerrar la instalación.",
    });
    if let Ok(s) = serde_json::to_string(&response) {
        ctx.state.redis.set_ai_plans_cache(&s, AI_BUSINESS_CACHE_TTL_SECS).await;
    }
    ToolResult::ok(response, started)
}

// ============================================
// Tool: check_coverage
// ============================================

#[derive(Deserialize)]
struct CheckCoverageArgs {
    zone: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct CachedZone {
    name: String,
    region: String,
}

async fn load_active_zones(ctx: &ToolContext) -> Result<Vec<CachedZone>, String> {
    if let Some(cached) = ctx.state.redis.get_ai_coverage_cache().await {
        if let Ok(parsed) = serde_json::from_str::<Vec<CachedZone>>(&cached) {
            return Ok(parsed);
        }
    }
    let zones = ctx.state.db.list_ai_coverage_zones(true).await?;
    let cached: Vec<CachedZone> = zones
        .into_iter()
        .map(|z| CachedZone { name: z.name, region: z.region })
        .collect();
    if let Ok(s) = serde_json::to_string(&cached) {
        ctx.state.redis.set_ai_coverage_cache(&s, AI_BUSINESS_CACHE_TTL_SECS).await;
    }
    Ok(cached)
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

    let zones = match load_active_zones(ctx).await {
        Ok(z) => z,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };

    let normalized = normalize_zone(raw);
    let matched = zones.iter().find(|z| {
        let nz = normalize_zone(&z.name);
        normalized == nz || normalized.contains(&nz) || nz.contains(&normalized)
    });

    let (matched_zone, region) = match matched {
        Some(z) => (Some(z.name.clone()), Some(z.region.clone())),
        None => (None, None),
    };
    let available_zones: Vec<&str> = zones.iter().map(|z| z.name.as_str()).collect();

    ToolResult::ok(
        json!({
            "covered": matched.is_some(),
            "matched_zone": matched_zone,
            "queried_zone": raw,
            "region": region,
            "available_zones": available_zones,
        }),
        started,
    )
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
        );
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
    if let Err(e) = ctx.state.db.update_conversation_ai_state(&conv_id, patch).await {
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
}

/// "584125403745" → "+58 412 540 3745". Defensivo: si el formato no matchea
/// devolvemos el original.
fn format_phone_pretty(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 12 && digits.starts_with("58") {
        format!(
            "+58 {} {} {}",
            &digits[2..5],
            &digits[5..8],
            &digits[8..12]
        )
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
    ("soporte_primer_segundo_nivel", "Soporte de Primer y Segundo Nivel"),
    ("configuraciones_tecnicas", "Configuraciones Técnicas"),
    ("mantenimiento_red", "Mantenimiento de Red"),
    ("despacho_tecnico", "Despacho Técnico (Campo)"),
    ("aprovisionamiento", "Aprovisionamiento"),
];

fn category_label(id: &str) -> Option<&'static str> {
    KNOWN_CATEGORIES.iter().find(|(k, _)| *k == id).map(|(_, l)| *l)
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
    let category_id = match parsed.category_id.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
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
    let summary_note = parsed.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);

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

    ToolResult::ok(
        json!({
            "ok": true,
            "mode": "live",
            "ticket_id": saved.id.map(|o| o.to_hex()).unwrap_or_default(),
            "category_id": category_id,
            "category_label": label,
        }),
        started,
    )
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

async fn exec_calculate_amount_bs(
    args: Value,
    ctx: &ToolContext,
    started: Instant,
) -> ToolResult {
    // 1. Parse args
    let parsed: CalculateAmountBsArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };
    let amount_usd = parsed.amount_usd;

    // 2. Validate amount
    if !(amount_usd > 0.0) {  // catches 0, negatives, NaN
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

    // 4. Resolve EMPRESARIAL tax (NO DEFAULT fallback)
    let tax = match ctx.state.db.find_tax_by_target(TAX_TARGET_EMPRESARIAL).await {
        Ok(Some(t)) => t,
        Ok(None) => return ToolResult::err("tax_config_missing", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let iva_factor = tax.iva;

    // 5. Compute (no chained rounding)
    let bs_base     = round2(amount_usd * rate);
    let bs_with_iva = round2(amount_usd * rate * iva_factor);
    let iva_percent = round2((iva_factor - 1.0) * 100.0);

    // 6. Date stamp (Caracas TZ — coherente con la clave diaria del cron BCV).
    let rate_date = crate::utils::timezone::VenezuelaDateTime::now()
        .date_string_venezuela();

    // 7. Result
    ToolResult::ok(
        json!({
            "amount_usd": amount_usd,
            "bcv_rate": rate,
            "rate_date": rate_date,
            "iva_factor": iva_factor,
            "iva_percent": iva_percent,
            "amount_bs_base": bs_base,
            "amount_bs_with_iva": bs_with_iva,
        }),
        started,
    )
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
    phone: Option<String>,
    #[serde(default)]
    debt_id: Option<String>,
    #[serde(default)]
    payment_date: Option<String>,
}

async fn exec_report_payment(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    use chrono::{DateTime, Utc};
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;
    use uuid::Uuid;

    use crate::crypto::aes::decrypt_payload;
    use super::dispatch::ai_agent_secret;

    // 1. Parse args
    let parsed: ReportPaymentArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };

    // 2. Validate media_id non-empty
    if parsed.media_id.trim().is_empty() {
        return ToolResult::err("image_required", started);
    }

    // 3. Validate reference non-empty
    if parsed.reference.trim().is_empty() {
        return ToolResult::err("reference_required", started);
    }

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

    // 8. Idempotency check — BEFORE any network or DB write
    let trimmed_ref = parsed.reference.trim().to_string();
    match ctx.state.db.check_reference(&client_oid, &trimmed_ref).await {
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
            );
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
    let user_info = match ctx.state.db.find_user_payment_info_by_id(&owner.id_owner).await {
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
        ctx.state.config.wa_media_relay_url.as_ref(),
        ctx.state.config.wa_media_relay_secret.as_ref(),
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
        "image/png"  => "png",
        "image/webp" => "webp",
        "image/gif"  => "gif",
        _            => "jpg",
    };
    let unique_name = format!("{}.{}", Uuid::new_v4(), ext);
    let file_path = format!("uploads/{}", unique_name);
    if let Err(e) = async {
        let mut file = File::create(&file_path).await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        Ok::<_, std::io::Error>(())
    }.await {
        return ToolResult::err(format!("image_save_failed:{}", e), started);
    }
    let image_url = format!("/uploads/{}", unique_name);

    // 15. Parse optional debt_id and payment_date
    let id_debt_oid: Option<ObjectId> = match parsed.debt_id.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(s) => match ObjectId::parse_str(s) {
            Ok(o) => Some(o),
            Err(_) => return ToolResult::err("invalid_debt_id", started),
        },
        None => None,
    };
    let payment_date: DateTime<Utc> = parsed.payment_date
        .as_deref()
        .and_then(|d| d.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    // 16. Build PaymentReport
    let report = crate::models::payment::PaymentReport {
        id: None,
        id_client: Some(client_oid),
        id_debt: id_debt_oid,
        id_payment_method: Some(id_payment_method),
        reference: trimmed_ref,
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
        created_at: Utc::now(),
    };

    // 17. Persist
    let inserted = match ctx.state.db.create_payment_report(report).await {
        Ok(r) => r,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let payment_id = inserted
        .inserted_id
        .as_object_id()
        .map(|o| o.to_hex())
        .unwrap_or_default();

    ToolResult::ok(
        json!({
            "ok": true,
            "mode": "live",
            "payment_id": payment_id,
            "already_registered": false,
            "amount_bs": amount_bs,
            "amount_usd": amount_usd,
            "exchange_rate": exchange_rate,
            "iva_rate": iva_rate,
            "is_advance": id_debt_oid.is_none(),
        }),
        started,
    )
}
