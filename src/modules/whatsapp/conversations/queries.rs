use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use mongodb::bson::oid::ObjectId;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::*,
    modules::whatsapp::shared::{authz, mappers, response},
    state::AppState,
    utils::get_bson_amount::get_bson_amount,
};

#[derive(serde::Deserialize)]
pub struct ConversationStatsQuery {
    pub business_phone: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct ConversationsQuery {
    pub status: Option<String>,
    pub assigned_to: Option<String>,
    pub business_phone: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

fn normalize_to_e164(input: &str) -> String {
    let digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.starts_with("58") {
        digits
    } else if let Some(rest) = digits.strip_prefix('0') {
        format!("58{}", rest)
    } else {
        format!("58{}", digits)
    }
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("status" = Option<String>, Query, description = "Filtrar por estado: pending | in_progress | closed"),
        ("assigned_to" = Option<String>, Query, description = "Filtrar por UUID de agente"),
        ("business_phone" = Option<String>, Query, description = "Filtrar por número de negocio (E.164 sin '+')"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco para paginación (copiar de next_cursor)"),
        ("limit" = Option<i64>, Query, description = "Resultados por página (default: 20, max: 100)"),
    ),
    responses(
        (status = 200, description = "Lista de conversaciones", body = ConversationsListResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_conversations_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<ConversationsQuery>,
) -> Result<Json<ConversationsListResponse>, ApiError> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let business_phone_norm = q.business_phone.as_deref().map(normalize_to_e164);

    let convs = state
        .db
        .get_conversations(
            q.status.as_deref(),
            q.assigned_to.as_deref(),
            business_phone_norm.as_deref(),
            q.cursor.as_deref(),
            limit,
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let next_cursor = if (convs.len() as i64) < limit {
        None
    } else {
        convs.last().and_then(|c| {
            Some(format!(
                "{}_{}",
                c.last_message_at.timestamp_millis(),
                c.id?.to_hex()
            ))
        })
    };

    // Batch-fetch last_opened_at del agente actual para todas las conversaciones.
    let ids: Vec<ObjectId> = convs.iter().filter_map(|c| c.id).collect();
    let opens = state
        .db
        .get_conversation_opens(&claims.id, &ids)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    // Batch-fetch workspace_name por business_phone único.
    let mut unique_phones: Vec<String> = convs.iter().map(|c| c.business_phone.clone()).collect();
    unique_phones.sort();
    unique_phones.dedup();
    let workspaces = state
        .db
        .get_workspace_names(&unique_phones)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    // Batch-resolve nombre del contacto contra Clients: primero por client_id,
    // luego por teléfono para los que no tienen link. Evita N+1 en listados.
    let (names_by_id, names_by_phone) = {
        use crate::db::ProfileRepository;
        let client_ids: Vec<ObjectId> = convs.iter().filter_map(|c| c.client_id).collect();
        let mut customer_phones: Vec<String> = convs
            .iter()
            .filter(|c| c.client_id.is_none())
            .map(|c| c.phone.clone())
            .collect();
        customer_phones.sort();
        customer_phones.dedup();
        let (ids_res, phones_res) = tokio::join!(
            state.db.get_client_names_by_ids(&client_ids),
            state.db.get_client_names_by_phones(&customer_phones),
        );
        (
            ids_res.map_err(ApiError::DatabaseError)?,
            phones_res.map_err(ApiError::DatabaseError)?,
        )
    };

    // Batch-resolve nombres de agentes (autor del último mensaje outbound + asignado).
    let agent_names = resolve_last_message_agent_names(&state, &convs).await;
    let assigned_names = resolve_assigned_agent_names(&state, &convs).await;

    let data = convs
        .into_iter()
        .map(|c| {
            let last_opened = c.id.and_then(|id| opens.get(&id).copied());
            let ws = workspaces.get(&c.business_phone).cloned();
            let resolved = c
                .client_id
                .and_then(|id| names_by_id.get(&id).cloned())
                .or_else(|| names_by_phone.get(&c.phone).cloned());
            let agent_name = c
                .last_message_from_user_id
                .as_ref()
                .and_then(|id| agent_names.get(id).cloned());
            let assigned_name = c
                .assigned_to
                .as_ref()
                .and_then(|id| assigned_names.get(id).cloned());
            response::conv_to_item(
                c,
                false,
                last_opened,
                ws,
                resolved,
                agent_name,
                assigned_name,
            )
        })
        .collect();

    Ok(Json(ConversationsListResponse {
        ok: true,
        data,
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/stats",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("business_phone" = Option<String>, Query, description = "Filtrar el scope a un solo número de negocio (E.164 sin '+'). Si se omite, cuenta sobre todos los números."),
    ),
    responses(
        (status = 200, description = "Contadores de conversaciones por categoría", body = ConversationStatsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn conversations_stats_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<ConversationStatsQuery>,
) -> Result<Json<ConversationStatsResponse>, ApiError> {
    let business_phone_norm = q.business_phone.as_deref().map(normalize_to_e164);
    let stats = state
        .db
        .get_conversation_stats(business_phone_norm.as_deref(), &claims.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(ConversationStatsResponse {
        ok: true,
        data: stats,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Detalle de conversación", body = ConversationDetailResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn get_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let item = mappers::build_conversation_item(&state, conv, &claims.id).await?;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        data: item,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/conversations/{id}/client-link",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Resolución del número del chat a cliente único o múltiples servicios", body = ConversationClientLinkResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "Conversación no encontrada"),
    )
)]
pub async fn get_conversation_client_link_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ConversationClientLinkResponse>, ApiError> {
    authz::require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let clients = state
        .db
        .find_clients_by_phone(&conv.phone)
        .await
        .map_err(ApiError::DatabaseError)?;

    if clients.is_empty() {
        let fallback_client_id = conv.client_id.map(|o| o.to_hex());
        return Ok(Json(ConversationClientLinkResponse {
            ok: true,
            data: ConversationClientLinkData {
                available: fallback_client_id.is_some(),
                resolution_type: if fallback_client_id.is_some() {
                    "single".into()
                } else {
                    "none".into()
                },
                client_id: fallback_client_id,
                services: vec![],
            },
        }));
    }

    if clients.len() == 1 {
        return Ok(Json(ConversationClientLinkResponse {
            ok: true,
            data: ConversationClientLinkData {
                available: true,
                resolution_type: "single".into(),
                client_id: Some(clients[0]._id.to_hex()),
                services: vec![],
            },
        }));
    }

    let seed_id = conv.client_id.unwrap_or_else(|| clients[0]._id);
    let raw = state
        .db
        .get_clients_by_phone_group(seed_id.to_hex())
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let mut services: Vec<ConversationClientLinkItem> = raw
        .into_iter()
        .map(|doc| ConversationClientLinkItem {
            id: doc
                .get_object_id("_id")
                .map(|v| v.to_hex())
                .unwrap_or_default(),
            name: doc.get_str("sName").unwrap_or_default().to_string(),
            phone: doc.get_str("sPhone").unwrap_or_default().to_string(),
            status: doc.get_str("sState").ok().map(|s| s.to_string()),
            balance: doc
                .contains_key("nBalance")
                .then(|| get_bson_amount(&doc, "nBalance")),
        })
        .filter(|item| !item.id.is_empty())
        .collect();

    services.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));

    Ok(Json(ConversationClientLinkResponse {
        ok: true,
        data: ConversationClientLinkData {
            available: !services.is_empty(),
            resolution_type: if services.len() <= 1 {
                "single".into()
            } else {
                "multiple".into()
            },
            client_id: if services.len() == 1 {
                services.first().map(|s| s.id.clone())
            } else {
                None
            },
            services,
        },
    }))
}

/// Batch-resolución de nombres de agentes para listados de conversaciones,
/// a partir de `last_message_from_user_id`. Dedup + 1 lookup por UUID único.
async fn resolve_last_message_agent_names(
    state: &Arc<AppState>,
    convs: &[WaConversation],
) -> HashMap<String, String> {
    use crate::db::UserRepository;

    let mut ids: Vec<String> = convs
        .iter()
        .filter_map(|c| c.last_message_from_user_id.clone())
        .collect();
    ids.sort();
    ids.dedup();

    let mut out = HashMap::new();
    for id in ids {
        if let Ok(Some(u)) = state.db.find_user_by_id(&id).await {
            out.insert(id, u.name);
        }
    }
    out
}

/// Batch-resolución de nombres de agentes asignados (`assigned_to`) para
/// listados. Mismo patrón que `resolve_last_message_agent_names`.
async fn resolve_assigned_agent_names(
    state: &Arc<AppState>,
    convs: &[WaConversation],
) -> HashMap<String, String> {
    use crate::db::UserRepository;

    let mut ids: Vec<String> = convs.iter().filter_map(|c| c.assigned_to.clone()).collect();
    ids.sort();
    ids.dedup();

    let mut out = HashMap::new();
    for id in ids {
        if let Ok(Some(u)) = state.db.find_user_by_id(&id).await {
            out.insert(id, u.name);
        }
    }
    out
}
