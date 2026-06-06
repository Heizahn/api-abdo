use std::sync::Arc;

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
    modules::whatsapp::shared::{authz, mappers},
    state::AppState,
    utils::get_bson_amount::get_bson_amount,
};

#[derive(serde::Deserialize)]
pub struct ConversationStatsQuery {
    pub business_phone: Option<String>,
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
