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
