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
    db::{ProfileRepository, SalesRepository, WaTicketRepository, WhatsAppRepository},
    models::{
        ai_agent::{AiInvoice, AiToolConfig},
        whatsapp::{WaTicket, WaTicketTimelineEntry},
    },
    state::AppState,
};

use super::gemini::FunctionDeclaration;

// ============================================
// Contexto + resultado
// ============================================

/// Contexto compartido para la ejecución de tools dentro de un turno.
///
/// `workspace_id` y `business_phone` no se leen en PR 2 (sandbox no necesita
/// emitir WS ni resolver workspace por phone), pero el dispatch real de PR 3
/// los va a usar para audit events y para enrutar mensajes outbound.
#[derive(Clone)]
pub struct ToolContext {
    pub state: Arc<AppState>,
    #[allow(dead_code)]
    pub workspace_id: ObjectId,
    #[allow(dead_code)]
    pub business_phone: String,
    /// Conversación origen del turno. `None` cuando estamos en sandbox sin
    /// conv asociada — `create_ticket` devuelve fake en ese caso.
    pub conversation_id: Option<ObjectId>,
    /// UUID del AI user (creador atribuido en mensajes/tickets/audit).
    pub ai_user_id: String,
    pub ai_user_name: String,
    /// Cuando `true`: tools de escritura no persisten ni emiten WS — devuelven
    /// un payload sintético para que el loop pueda continuar.
    pub is_sandbox: bool,
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

const T_LOOKUP_CUSTOMER: &str = "lookup_customer";
const T_GET_INVOICES: &str = "get_invoices";
const T_REQUEST_HUMAN: &str = "request_human";
const T_CREATE_TICKET: &str = "create_ticket";

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
        _ => None,
    }
}

/// Construye los `FunctionDeclaration` que viajan a Gemini. Filtra por
/// `enabled = true` y aplica `description_override` cuando esté seteado.
pub fn build_function_declarations(tools: &[AiToolConfig]) -> Vec<FunctionDeclaration> {
    tools
        .iter()
        .filter(|t| t.enabled)
        .filter_map(|t| {
            tool_default(&t.name).map(|(default_desc, params)| FunctionDeclaration {
                name: t.name.clone(),
                description: t
                    .description_override
                    .clone()
                    .unwrap_or_else(|| default_desc.to_string()),
                parameters: params,
            })
        })
        .collect()
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
    let parsed: RequestHumanArgs = serde_json::from_value(args).unwrap_or(RequestHumanArgs { reason: None });
    let reason = parsed.reason.unwrap_or_default();

    // En sandbox: respuesta sintética, sin tocar DB.
    // En real (PR 3): cambiaría status=pending + assigned_to=null + emitiría
    // CHAT_TRANSFERIDO. PR 2 corre solo desde sandbox, así que no implementamos
    // el path real todavía — el loop ya marca `escalated=true` igual cuando se
    // pide este tool.
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

    // PR 3 implementará la transición real. Por ahora no rompemos.
    ToolResult::ok(
        json!({
            "ok": true,
            "mode": "live_stub",
            "reason": reason,
            "note": "El handoff real llega en PR 3. La IA debe igualmente terminar el turno."
        }),
        started,
    )
}

// ============================================
// Tool: create_ticket
// ============================================

#[derive(Deserialize)]
struct CreateTicketArgs {
    category_id: String,
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
    let label = match category_label(&parsed.category_id) {
        Some(l) => l.to_string(),
        None => return ToolResult::err(format!("invalid_category:{}", parsed.category_id), started),
    };

    // Sandbox: no persiste, devuelve fake id estable.
    if ctx.is_sandbox {
        return ToolResult::ok(
            json!({
                "ok": true,
                "mode": "sandbox",
                "ticket_id": "sandbox-fake-ticket",
                "category_id": parsed.category_id,
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
        category_id: Some(parsed.category_id.clone()),
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
            "category_id": parsed.category_id,
            "category_label": label,
        }),
        started,
    )
}
