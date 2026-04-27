//! Tickets de soporte derivados de conversaciones WhatsApp.
//!
//! Un ticket es la unidad de seguimiento que un agente genera cuando una
//! conversación necesita escalado, revisión o trabajo asíncrono. Al crearlo,
//! la conversación origen se cierra automáticamente para que no quede
//! flotando en la cola del chat.
//!
//! Reglas de negocio (ver el plan en docs internos):
//! - Categorías hardcodeadas en `TICKET_CATEGORIES` (MVP, decisión #1).
//! - `POST /tickets` cierra la conversación referenciada (decisión #2). Si
//!   ya estaba `closed`, el cierre es no-op y el ticket se crea igual.
//! - `409 ticket_already_open` si existe un ticket `open|in_progress` para
//!   esa conversación (decisión #3); estados `resolved|closed|cancelled`
//!   permiten crear uno nuevo.
//! - `Idempotency-Key` opcional (decisión #4): el mismo agente con la misma
//!   key recibe el ticket original sin duplicar.
//! - Visibilidad por rol (decisión #5): SUPERADMIN ve todos; agentes ven
//!   los asignados a ellos o creados por ellos.
//! - WS `TICKET_ACTUALIZADO` scope (decisión #6): creador + asignado actual
//!   + SUPERADMIN. Asignados previos no.
//! - `POST /transfer-and-ticket` atómico (decisión #7): sustituye al
//!   patrón de dos llamadas desde el front.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Extension, Json,
};
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};
use serde::Deserialize;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{
        ProfileRepository, TicketActionUpdate, TicketListFilter, UserRepository,
        WaTicketRepository, WhatsAppRepository,
    },
    error::ApiError,
    models::whatsapp::{
        CreateTicketRequest, TicketCategoriesResponse, TicketCategoryItem, TicketItem,
        TicketResponse, TicketTimelineEntryItem, TicketsListResponse, TransferAndTicketData,
        TransferAndTicketRequest, TransferAndTicketResponse, UpdateTicketStatusRequest, WaTicket,
        WaTicketTimelineEntry,
    },
    state::AppState,
};

use super::handler::{build_conversation_item, iso8601_pub as iso8601, require_can_chat};
use super::ws::{send_to_user, WsServerEvent};

// ============================================
// CATÁLOGO DE CATEGORÍAS (MVP — hardcoded)
// ============================================
//
// 10 categorías agrupadas en 2 pilares:
// - `administration` — Ventas + Admin (contratos, dinero, datos del cliente).
// - `operators` — Soporte Técnico (red, equipos, despacho de campo).
//
// `target_roles` lista los `nRole` (f32) habilitados para ser asignados como
// dueños del ticket en esa categoría. SUPERADMIN (`0.0`) está en todas como
// fallback universal. El front filtra el picker; el back valida server-side
// en POST /tickets, PATCH transfer y POST /transfer-and-ticket.
//
// Mapping de roles (ver memoria `user_roles.md`):
//   0   superadmin
//   0.5 operador             → operators (soporte general)
//   1   contador              → administration (cobranzas/facturación)
//   1.5 contador mensajero    → administration (operativo)
//   2   supercajero           → administration (full admin)
//   3   proveedor             → ninguna (externo, no recibe tickets)
//   4   cajero                → administration (cobranzas + ventas)
//   5   instalador            → operators (despacho técnico/aprovisionamiento)

struct CategorySpec {
    id: &'static str,
    label: &'static str,
    department: &'static str,
    target_roles: &'static [f32],
}

const DEPT_ADMIN: &str = "administration";
const DEPT_OPS: &str = "operators";

const TICKET_CATEGORIES: &[CategorySpec] = &[
    // ─── Administración (Ventas + Admin) ─────────────────────────────────
    CategorySpec {
        id: "ventas_contrataciones",
        label: "Ventas y Contrataciones",
        department: DEPT_ADMIN,
        target_roles: &[0.0, 1.0, 2.0, 4.0],
    },
    CategorySpec {
        id: "cobranzas_facturacion",
        label: "Cobranzas y Facturación",
        department: DEPT_ADMIN,
        target_roles: &[0.0, 1.0, 1.5, 2.0, 4.0],
    },
    CategorySpec {
        id: "gestion_planes",
        label: "Gestión de Planes",
        department: DEPT_ADMIN,
        target_roles: &[0.0, 1.0, 2.0],
    },
    CategorySpec {
        id: "bajas_retencion",
        label: "Bajas y Retención",
        department: DEPT_ADMIN,
        target_roles: &[0.0, 1.0, 2.0],
    },
    CategorySpec {
        id: "actualizacion_datos",
        label: "Actualización de Datos",
        department: DEPT_ADMIN,
        target_roles: &[0.0, 1.0, 1.5, 2.0, 4.0],
    },
    // ─── Operadores (Soporte Técnico) ─────────────────────────────────────
    CategorySpec {
        id: "soporte_primer_segundo_nivel",
        label: "Soporte de Primer y Segundo Nivel",
        department: DEPT_OPS,
        target_roles: &[0.0, 0.5],
    },
    CategorySpec {
        id: "configuraciones_tecnicas",
        label: "Configuraciones Técnicas",
        department: DEPT_OPS,
        target_roles: &[0.0, 0.5, 5.0],
    },
    CategorySpec {
        id: "mantenimiento_red",
        label: "Mantenimiento de Red",
        department: DEPT_OPS,
        target_roles: &[0.0, 0.5],
    },
    CategorySpec {
        id: "despacho_tecnico",
        label: "Despacho Técnico (Campo)",
        department: DEPT_OPS,
        target_roles: &[0.0, 0.5, 5.0],
    },
    CategorySpec {
        id: "aprovisionamiento",
        label: "Aprovisionamiento",
        department: DEPT_OPS,
        target_roles: &[0.0, 0.5, 5.0],
    },
];

const REASON_MAX_LEN: usize = 500;
const TICKET_LIST_DEFAULT_LIMIT: i64 = 30;
const TICKET_LIST_MAX_LIMIT: i64 = 100;

/// Idempotency-Key header — máx 128 chars.
const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";
const IDEMPOTENCY_KEY_MAX_LEN: usize = 128;

fn find_category(id: &str) -> Option<&'static CategorySpec> {
    TICKET_CATEGORIES.iter().find(|c| c.id == id)
}

fn category_label(id: &str) -> Option<&'static str> {
    find_category(id).map(|c| c.label)
}

// ============================================
// HELPERS
// ============================================

/// Resuelve el `customer_name` para snapshot del ticket.
/// Prioridad: `Clients.sName` (vía `client_id` o `phone`) → `WaConversation.name` → None.
async fn resolve_customer_snapshot(
    state: &Arc<AppState>,
    conv: &crate::models::whatsapp::WaConversation,
) -> (Option<String>, Option<ObjectId>) {
    let mut name: Option<String> = None;
    let mut client_id = conv.client_id;

    if let Some(cid) = client_id {
        if let Ok(map) = state.db.get_client_names_by_ids(&[cid]).await {
            if let Some(n) = map.get(&cid) { name = Some(n.clone()); }
        }
    }
    if name.is_none() {
        if let Ok(map) = state.db.get_client_names_by_phones(&[conv.phone.clone()]).await {
            if let Some(n) = map.get(&conv.phone) { name = Some(n.clone()); }
        }
    }
    if name.is_none() {
        name = conv.name.clone();
    }
    if client_id.is_none() {
        // Si no estaba linkeado pero hay match por teléfono, no perdemos info:
        // dejamos `client_id = None` (se podría enlazar en una iteración futura).
        client_id = None;
    }
    (name, client_id)
}

fn ticket_to_item(t: WaTicket, include_timeline: bool) -> TicketItem {
    let resolution_time_seconds = match (t.resolved_at, t.closed_at) {
        (Some(r), _) => Some((r.timestamp_millis() - t.created_at.timestamp_millis()) / 1000),
        (None, Some(c)) => Some((c.timestamp_millis() - t.created_at.timestamp_millis()) / 1000),
        _ => None,
    };
    let timeline = if include_timeline {
        Some(t.timeline.into_iter().map(timeline_entry_to_item).collect())
    } else {
        None
    };
    TicketItem {
        id: t.id.map(|o| o.to_hex()).unwrap_or_default(),
        conversation_id: t.conversation_id.to_hex(),
        customer_phone: t.customer_phone,
        customer_name: t.customer_name,
        customer_id: t.customer_id.map(|o| o.to_hex()),
        business_phone: t.business_phone,
        created_by_id: t.created_by_id,
        created_by_name: t.created_by_name,
        assigned_to_id: t.assigned_to_id,
        assigned_to_name: t.assigned_to_name,
        category_id: t.category_id,
        category_label: t.category_label,
        reason: t.reason,
        status: t.status,
        resolution: t.resolution,
        resolved_at: t.resolved_at.map(iso8601),
        closed_at: t.closed_at.map(iso8601),
        resolution_time_seconds,
        transferred_from_id: t.transferred_from_id,
        transferred_from_name: t.transferred_from_name,
        created_at: iso8601(t.created_at),
        updated_at: iso8601(t.updated_at),
        timeline,
    }
}

fn timeline_entry_to_item(e: WaTicketTimelineEntry) -> TicketTimelineEntryItem {
    TicketTimelineEntryItem {
        action: e.action,
        actor_id: e.actor_id,
        actor_name: e.actor_name,
        from_status: e.from_status,
        to_status: e.to_status,
        assigned_to_id: e.assigned_to_id,
        assigned_to_name: e.assigned_to_name,
        note: e.note,
        created_at: iso8601(e.created_at),
    }
}

fn parse_oid(id: &str, field: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(id).map_err(|_| ApiError::ValidationError {
        code: "invalid_id".into(),
        field: field.into(),
        message: format!("'{}' no es un ObjectId válido", field),
    })
}

fn validate_reason(reason: &str) -> Result<String, ApiError> {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        return Err(ApiError::ValidationError {
            code: "missing_field".into(),
            field: "reason".into(),
            message: "El motivo es requerido".into(),
        });
    }
    if trimmed.chars().count() > REASON_MAX_LEN {
        return Err(ApiError::ValidationError {
            code: "field_too_long".into(),
            field: "reason".into(),
            message: format!("El motivo no puede superar {} caracteres", REASON_MAX_LEN),
        });
    }
    Ok(trimmed.to_string())
}

fn extract_idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let raw = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(s) = &raw {
        if s.len() > IDEMPOTENCY_KEY_MAX_LEN {
            return Err(ApiError::ValidationError {
                code: "idempotency_key_too_long".into(),
                field: "Idempotency-Key".into(),
                message: format!(
                    "Idempotency-Key no puede superar {} caracteres",
                    IDEMPOTENCY_KEY_MAX_LEN
                ),
            });
        }
    }
    Ok(raw)
}

/// Resuelve y valida la categoría: si viene una key inválida devuelve 422.
/// Si viene `None` o vacía, devuelve `(None, None)`.
fn resolve_category(
    raw: Option<&str>,
) -> Result<(Option<String>, Option<String>), ApiError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok((None, None)),
        Some(id) => match category_label(id) {
            Some(label) => Ok((Some(id.to_string()), Some(label.to_string()))),
            None => Err(ApiError::ValidationError {
                code: "invalid_category".into(),
                field: "category_id".into(),
                message: format!("Categoría desconocida: {}", id),
            }),
        },
    }
}

/// Resuelve `(name, role)` del agente destino. Útil cuando además del nombre
/// hay que validar que el rol esté permitido en `target_roles` de la categoría.
async fn resolve_assignee(
    state: &Arc<AppState>,
    user_id: &str,
) -> Result<(String, f32), ApiError> {
    let u = state
        .db
        .find_user_by_id(user_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::ValidationError {
            code: "user_not_found".into(),
            field: "assign_to_id".into(),
            message: format!("El usuario {} no existe", user_id),
        })?;
    Ok((u.name, u.role))
}

/// Valida que el rol del agente destino esté en `target_roles` de la
/// categoría del ticket. Si no, devuelve `409 invalid_assignee_for_category`
/// con `details = { category_id, allowed_roles, agent_role }` para que el
/// front pueda mostrar el error con contexto.
///
/// La comparación es exacta sobre `f32` — `nRole` vive en valores
/// determinados (0.0, 0.5, 1.0, 1.5, ...) que son representables sin
/// pérdida en f32, así que `==` es seguro.
fn validate_assignee_for_category(
    category_id: &str,
    assignee_role: f32,
) -> Result<(), ApiError> {
    let cat = match find_category(category_id) {
        Some(c) => c,
        // Si la categoría no existe en el catálogo (probablemente legacy),
        // dejamos pasar — la creación del ticket ya valida `category_id`
        // contra el catálogo en `resolve_category`. Acá no es nuestro deber
        // bloquear por una categoría desconocida.
        None => return Ok(()),
    };
    if !cat.target_roles.iter().any(|r| *r == assignee_role) {
        return Err(ApiError::domain_with_details(
            StatusCode::CONFLICT,
            "invalid_assignee_for_category",
            format!(
                "El rol {} no puede ser asignado a tickets de '{}'",
                assignee_role, cat.label
            ),
            serde_json::json!({
                "category_id": cat.id,
                "allowed_roles": cat.target_roles,
                "agent_role": assignee_role,
            }),
        ));
    }
    Ok(())
}

fn ticket_already_open(existing: &WaTicket) -> ApiError {
    ApiError::domain_with_details(
        StatusCode::CONFLICT,
        "ticket_already_open",
        "Ya existe un ticket activo para esta conversación",
        serde_json::json!({
            "ticket_id": existing.id.map(|o| o.to_hex()).unwrap_or_default(),
        }),
    )
}

fn ticket_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "ticket_not_found",
        "Ticket no encontrado",
    )
}

fn invalid_transition(current: &str, action: &str) -> ApiError {
    ApiError::domain_with_details(
        StatusCode::CONFLICT,
        "invalid_transition",
        format!("La acción '{}' no es válida en estado '{}'", action, current),
        serde_json::json!({ "current": current, "action": action }),
    )
}

/// Broadcast `TICKET_ACTUALIZADO` al scope: creador + asignado actual + SUPERADMINs.
/// Dedup por user_id antes de enviar para no duplicar pushes en el socket.
async fn broadcast_ticket_updated(
    state: &Arc<AppState>,
    ticket: &TicketItem,
    previous_status: &str,
    changed_by_name: &str,
) {
    let event = WsServerEvent::TicketActualizado {
        ticket: ticket.clone(),
        previous_status: previous_status.to_string(),
        changed_by_name: changed_by_name.to_string(),
    };
    let payload = match serde_json::to_string(&event) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("[ws] serialize TICKET_ACTUALIZADO: {}", e);
            return;
        }
    };

    // Dedup: creador + asignado actual + SUPERADMINs.
    let mut recipients: std::collections::HashSet<String> = std::collections::HashSet::new();
    recipients.insert(ticket.created_by_id.clone());
    if let Some(uid) = &ticket.assigned_to_id { recipients.insert(uid.clone()); }
    match state.db.find_superadmin_ids().await {
        Ok(ids) => for id in ids { recipients.insert(id); },
        Err(e) => tracing::warn!("[ws] find_superadmin_ids: {}", e),
    }

    for uid in recipients {
        send_to_user(&state.ws_registry, &uid, payload.clone()).await;
    }
}

/// Envía `TICKET_ASIGNADO` sólo al destino. Drop silencioso si no está conectado.
async fn send_ticket_assigned(
    state: &Arc<AppState>,
    ticket: &TicketItem,
    assigned_by_name: &str,
) {
    let assignee = match ticket.assigned_to_id.as_deref() {
        Some(id) => id,
        None => return,
    };
    let event = WsServerEvent::TicketAsignado {
        ticket: ticket.clone(),
        assigned_by_name: assigned_by_name.to_string(),
    };
    let payload = match serde_json::to_string(&event) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("[ws] serialize TICKET_ASIGNADO: {}", e);
            return;
        }
    };
    send_to_user(&state.ws_registry, assignee, payload).await;
}

// ============================================
// HANDLER: GET /tickets/categories
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/tickets/categories",
    tag = "WhatsApp — Tickets",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Catálogo de categorías", body = TicketCategoriesResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — requiere bCanChat"),
    )
)]
pub async fn list_ticket_categories_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
) -> Result<Json<TicketCategoriesResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;
    let data = TICKET_CATEGORIES
        .iter()
        .map(|c| TicketCategoryItem {
            id: c.id.to_string(),
            label: c.label.to_string(),
            department: c.department.to_string(),
            target_roles: c.target_roles.to_vec(),
        })
        .collect();
    Ok(Json(TicketCategoriesResponse { ok: true, data }))
}

// ============================================
// HANDLER: POST /tickets
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/tickets",
    tag = "WhatsApp — Tickets",
    security(("bearerAuth" = [])),
    request_body = CreateTicketRequest,
    responses(
        (status = 201, description = "Ticket creado", body = TicketResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — requiere bCanChat"),
        (status = 404, description = "conversation_not_found"),
        (status = 409, description = "ticket_already_open — details.ticket_id"),
        (status = 422, description = "Validación: missing_field / field_too_long / invalid_category / user_not_found"),
    )
)]
pub async fn create_ticket_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    headers: HeaderMap,
    Json(body): Json<CreateTicketRequest>,
) -> Result<(StatusCode, Json<TicketResponse>), ApiError> {
    let creator = require_can_chat(&state, &claims.id).await?;

    let conv_oid = parse_oid(&body.conversation_id, "conversation_id")?;
    let reason = validate_reason(&body.reason)?;
    let (category_id, category_label) = resolve_category(body.category_id.as_deref())?;

    // Idempotency-Key: si ya existe ticket creado por este agente con la
    // misma key, devolvemos el ticket previo (200 OK semántico — pero
    // mantenemos 201 para no obligar al front a discriminar).
    let idempotency_key = extract_idempotency_key(&headers)?;
    if let Some(key) = &idempotency_key {
        if let Some(existing) = state.db
            .find_ticket_by_idempotency(&claims.id, key)
            .await
            .map_err(ApiError::DatabaseError)?
        {
            return Ok((StatusCode::CREATED, Json(TicketResponse {
                ok: true,
                data: ticket_to_item(existing, true),
            })));
        }
    }

    // 1. Conversación existe.
    let conv = state.db
        .find_conversation_by_id(&conv_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::domain_simple(
            StatusCode::NOT_FOUND,
            "conversation_not_found",
            "Conversación no encontrada",
        ))?;

    // 2. No hay ya un ticket abierto para la conversación.
    if let Some(existing) = state.db
        .find_open_ticket_for_conversation(&conv_oid)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        return Err(ticket_already_open(&existing));
    }

    // 3. Resolver agente destino si vino assign_to_id, validando rol vs categoría.
    let assignee_name = match body.assign_to_id.as_deref().filter(|s| !s.is_empty()) {
        Some(uid) => {
            let (name, role) = resolve_assignee(&state, uid).await?;
            if let Some(cid) = category_id.as_deref() {
                validate_assignee_for_category(cid, role)?;
            }
            Some(name)
        }
        None => None,
    };

    // 4. Snapshot del cliente.
    let (customer_name, customer_id) = resolve_customer_snapshot(&state, &conv).await;

    // 5. Construir doc.
    let now = BsonDateTime::now();
    let initial_status = if body.assign_to_id.is_some() { "open" } else { "open" };
    let timeline_note = body.transfer_note.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut timeline_entries = vec![WaTicketTimelineEntry {
        action: "created".into(),
        actor_id: claims.id.clone(),
        actor_name: creator.name.clone(),
        from_status: None,
        to_status: Some(initial_status.into()),
        assigned_to_id: body.assign_to_id.clone(),
        assigned_to_name: assignee_name.clone(),
        note: timeline_note,
        created_at: now,
    }];
    // Si vino assign al crear, el evento `created` ya refleja la asignación.
    // Documentamos un `assigned` adicional sólo cuando se necesita diferenciarlo
    // de la propia creación — para MVP no agregamos entry duplicada.
    let _ = &mut timeline_entries; // keep mutable for future expansion

    let ticket = WaTicket {
        id: None,
        conversation_id: conv_oid,
        customer_phone: conv.phone.clone(),
        customer_name,
        customer_id,
        business_phone: conv.business_phone.clone(),
        created_by_id: claims.id.clone(),
        created_by_name: creator.name.clone(),
        assigned_to_id: body.assign_to_id.clone(),
        assigned_to_name: assignee_name.clone(),
        category_id,
        category_label,
        reason,
        status: initial_status.into(),
        resolution: None,
        resolved_at: None,
        closed_at: None,
        transferred_from_id: None,
        transferred_from_name: None,
        idempotency_key,
        created_at: now,
        updated_at: now,
        timeline: timeline_entries,
    };

    let ticket = state.db
        .create_ticket(ticket)
        .await
        .map_err(ApiError::DatabaseError)?;

    // 6. Cierre best-effort de la conversación origen. Si ya estaba cerrada,
    // `close_conversation` sigue siendo seguro (es un $set sin precondición
    // de status). El error no bloquea la respuesta del ticket — se loggea.
    if conv.status != "closed" {
        if let Err(e) = state.db.close_conversation(&conv_oid).await {
            tracing::warn!("[tickets] close_conversation tras create_ticket falló: {}", e);
        } else {
            // Liberar carga del agente que tenía el chat (si lo había).
            if let Some(prev_agent) = conv.assigned_to.as_deref() {
                state.redis.decr_agent_load(prev_agent).await;
            }
            // Broadcast del cierre — mismo pattern que close_conversation_handler.
            let close_ev = WsServerEvent::ChatCerrado {
                conversation_id: body.conversation_id.clone(),
            };
            super::ws::broadcast_all(&state.ws_registry, &close_ev).await;
            // Auditoría — best-effort.
            if let Err(e) = state.db.record_conversation_event(crate::models::whatsapp::WaConversationEventInput {
                conversation_id: &conv_oid,
                business_phone: &conv.business_phone,
                event_type: "closed",
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(creator.name.as_str()),
                target_id: None,
                target_name: None,
                note: Some("ticket_created"),
            }).await {
                tracing::warn!("[tickets] record_conversation_event(closed) failed: {}", e);
            }
        }
    }

    let item = ticket_to_item(ticket, true);

    // 7. WS: TICKET_ASIGNADO al destino (si aplica).
    if item.assigned_to_id.is_some() {
        send_ticket_assigned(&state, &item, &creator.name).await;
    }

    Ok((StatusCode::CREATED, Json(TicketResponse {
        ok: true,
        data: item,
    })))
}

// ============================================
// HANDLER: GET /tickets
// ============================================

#[derive(Debug, Deserialize)]
pub struct TicketsListQuery {
    pub status: Option<String>,
    pub assigned_to_id: Option<String>,
    pub created_by_id: Option<String>,
    pub conversation_id: Option<String>,
    pub customer_phone: Option<String>,
    pub business_phone: Option<String>,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

fn parse_iso(s: &str, field: &str) -> Result<BsonDateTime, ApiError> {
    use chrono::DateTime as ChronoDateTime;
    ChronoDateTime::parse_from_rfc3339(s)
        .map(|dt| BsonDateTime::from_millis(dt.timestamp_millis()))
        .map_err(|_| ApiError::ValidationError {
            code: "invalid_date".into(),
            field: field.into(),
            message: format!("'{}' debe ser ISO-8601", field),
        })
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/tickets",
    tag = "WhatsApp — Tickets",
    security(("bearerAuth" = [])),
    params(
        ("status" = Option<String>, Query, description = "open|in_progress|resolved|closed|cancelled"),
        ("assigned_to_id" = Option<String>, Query, description = "UUID del asignado"),
        ("created_by_id" = Option<String>, Query, description = "UUID del creador"),
        ("conversation_id" = Option<String>, Query, description = "ObjectId hex"),
        ("customer_phone" = Option<String>, Query, description = "E.164 sin '+'"),
        ("business_phone" = Option<String>, Query, description = "E.164 sin '+'"),
        ("from_date" = Option<String>, Query, description = "ISO-8601"),
        ("to_date" = Option<String>, Query, description = "ISO-8601"),
        ("search" = Option<String>, Query, description = "Substring case-insensitive en reason/resolution"),
        ("limit" = Option<i64>, Query, description = "Default 30, máx 100"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco"),
    ),
    responses(
        (status = 200, description = "Lista de tickets", body = TicketsListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — requiere bCanChat"),
    )
)]
pub async fn list_tickets_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<TicketsListQuery>,
) -> Result<Json<TicketsListResponse>, ApiError> {
    let user = require_can_chat(&state, &claims.id).await?;
    let is_super = user.role == 0.0;

    // Parseo y normalización de filtros.
    let conv_oid = match q.conversation_id.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => Some(parse_oid(s, "conversation_id")?),
        None => None,
    };
    let from_date = match q.from_date.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => Some(parse_iso(s, "from_date")?),
        None => None,
    };
    let to_date = match q.to_date.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => Some(parse_iso(s, "to_date")?),
        None => None,
    };

    let limit = q
        .limit
        .map(|n| n.clamp(1, TICKET_LIST_MAX_LIMIT))
        .unwrap_or(TICKET_LIST_DEFAULT_LIMIT);

    // Scope por rol: agente sólo ve los suyos (asignado o creador). El front
    // puede filtrar adicionalmente con `assigned_to_id` o `created_by_id`,
    // pero el back los pisa para evitar leak.
    let (assigned_to_id, created_by_id): (Option<String>, Option<String>) = if is_super {
        (q.assigned_to_id.clone(), q.created_by_id.clone())
    } else {
        // Agente: el filtro es siempre `claims.id` en ambos campos (OR en repo).
        (Some(claims.id.clone()), Some(claims.id.clone()))
    };

    let filter = TicketListFilter {
        status: q.status.as_deref().filter(|s| !s.is_empty()),
        assigned_to_id: assigned_to_id.as_deref(),
        created_by_id: created_by_id.as_deref(),
        conversation_id: conv_oid.as_ref(),
        customer_phone: q.customer_phone.as_deref().filter(|s| !s.is_empty()),
        business_phone: q.business_phone.as_deref().filter(|s| !s.is_empty()),
        from_date,
        to_date,
        search: q.search.as_deref().filter(|s| !s.is_empty()),
        limit,
        cursor: q.cursor.as_deref().filter(|s| !s.is_empty()),
    };

    let tickets = state.db
        .list_tickets(filter)
        .await
        .map_err(ApiError::DatabaseError)?;

    // Cursor: último item si trajimos `limit` resultados.
    let next_cursor = if tickets.len() as i64 == limit {
        tickets.last().and_then(|t| {
            t.id.map(|oid| format!("{}_{}", t.created_at.timestamp_millis(), oid.to_hex()))
        })
    } else {
        None
    };

    let data = tickets.into_iter().map(|t| ticket_to_item(t, false)).collect();
    Ok(Json(TicketsListResponse { ok: true, data, next_cursor }))
}

// ============================================
// HANDLER: GET /tickets/:id
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/tickets/{id}",
    tag = "WhatsApp — Tickets",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    responses(
        (status = 200, description = "Detalle del ticket con timeline", body = TicketResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — sin acceso a este ticket"),
        (status = 404, description = "ticket_not_found"),
    )
)]
pub async fn get_ticket_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<TicketResponse>, ApiError> {
    let user = require_can_chat(&state, &claims.id).await?;
    let oid = parse_oid(&id, "id")?;

    let ticket = state.db
        .find_ticket_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(ticket_not_found)?;

    // Scope por rol: SUPERADMIN ve todo; agente sólo si es asignado o creador.
    let is_super = user.role == 0.0;
    let is_owner = ticket.created_by_id == claims.id
        || ticket.assigned_to_id.as_deref() == Some(claims.id.as_str());
    if !is_super && !is_owner {
        return Err(ApiError::Forbidden);
    }

    Ok(Json(TicketResponse {
        ok: true,
        data: ticket_to_item(ticket, true),
    }))
}

// ============================================
// HANDLER: PATCH /tickets/:id
// ============================================

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/tickets/{id}",
    tag = "WhatsApp — Tickets",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    request_body = UpdateTicketStatusRequest,
    responses(
        (status = 200, description = "Ticket actualizado", body = TicketResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — la transición requiere otro rol o ser asignado"),
        (status = 404, description = "ticket_not_found"),
        (status = 409, description = "invalid_transition — details.current + details.action"),
        (status = 422, description = "Validación: missing_field / user_not_found / invalid_action"),
    )
)]
pub async fn update_ticket_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTicketStatusRequest>,
) -> Result<Json<TicketResponse>, ApiError> {
    let user = require_can_chat(&state, &claims.id).await?;
    let is_super = user.role == 0.0;
    let oid = parse_oid(&id, "id")?;

    let ticket = state.db
        .find_ticket_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(ticket_not_found)?;

    let action = body.action.as_str();
    let current = ticket.status.as_str();
    let is_assignee = ticket.assigned_to_id.as_deref() == Some(claims.id.as_str());
    let is_creator = ticket.created_by_id == claims.id;

    // Resolución de transición + autorización.
    let (new_status, requires_assignee, requires_creator_or_super, expects_assign_to_id, set_resolved_at, set_closed_at, clear_assignment, change_assignment) =
        match (current, action) {
            ("open", "take")        => ("in_progress", false, false, false, false, false, false, true),
            ("open", "transfer")    => ("open",        false, true,  true,  false, false, false, true),
            ("open", "cancel")      => ("cancelled",   false, true,  false, false, false, true,  true),
            ("in_progress", "transfer") => ("open",     true,  false, true,  false, false, false, true),
            ("in_progress", "resolve")  => ("resolved", true,  false, false, true,  false, false, false),
            ("in_progress", "close")    => ("closed",   true,  false, false, false, true,  true,  true),
            ("resolved", "close")       => ("closed",   true,  false, false, false, true,  true,  true),
            ("resolved", "reopen")      => ("open",     false, false, false, false, false, true,  true),
            ("closed", "reopen")        => ("open",     false, false, false, false, false, true,  true),
            ("cancelled", "reopen")     => ("open",     false, false, false, false, false, true,  true),
            _ => return Err(invalid_transition(current, action)),
        };

    // Reglas de autorización:
    // - `requires_assignee` = la acción la ejecuta el asignado actual (o SUPERADMIN).
    // - `requires_creator_or_super` = creador o SUPERADMIN.
    // - `reopen` (de cualquier estado) = SUPERADMIN únicamente.
    if action == "reopen" && !is_super {
        return Err(ApiError::Forbidden);
    }
    if requires_assignee && !is_super && !is_assignee {
        return Err(ApiError::domain_simple(
            StatusCode::FORBIDDEN,
            "not_assignee",
            "Sólo el agente asignado puede realizar esta acción",
        ));
    }
    if requires_creator_or_super && !is_super && !is_creator {
        return Err(ApiError::Forbidden);
    }

    // Datos derivados del action.
    let mut new_assigned_id: Option<String> = ticket.assigned_to_id.clone();
    let mut new_assigned_name: Option<String> = ticket.assigned_to_name.clone();

    match action {
        "take" => {
            new_assigned_id = Some(claims.id.clone());
            new_assigned_name = Some(user.name.clone());
        }
        "transfer" => {
            let target_id = body.assign_to_id.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ApiError::ValidationError {
                    code: "missing_field".into(),
                    field: "assign_to_id".into(),
                    message: "assign_to_id es requerido para transfer".into(),
                })?;
            if !expects_assign_to_id {
                // Defensive — el match arriba ya dice que sí lo espera.
            }
            let (name, role) = resolve_assignee(&state, target_id).await?;
            // Si el ticket tiene categoría, validar que el destino pueda atenderla.
            if let Some(cid) = ticket.category_id.as_deref() {
                validate_assignee_for_category(cid, role)?;
            }
            new_assigned_id = Some(target_id.to_string());
            new_assigned_name = Some(name);
        }
        "cancel" | "close" | "reopen" => {
            new_assigned_id = None;
            new_assigned_name = None;
        }
        _ => {}
    }

    // Resolución (sólo aplica a resolve/close).
    let resolution_text = body.resolution.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Note del timeline.
    let note = body.note.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let now = BsonDateTime::now();
    let entry = WaTicketTimelineEntry {
        action: action_to_timeline_label(action).to_string(),
        actor_id: claims.id.clone(),
        actor_name: user.name.clone(),
        from_status: Some(current.into()),
        to_status: Some(new_status.into()),
        assigned_to_id: if change_assignment { new_assigned_id.clone() } else { None },
        assigned_to_name: if change_assignment { new_assigned_name.clone() } else { None },
        note,
        created_at: now,
    };

    let patch = TicketActionUpdate {
        new_status,
        assigned_to_id: new_assigned_id.as_deref(),
        assigned_to_name: new_assigned_name.as_deref(),
        clear_assignment,
        assignment_changed: change_assignment,
        resolution: resolution_text.as_deref(),
        set_resolved_at,
        set_closed_at,
        timeline_entry: entry,
    };

    let updated = state.db
        .update_ticket_action(&oid, patch)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(ticket_not_found)?;

    let item = ticket_to_item(updated, true);

    // WS broadcast.
    if action == "transfer" {
        send_ticket_assigned(&state, &item, &user.name).await;
    }
    broadcast_ticket_updated(&state, &item, current, &user.name).await;

    Ok(Json(TicketResponse { ok: true, data: item }))
}

fn action_to_timeline_label(action: &str) -> &'static str {
    match action {
        "take" => "taken",
        "transfer" => "transferred",
        "resolve" => "resolved",
        "close" => "closed",
        "cancel" => "cancelled",
        "reopen" => "reopened",
        _ => "status_changed",
    }
}

// ============================================
// HANDLER: POST /conversations/:id/transfer-and-ticket
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/transfer-and-ticket",
    tag = "WhatsApp — Tickets",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la conversación")),
    request_body = TransferAndTicketRequest,
    responses(
        (status = 201, description = "Ticket creado y conversación cerrada", body = TransferAndTicketResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — requiere ser asignado de la conversación o SUPERADMIN"),
        (status = 404, description = "conversation_not_found / user_not_found"),
        (status = 409, description = "ticket_already_open"),
        (status = 422, description = "Validación"),
    )
)]
pub async fn transfer_and_ticket_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<TransferAndTicketRequest>,
) -> Result<(StatusCode, Json<TransferAndTicketResponse>), ApiError> {
    let creator = require_can_chat(&state, &claims.id).await?;
    let conv_oid = parse_oid(&id, "id")?;
    let reason = validate_reason(&body.reason)?;
    let (category_id, category_label) = resolve_category(body.category_id.as_deref())?;

    let conv = state.db
        .find_conversation_by_id(&conv_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::domain_simple(
            StatusCode::NOT_FOUND,
            "conversation_not_found",
            "Conversación no encontrada",
        ))?;

    // Sólo asignado actual o SUPERADMIN puede transferir.
    let is_super = creator.role == 0.0;
    let is_assignee = conv.assigned_to.as_deref() == Some(claims.id.as_str());
    if !is_super && !is_assignee {
        return Err(ApiError::domain_simple(
            StatusCode::FORBIDDEN,
            "not_assignee",
            "Sólo el agente asignado puede transferir y generar ticket",
        ));
    }

    // Conflicto: ticket ya activo.
    if let Some(existing) = state.db
        .find_open_ticket_for_conversation(&conv_oid)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        return Err(ticket_already_open(&existing));
    }

    // Idempotency-Key — mismo soporte que POST /tickets.
    let idempotency_key = extract_idempotency_key(&headers)?;
    if let Some(key) = &idempotency_key {
        if let Some(existing) = state.db
            .find_ticket_by_idempotency(&claims.id, key)
            .await
            .map_err(ApiError::DatabaseError)?
        {
            // Construir conversation aún cuando reusamos ticket.
            let conv_after = state.db
                .find_conversation_by_id(&conv_oid)
                .await
                .map_err(ApiError::DatabaseError)?
                .ok_or_else(|| ApiError::domain_simple(
                    StatusCode::NOT_FOUND,
                    "conversation_not_found",
                    "Conversación no encontrada",
                ))?;
            let conv_item = build_conversation_item(&state, conv_after, &claims.id).await?;
            return Ok((StatusCode::CREATED, Json(TransferAndTicketResponse {
                ok: true,
                data: TransferAndTicketData {
                    ticket: ticket_to_item(existing, true),
                    conversation: conv_item,
                },
            })));
        }
    }

    // Validar destino + rol vs categoría (si aplica).
    let (assignee_name, assignee_role) = resolve_assignee(&state, &body.transfer_to_id).await?;
    if let Some(cid) = category_id.as_deref() {
        validate_assignee_for_category(cid, assignee_role)?;
    }
    let (customer_name, customer_id) = resolve_customer_snapshot(&state, &conv).await;

    let now = BsonDateTime::now();
    let timeline_note = body.note.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let timeline = vec![WaTicketTimelineEntry {
        action: "created".into(),
        actor_id: claims.id.clone(),
        actor_name: creator.name.clone(),
        from_status: None,
        to_status: Some("open".into()),
        assigned_to_id: Some(body.transfer_to_id.clone()),
        assigned_to_name: Some(assignee_name.clone()),
        note: timeline_note,
        created_at: now,
    }];

    let ticket = WaTicket {
        id: None,
        conversation_id: conv_oid,
        customer_phone: conv.phone.clone(),
        customer_name,
        customer_id,
        business_phone: conv.business_phone.clone(),
        created_by_id: claims.id.clone(),
        created_by_name: creator.name.clone(),
        assigned_to_id: Some(body.transfer_to_id.clone()),
        assigned_to_name: Some(assignee_name.clone()),
        category_id,
        category_label,
        reason,
        status: "open".into(),
        resolution: None,
        resolved_at: None,
        closed_at: None,
        // Origen del transfer = el agente actual de la conversación (creador).
        transferred_from_id: Some(claims.id.clone()),
        transferred_from_name: Some(creator.name.clone()),
        idempotency_key,
        created_at: now,
        updated_at: now,
        timeline,
    };

    let ticket = state.db.create_ticket(ticket).await.map_err(ApiError::DatabaseError)?;

    // Cierre de la conversación + auditoría + WS, igual que POST /tickets.
    if conv.status != "closed" {
        if let Err(e) = state.db.close_conversation(&conv_oid).await {
            tracing::warn!("[tickets] close_conversation tras transfer-and-ticket falló: {}", e);
        } else {
            if let Some(prev_agent) = conv.assigned_to.as_deref() {
                state.redis.decr_agent_load(prev_agent).await;
            }
            let close_ev = WsServerEvent::ChatCerrado { conversation_id: id.clone() };
            super::ws::broadcast_all(&state.ws_registry, &close_ev).await;
            if let Err(e) = state.db.record_conversation_event(crate::models::whatsapp::WaConversationEventInput {
                conversation_id: &conv_oid,
                business_phone: &conv.business_phone,
                event_type: "closed",
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(creator.name.as_str()),
                target_id: None,
                target_name: None,
                note: Some("transfer_and_ticket"),
            }).await {
                tracing::warn!("[tickets] record_conversation_event(closed) failed: {}", e);
            }
        }
    }

    let item = ticket_to_item(ticket, true);
    send_ticket_assigned(&state, &item, &creator.name).await;

    // Cargar conv actualizada para la respuesta.
    let conv_after = state.db
        .find_conversation_by_id(&conv_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::domain_simple(
            StatusCode::NOT_FOUND,
            "conversation_not_found",
            "Conversación no encontrada",
        ))?;
    let conv_item = build_conversation_item(&state, conv_after, &claims.id).await?;

    Ok((StatusCode::CREATED, Json(TransferAndTicketResponse {
        ok: true,
        data: TransferAndTicketData {
            ticket: item,
            conversation: conv_item,
        },
    })))
}
