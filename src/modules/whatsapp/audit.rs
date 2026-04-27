//! Endpoints de auditoría / trazabilidad WhatsApp (SUPERADMIN only).
//!
//! El módulo de soporte (`handler.rs`) sirve la operación normal — los agentes
//! ven sólo sus conversaciones. La auditoría es cross-conversation y la usa
//! exclusivamente el supervisor: filtros por agente, número, rango de fechas,
//! tipo, dirección.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Extension, Json,
};
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};
use serde::Deserialize;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{AuditMessageFilter, ProfileRepository, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::{
        AuditMessageItem, AuditMessagesResponse, WaConversation, WaMessage,
    },
    state::AppState,
};

use super::handler::require_superadmin;

// ============================================
// CONSTANTES
// ============================================

/// Máximo rango de fechas aceptado (ISO-8601 → milisegundos). 90 días.
const AUDIT_MAX_RANGE_MS: i64 = 90 * 24 * 60 * 60 * 1000;
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

    // 2. Validar fechas y rango.
    let from_date = match q.from_date.as_deref() {
        Some(s) if !s.is_empty() => Some(parse_iso_to_bson(s, "from_date")?),
        _ => None,
    };
    let to_date = match q.to_date.as_deref() {
        Some(s) if !s.is_empty() => Some(parse_iso_to_bson(s, "to_date")?),
        _ => None,
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
