//! Endpoints de auditoría / trazabilidad WhatsApp (SUPERADMIN only).
//!
//! El módulo de soporte (`handler.rs`) sirve la operación normal — los agentes
//! ven sólo sus conversaciones. La auditoría es cross-conversation y la usa
//! exclusivamente el supervisor: filtros por agente, número, rango de fechas,
//! tipo, dirección.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};
use serde::Deserialize;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{AuditMessageFilter, AuditMetricsFilter, ProfileRepository, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::{
        AuditAssignedToHistoryItem, AuditConversationEventItem, AuditConversationHeader,
        AuditConversationTimeline, AuditConversationTimelineResponse, AuditMessageItem,
        AuditMessagesResponse, AuditMetricsByAgent, AuditMetricsByDay, AuditMetricsByType,
        AuditMetricsData, AuditMetricsResponse, AuditMetricsSummary, WaConversation,
        WaConversationEvent, WaMessage,
    },
    state::AppState,
};

use super::handler::require_superadmin;

// ============================================
// CONSTANTES
// ============================================

/// Máximo rango de fechas aceptado (ISO-8601 → milisegundos). 90 días.
const AUDIT_MAX_RANGE_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// Rango por defecto cuando el caller no manda `from_date`/`to_date` — evita
/// que el dashboard abra mostrando histórico completo en la primera página.
const AUDIT_DEFAULT_RANGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const AUDIT_DEFAULT_LIMIT: i64 = 50;
const AUDIT_MAX_LIMIT: i64 = 200;

// ============================================
// QUERY PARAMS
// ============================================

#[derive(Debug, Deserialize)]
pub struct AuditMessagesQuery {
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub agent_id: Option<String>,
    pub customer_phone: Option<String>,
    pub business_phone: Option<String>,
    /// `"in"` o `"out"`.
    pub direction: Option<String>,
    /// Alias del shape API: `?type=image`. Se mapea a `msg_type` internamente.
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

// ============================================
// HELPERS
// ============================================

/// Parsea un string ISO-8601 (RFC3339) a `BsonDateTime`. Devuelve un
/// `ApiError::Domain { code: "invalid_date_range" }` si el formato es inválido.
fn parse_iso_to_bson(s: &str, field: &str) -> Result<BsonDateTime, ApiError> {
    use chrono::DateTime as ChronoDateTime;
    ChronoDateTime::parse_from_rfc3339(s)
        .map(|dt| BsonDateTime::from_millis(dt.timestamp_millis()))
        .map_err(|_| ApiError::Domain {
            status: axum::http::StatusCode::BAD_REQUEST,
            code: "invalid_date_range".into(),
            field: Some(field.to_string()),
            message: format!("Fecha inválida en {} (debe ser ISO-8601)", field),
            details: None,
        })
}

/// Validación: `from_date <= to_date` y rango ≤ 90 días.
fn validate_range(
    from: Option<BsonDateTime>,
    to: Option<BsonDateTime>,
) -> Result<(), ApiError> {
    if let (Some(f), Some(t)) = (from, to) {
        let f_ms = f.timestamp_millis();
        let t_ms = t.timestamp_millis();
        if f_ms > t_ms {
            return Err(ApiError::Domain {
                status: axum::http::StatusCode::BAD_REQUEST,
                code: "invalid_date_range".into(),
                field: Some("from_date".into()),
                message: "from_date debe ser menor o igual a to_date".into(),
                details: None,
            });
        }
        if t_ms - f_ms > AUDIT_MAX_RANGE_MS {
            return Err(ApiError::Domain {
                status: axum::http::StatusCode::BAD_REQUEST,
                code: "invalid_date_range".into(),
                field: Some("to_date".into()),
                message: "El rango entre from_date y to_date no puede superar 90 días".into(),
                details: None,
            });
        }
    }
    Ok(())
}

fn iso8601(dt: BsonDateTime) -> String {
    dt.try_to_rfc3339_string().unwrap_or_default()
}

fn encode_cursor(timestamp: BsonDateTime, id: ObjectId) -> String {
    format!("{}_{}", timestamp.timestamp_millis(), id.to_hex())
}

/// Resuelve nombres de cliente para un set de conversaciones.
/// Estrategia: para los que tengan `client_id`, batch lookup por IDs;
/// para los que no, batch lookup por phone.
async fn resolve_customer_names(
    state: &Arc<AppState>,
    convs: &HashMap<ObjectId, WaConversation>,
) -> HashMap<ObjectId, String> {
    let mut by_id: Vec<ObjectId> = Vec::new();
    let mut by_phone: Vec<String> = Vec::new();
    let mut conv_phones: HashMap<ObjectId, String> = HashMap::new();

    for (oid, c) in convs {
        if let Some(cid) = c.client_id {
            by_id.push(cid);
        }
        by_phone.push(c.phone.clone());
        conv_phones.insert(*oid, c.phone.clone());
    }

    let names_by_id = state.db.get_client_names_by_ids(&by_id).await.unwrap_or_default();
    let names_by_phone = state.db.get_client_names_by_phones(&by_phone).await.unwrap_or_default();

    let mut out = HashMap::new();
    for (oid, c) in convs {
        if let Some(cid) = c.client_id {
            if let Some(n) = names_by_id.get(&cid) {
                out.insert(*oid, n.clone());
                continue;
            }
        }
        if let Some(n) = names_by_phone.get(&c.phone) {
            out.insert(*oid, n.clone());
        }
    }
    out
}

/// Resuelve nombres de agentes (por `sent_by`) en una sola query batch.
async fn resolve_agent_names(
    state: &Arc<AppState>,
    user_ids: &[String],
) -> HashMap<String, String> {
    use crate::db::UserRepository;
    let mut out = HashMap::new();
    if user_ids.is_empty() {
        return out;
    }
    // Dedup
    let unique: std::collections::HashSet<&String> = user_ids.iter().collect();
    for id in unique {
        if let Ok(Some(u)) = state.db.find_user_by_id(id).await {
            out.insert(id.clone(), u.name);
        }
    }
    out
}

// ============================================
// HANDLER: GET /v1/auth-user/whatsapp/audit/messages
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/audit/messages",
    tag = "WhatsApp — Auditoría",
    security(("bearerAuth" = [])),
    params(
        ("from_date" = Option<String>, Query, description = "ISO-8601 — inicio del rango"),
        ("to_date" = Option<String>, Query, description = "ISO-8601 — fin del rango"),
        ("agent_id" = Option<String>, Query, description = "UUID del agente (filtra outbound por sent_by)"),
        ("customer_phone" = Option<String>, Query, description = "E.164 sin '+'"),
        ("business_phone" = Option<String>, Query, description = "Número WA del negocio en E.164 sin '+'"),
        ("direction" = Option<String>, Query, description = "'in' | 'out'"),
        ("type" = Option<String>, Query, description = "WaMessageType (text|image|audio|video|...)"),
        ("search" = Option<String>, Query, description = "Substring case-insensitive en body"),
        ("limit" = Option<i64>, Query, description = "Default 50, máx 200"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco de la página anterior"),
    ),
    responses(
        (status = 200, description = "Mensajes auditados", body = AuditMessagesResponse),
        (status = 400, description = "invalid_date_range — rango inválido o > 90 días"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — requiere SUPERADMIN"),
    )
)]
pub async fn audit_messages_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<AuditMessagesQuery>,
) -> Result<Json<AuditMessagesResponse>, ApiError> {
    // 1. Auth: SUPERADMIN only.
    require_superadmin(&state, &claims.id).await?;

    // 2. Validar fechas y rango. Si ambos faltan, defaultear a últimos 30 días
    //    (cap de 90 días igual aplica). Si sólo viene uno, completar el otro
    //    para mantener un rango bien definido.
    let from_raw = match q.from_date.as_deref() {
        Some(s) if !s.is_empty() => Some(parse_iso_to_bson(s, "from_date")?),
        _ => None,
    };
    let to_raw = match q.to_date.as_deref() {
        Some(s) if !s.is_empty() => Some(parse_iso_to_bson(s, "to_date")?),
        _ => None,
    };
    let now_ms = BsonDateTime::now().timestamp_millis();
    let (from_date, to_date) = match (from_raw, to_raw) {
        (Some(f), Some(t)) => (Some(f), Some(t)),
        (Some(f), None) => (
            Some(f),
            Some(BsonDateTime::from_millis(now_ms)),
        ),
        (None, Some(t)) => (
            Some(BsonDateTime::from_millis(t.timestamp_millis() - AUDIT_DEFAULT_RANGE_MS)),
            Some(t),
        ),
        (None, None) => (
            Some(BsonDateTime::from_millis(now_ms - AUDIT_DEFAULT_RANGE_MS)),
            Some(BsonDateTime::from_millis(now_ms)),
        ),
    };
    validate_range(from_date, to_date)?;

    let limit = q
        .limit
        .map(|n| n.clamp(1, AUDIT_MAX_LIMIT))
        .unwrap_or(AUDIT_DEFAULT_LIMIT);

    // 3. Si vienen filtros de teléfono, resolver primero los conversation_ids.
    let conversation_ids: Option<Vec<ObjectId>> = if q.customer_phone.is_some() || q.business_phone.is_some() {
        let ids = state.db
            .find_conversation_ids_by_phones(
                q.customer_phone.as_deref(),
                q.business_phone.as_deref(),
            )
            .await
            .map_err(ApiError::DatabaseError)?;
        Some(ids)
    } else {
        None
    };

    // Si pidieron filtrar y no hay match, retornar lista vacía sin tocar WaMessages.
    if matches!(conversation_ids.as_deref(), Some(ids) if ids.is_empty()) {
        return Ok(Json(AuditMessagesResponse { ok: true, data: vec![], next_cursor: None }));
    }

    // 4. Query principal sobre WaMessages.
    let filter = AuditMessageFilter {
        from_date,
        to_date,
        agent_id: q.agent_id.as_deref(),
        conversation_ids: conversation_ids.as_deref(),
        direction: q.direction.as_deref(),
        msg_type: q.msg_type.as_deref(),
        search: q.search.as_deref(),
        limit,
        cursor: q.cursor.as_deref(),
    };

    let messages: Vec<WaMessage> = state.db
        .audit_list_messages(filter)
        .await
        .map_err(ApiError::DatabaseError)?;

    // 5. Resolver datos de conversación + nombres de cliente + agentes en batch.
    let conv_ids: Vec<ObjectId> = messages
        .iter()
        .map(|m| m.conversation_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let convs = state.db
        .find_conversations_by_ids(&conv_ids)
        .await
        .map_err(ApiError::DatabaseError)?;

    let business_phones: Vec<String> = convs
        .values()
        .map(|c| c.business_phone.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let workspace_names = state.db
        .get_workspace_names(&business_phones)
        .await
        .unwrap_or_default();

    let customer_names = resolve_customer_names(&state, &convs).await;

    let agent_ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.sent_by.clone())
        .collect();
    let agent_names = resolve_agent_names(&state, &agent_ids).await;

    // 6. Armar el response item por mensaje.
    let next_cursor = messages.last().and_then(|m| {
        m.id.map(|oid| encode_cursor(m.timestamp, oid))
    }).filter(|_| messages.len() as i64 == limit);

    let mut data: Vec<AuditMessageItem> = Vec::with_capacity(messages.len());
    for m in messages {
        let conv = convs.get(&m.conversation_id);
        let (customer_phone, business_phone) = conv
            .map(|c| (c.phone.clone(), c.business_phone.clone()))
            .unwrap_or_default();
        let customer_name = customer_names.get(&m.conversation_id).cloned();
        let workspace_name = workspace_names.get(&business_phone).cloned();
        let from_user_name = m.sent_by.as_deref().and_then(|id| agent_names.get(id).cloned());

        data.push(AuditMessageItem {
            id: m.id.map(|o| o.to_hex()).unwrap_or_default(),
            conversation_id: m.conversation_id.to_hex(),
            customer_phone,
            customer_name,
            business_phone,
            workspace_name,
            direction: m.direction,
            msg_type: m.msg_type,
            content: m.body,
            media_filename: m.media_filename,
            from_user_id: m.sent_by,
            from_user_name,
            status: m.status,
            created_at: iso8601(m.timestamp),
        });
    }

    Ok(Json(AuditMessagesResponse {
        ok: true,
        data,
        next_cursor,
    }))
}

// ============================================
// HANDLER: GET /v1/auth-user/whatsapp/audit/conversations/:id/timeline
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/audit/conversations/{id}/timeline",
    tag = "WhatsApp — Auditoría",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Timeline completo de la conversación", body = AuditConversationTimelineResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — requiere SUPERADMIN"),
        (status = 404, description = "Conversación no encontrada"),
    )
)]
pub async fn audit_conversation_timeline_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<AuditConversationTimelineResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id)
        .map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    // 1. Conversación.
    let conv = state.db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 2. Eventos ordenados ASC (created_at).
    let events: Vec<WaConversationEvent> = state.db
        .list_conversation_events(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;

    // 3. Conteo de mensajes.
    let message_count = state.db
        .count_messages_for_conversation(&oid)
        .await
        .unwrap_or(0);

    // 4. Resolución de nombres faltantes (eventos backfilled tienen names en None).
    let missing_user_ids: Vec<String> = events
        .iter()
        .flat_map(|e| {
            let mut v = Vec::new();
            if let (Some(id), None) = (e.actor_id.as_deref(), e.actor_name.as_deref()) {
                v.push(id.to_string());
            }
            if let (Some(id), None) = (e.target_id.as_deref(), e.target_name.as_deref()) {
                v.push(id.to_string());
            }
            v
        })
        .collect();
    let agent_names = resolve_agent_names(&state, &missing_user_ids).await;

    // 5. Customer + workspace.
    let mut conv_map = HashMap::new();
    conv_map.insert(oid, conv.clone());
    let customer_names = resolve_customer_names(&state, &conv_map).await;
    let customer_name = customer_names.get(&oid).cloned();
    let workspace_name = state.db
        .get_workspace_names(&[conv.business_phone.clone()])
        .await
        .ok()
        .and_then(|m| m.get(&conv.business_phone).cloned());

    // 6. Mapear eventos al shape API.
    let events_out: Vec<AuditConversationEventItem> = events
        .iter()
        .map(|e| AuditConversationEventItem {
            id: e.id.map(|o| o.to_hex()).unwrap_or_default(),
            event_type: e.event_type.clone(),
            actor_id: e.actor_id.clone(),
            actor_name: e
                .actor_name
                .clone()
                .or_else(|| e.actor_id.as_deref().and_then(|id| agent_names.get(id).cloned())),
            target_id: e.target_id.clone(),
            target_name: e
                .target_name
                .clone()
                .or_else(|| e.target_id.as_deref().and_then(|id| agent_names.get(id).cloned())),
            note: e.note.clone(),
            created_at: iso8601(e.created_at),
        })
        .collect();

    // 7. Reconstruir assigned_to_history. Regla: el dueño nuevo es target_id
    //    (en `taken` actor==target; en `transferred` target==destino).
    //    `closed` cierra el intervalo; `reopened` no abre uno nuevo (se queda
    //    `pending` sin dueño hasta que llegue un `taken`).
    let assigned_to_history = build_assigned_to_history(&events_out);

    let header = AuditConversationHeader {
        id: oid.to_hex(),
        customer_phone: conv.phone,
        customer_name,
        business_phone: conv.business_phone,
        workspace_name,
        status: conv.status,
        created_at: iso8601(conv.created_at),
        updated_at: iso8601(conv.last_message_at),
    };

    Ok(Json(AuditConversationTimelineResponse {
        ok: true,
        data: AuditConversationTimeline {
            conversation: header,
            events: events_out,
            message_count,
            assigned_to_history,
        },
    }))
}

// ============================================
// HANDLER: GET /v1/auth-user/whatsapp/audit/metrics
// ============================================

#[derive(Debug, Deserialize)]
pub struct AuditMetricsQuery {
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub business_phone: Option<String>,
    pub granularity: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/audit/metrics",
    tag = "WhatsApp — Auditoría",
    security(("bearerAuth" = [])),
    params(
        ("from_date" = String, Query, description = "ISO-8601 — REQUERIDO"),
        ("to_date" = String, Query, description = "ISO-8601 — REQUERIDO"),
        ("business_phone" = Option<String>, Query, description = "E.164 sin '+' (filtra por workspace)"),
        ("granularity" = Option<String>, Query, description = "'day' | 'week' | 'month' (default 'day')"),
    ),
    responses(
        (status = 200, description = "Métricas agregadas", body = AuditMetricsResponse),
        (status = 400, description = "invalid_date_range | invalid_granularity"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Forbidden — requiere SUPERADMIN"),
    )
)]
pub async fn audit_metrics_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<AuditMetricsQuery>,
) -> Result<Json<AuditMetricsResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;

    // 1. Fechas REQUERIDAS para metrics (a diferencia de /messages que defaultea).
    let from_str = q.from_date.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| ApiError::Domain {
        status: axum::http::StatusCode::BAD_REQUEST,
        code: "invalid_date_range".into(),
        field: Some("from_date".into()),
        message: "from_date es requerido".into(),
        details: None,
    })?;
    let to_str = q.to_date.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| ApiError::Domain {
        status: axum::http::StatusCode::BAD_REQUEST,
        code: "invalid_date_range".into(),
        field: Some("to_date".into()),
        message: "to_date es requerido".into(),
        details: None,
    })?;
    let from_date = parse_iso_to_bson(from_str, "from_date")?;
    let to_date = parse_iso_to_bson(to_str, "to_date")?;
    validate_range(Some(from_date), Some(to_date))?;

    // 2. Granularity.
    let granularity = q.granularity.as_deref().unwrap_or("day");
    if !matches!(granularity, "day" | "week" | "month") {
        return Err(ApiError::Domain {
            status: axum::http::StatusCode::BAD_REQUEST,
            code: "invalid_granularity".into(),
            field: Some("granularity".into()),
            message: "granularity debe ser 'day', 'week' o 'month'".into(),
            details: None,
        });
    }

    // 3. Resolver conversation_ids si el caller filtra por business_phone.
    let conversation_ids: Option<Vec<ObjectId>> = if let Some(b) = q.business_phone.as_deref() {
        let ids = state.db
            .find_conversation_ids_by_phones(None, Some(b))
            .await
            .map_err(ApiError::DatabaseError)?;
        Some(ids)
    } else {
        None
    };

    // Si filtraron por workspace y no hay match, devolver shape vacío.
    if matches!(conversation_ids.as_deref(), Some(ids) if ids.is_empty()) {
        return Ok(Json(empty_metrics_response()));
    }

    let filter = AuditMetricsFilter {
        from_date,
        to_date,
        conversation_ids: conversation_ids.as_deref(),
        granularity,
    };

    // 4. Aggregates en paralelo. Algunas dependen de WaConversationEvents (no
    //    aceptan filtro de conversation_ids) y se filtran sólo por business_phone.
    let business_phone = q.business_phone.as_deref();
    let (
        summary_res,
        by_day_msgs_res,
        by_agent_res,
        by_type_res,
        first_resp_res,
        lifecycle_res,
        resolution_res,
    ) = tokio::join!(
        state.db.audit_messages_summary(&filter),
        state.db.audit_messages_by_day(&filter),
        state.db.audit_messages_by_agent(&filter),
        state.db.audit_messages_by_type(&filter),
        state.db.audit_first_responses(&filter),
        state.db.audit_lifecycle_by_day(from_date, to_date, business_phone, granularity),
        state.db.audit_resolution_times(from_date, to_date, business_phone),
    );

    let summary_raw = summary_res.map_err(ApiError::DatabaseError)?;
    let by_day_msgs = by_day_msgs_res.map_err(ApiError::DatabaseError)?;
    let by_agent_raw = by_agent_res.map_err(ApiError::DatabaseError)?;
    let by_type_raw = by_type_res.map_err(ApiError::DatabaseError)?;
    let first_responses = first_resp_res.map_err(ApiError::DatabaseError)?;
    let lifecycle = lifecycle_res.map_err(ApiError::DatabaseError)?;
    let resolution_times = resolution_res.map_err(ApiError::DatabaseError)?;

    // 5. Avg response time: avg de delta_seconds sobre conversaciones con par válido.
    let avg_response_time_seconds = avg_or_none(first_responses.iter().map(|f| f.delta_seconds));
    let avg_resolution_time_seconds = avg_or_none(resolution_times.iter().copied());

    // 6. Mapa agent_id → avg de delta_seconds (para `by_agent.avg_response_time_seconds`).
    let mut by_agent_avg: HashMap<String, (i64, u64)> = HashMap::new(); // (sum, count)
    for fr in &first_responses {
        if let Some(a) = fr.agent_id.as_deref() {
            let entry = by_agent_avg.entry(a.to_string()).or_insert((0, 0));
            entry.0 += fr.delta_seconds;
            entry.1 += 1;
        }
    }

    // 7. Resolver nombres de agentes en by_agent.
    let agent_ids: Vec<String> = by_agent_raw.iter().map(|a| a.agent_id.clone()).collect();
    let agent_names = resolve_agent_names(&state, &agent_ids).await;

    let by_agent: Vec<AuditMetricsByAgent> = by_agent_raw
        .into_iter()
        .map(|a| {
            let avg = by_agent_avg.get(&a.agent_id).map(|(sum, n)| {
                if *n == 0 { 0.0 } else { *sum as f64 / *n as f64 }
            });
            AuditMetricsByAgent {
                agent_name: agent_names.get(&a.agent_id).cloned().unwrap_or_default(),
                agent_id: a.agent_id,
                messages_sent: a.messages_sent,
                conversations_handled: a.conversations_handled,
                avg_response_time_seconds: avg,
            }
        })
        .collect();

    // 8. Merge by_day mensajes ↔ ciclo de vida en una sola lista por bucket.
    let mut by_day_map: HashMap<String, AuditMetricsByDay> = HashMap::new();
    for b in by_day_msgs {
        by_day_map.insert(
            b.date.clone(),
            AuditMetricsByDay {
                date: b.date,
                inbound: b.inbound,
                outbound: b.outbound,
                new_conversations: 0,
                closed_conversations: 0,
            },
        );
    }
    for b in lifecycle {
        let entry = by_day_map.entry(b.date.clone()).or_insert(AuditMetricsByDay {
            date: b.date,
            inbound: 0,
            outbound: 0,
            new_conversations: 0,
            closed_conversations: 0,
        });
        entry.new_conversations = b.new_conversations;
        entry.closed_conversations = b.closed_conversations;
    }
    let mut by_day: Vec<AuditMetricsByDay> = by_day_map.into_values().collect();
    by_day.sort_by(|a, b| a.date.cmp(&b.date));

    let by_message_type: Vec<AuditMetricsByType> = by_type_raw
        .into_iter()
        .map(|t| AuditMetricsByType { msg_type: t.msg_type, count: t.count })
        .collect();

    Ok(Json(AuditMetricsResponse {
        ok: true,
        data: AuditMetricsData {
            summary: AuditMetricsSummary {
                total_messages: summary_raw.total,
                total_inbound: summary_raw.inbound,
                total_outbound: summary_raw.outbound,
                total_conversations: summary_raw.distinct_conversations,
                avg_response_time_seconds,
                avg_resolution_time_seconds,
            },
            by_day,
            by_agent,
            by_message_type,
        },
    }))
}

fn empty_metrics_response() -> AuditMetricsResponse {
    AuditMetricsResponse {
        ok: true,
        data: AuditMetricsData {
            summary: AuditMetricsSummary {
                total_messages: 0,
                total_inbound: 0,
                total_outbound: 0,
                total_conversations: 0,
                avg_response_time_seconds: None,
                avg_resolution_time_seconds: None,
            },
            by_day: vec![],
            by_agent: vec![],
            by_message_type: vec![],
        },
    }
}

fn avg_or_none<I: Iterator<Item = i64>>(iter: I) -> Option<f64> {
    let mut sum: i64 = 0;
    let mut count: u64 = 0;
    for v in iter {
        sum += v;
        count += 1;
    }
    if count == 0 {
        None
    } else {
        Some(sum as f64 / count as f64)
    }
}

/// Reconstruye los intervalos de "quién tuvo asignada esta conversación"
/// recorriendo los eventos en orden ASC. Cada `taken`/`transferred` con
/// `target_id` distinto al dueño actual cierra el intervalo previo y abre
/// uno nuevo. `closed` cierra el intervalo activo. `reopened` y `created`
/// no abren intervalos por sí solos (no hay dueño hasta el próximo `taken`).
fn build_assigned_to_history(
    events: &[AuditConversationEventItem],
) -> Vec<AuditAssignedToHistoryItem> {
    let mut out: Vec<AuditAssignedToHistoryItem> = Vec::new();
    let mut current_owner: Option<(String, Option<String>, String)> = None; // (user_id, user_name, from)

    let close_current = |out: &mut Vec<AuditAssignedToHistoryItem>,
                         current: &mut Option<(String, Option<String>, String)>,
                         to_at: &str| {
        if let Some((uid, uname, from)) = current.take() {
            out.push(AuditAssignedToHistoryItem {
                user_id: Some(uid),
                user_name: uname,
                from,
                to: Some(to_at.to_string()),
            });
        }
    };

    for ev in events {
        match ev.event_type.as_str() {
            "taken" | "transferred" => {
                let new_owner = match ev.target_id.as_deref() {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => continue,
                };
                let same_owner = current_owner
                    .as_ref()
                    .map(|(uid, _, _)| uid == &new_owner)
                    .unwrap_or(false);
                if !same_owner {
                    close_current(&mut out, &mut current_owner, &ev.created_at);
                    current_owner = Some((
                        new_owner,
                        ev.target_name.clone(),
                        ev.created_at.clone(),
                    ));
                }
            }
            "closed" => {
                close_current(&mut out, &mut current_owner, &ev.created_at);
            }
            _ => {} // created, reopened, etc. no afectan ownership por sí solos.
        }
    }

    if let Some((uid, uname, from)) = current_owner {
        out.push(AuditAssignedToHistoryItem {
            user_id: Some(uid),
            user_name: uname,
            from,
            to: None,
        });
    }

    out
}
