use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use mongodb::bson::oid::ObjectId;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{WaTemplateListFilter, WaTemplateRepository, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::*,
    state::AppState,
};

use super::super::shared::{
    authz::{require_can_chat, require_superadmin},
    time::iso8601,
};

#[derive(serde::Deserialize)]
pub struct TemplatesListQuery {
    pub phone_number_id: String,
    pub status: Option<String>,
    pub category: Option<String>,
    pub only_system: Option<bool>,
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// Convierte un `WaTemplate` de DB al shape de response `WaTemplateItem`.
pub(in crate::modules::whatsapp) fn to_template_item(t: WaTemplate) -> WaTemplateItem {
    WaTemplateItem {
        id: t.id.map(|o| o.to_hex()).unwrap_or_default(),
        phone_number_id: t.phone_number_id,
        name: t.name,
        display_name: t.display_name,
        name_input: t.name_input,
        language: t.language,
        category: t.category,
        components: t.components,
        body_placeholders: t.body_placeholders,
        status: t.status,
        rejection_reason: t.rejection_reason,
        meta_template_id: t.meta_template_id,
        is_system: t.is_system,
        submit_to_meta: t.submit_to_meta,
        created_by: t.created_by,
        created_by_name: t.created_by_name,
        created_at: iso8601(t.created_at),
        updated_at: iso8601(t.updated_at),
    }
}

/// Error canónico para plantilla no encontrada (404).
pub(in crate::modules::whatsapp) fn template_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "template_not_found",
        "Plantilla no encontrada",
    )
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/templates",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(
        ("phone_number_id" = String, Query, description = "phone_number_id del workspace (requerido)"),
        ("status" = Option<String>, Query, description = "Filtrar por status(es) separados por coma"),
        ("category" = Option<String>, Query, description = "MARKETING | UTILITY | AUTHENTICATION"),
        ("only_system" = Option<bool>, Query, description = "Si true, sólo plantillas del sistema"),
        ("search" = Option<String>, Query, description = "Búsqueda substring en display_name y name"),
        ("limit" = Option<i64>, Query, description = "Default 50, máx 100"),
        ("cursor" = Option<String>, Query, description = "Cursor opaco de paginación"),
    ),
    responses(
        (status = 200, description = "Lista de plantillas", body = WaTemplatesListResponse),
        (status = 400, description = "Parámetros inválidos"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "phone_number_not_found"),
    )
)]
pub async fn list_templates_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<TemplatesListQuery>,
) -> Result<Json<WaTemplatesListResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let phone_number_id = q.phone_number_id.trim().to_string();
    if phone_number_id.is_empty() {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "phone_number_id es requerido",
        ));
    }

    state
        .db
        .find_wa_settings_by_phone_number_id(&phone_number_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_with_field(
                StatusCode::NOT_FOUND,
                "phone_number_not_found",
                "phone_number_id",
                "El número de WhatsApp no está configurado",
            )
        })?;

    let status_vec: Option<Vec<WaTemplateStatus>> = if let Some(s) = &q.status {
        let mut parsed = Vec::new();
        for part in s.split(',') {
            let trimmed = part.trim();
            let st = match trimmed.to_uppercase().as_str() {
                "DRAFT" => WaTemplateStatus::Draft,
                "PENDING" => WaTemplateStatus::Pending,
                "APPROVED" => WaTemplateStatus::Approved,
                "REJECTED" => WaTemplateStatus::Rejected,
                "PAUSED" => WaTemplateStatus::Paused,
                "DISABLED" => WaTemplateStatus::Disabled,
                _ => {
                    return Err(ApiError::domain_simple(
                        StatusCode::BAD_REQUEST,
                        "invalid_query",
                        "Status inválido",
                    ));
                }
            };
            parsed.push(st);
        }
        if parsed.is_empty() {
            None
        } else {
            Some(parsed)
        }
    } else {
        None
    };

    let category_filter: Option<WaTemplateCategory> = if let Some(c) = &q.category {
        Some(match c.trim().to_uppercase().as_str() {
            "MARKETING" => WaTemplateCategory::Marketing,
            "UTILITY" => WaTemplateCategory::Utility,
            "AUTHENTICATION" => WaTemplateCategory::Authentication,
            _ => {
                return Err(ApiError::domain_simple(
                    StatusCode::BAD_REQUEST,
                    "invalid_query",
                    "Categoría inválida",
                ));
            }
        })
    } else {
        None
    };

    let limit = q.limit.unwrap_or(50).clamp(1, 100);

    let filter = WaTemplateListFilter {
        phone_number_id: &phone_number_id,
        status: status_vec.as_deref(),
        category: category_filter,
        only_system: q.only_system.unwrap_or(false),
        search: q.search.as_deref(),
        limit,
        cursor: q.cursor.as_deref(),
    };

    let templates = state
        .db
        .list_templates_filtered(filter)
        .await
        .map_err(ApiError::DatabaseError)?;

    let next_cursor = if (templates.len() as i64) < limit {
        None
    } else {
        templates.last().and_then(|t| {
            t.id.map(|id| format!("{}_{}", t.created_at.timestamp_millis(), id.to_hex()))
        })
    };

    let data: Vec<WaTemplateItem> = templates.into_iter().map(to_template_item).collect();

    Ok(Json(WaTemplatesListResponse {
        ok: true,
        data,
        next_cursor,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/templates/{id}",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la plantilla")),
    responses(
        (status = 200, description = "Detalle de plantilla", body = WaTemplateResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sólo SUPERADMIN"),
        (status = 404, description = "template_not_found"),
    )
)]
pub async fn get_template_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<WaTemplateResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| template_not_found())?;

    let doc = state
        .db
        .find_template_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(template_not_found)?;

    Ok(Json(WaTemplateResponse {
        ok: true,
        data: to_template_item(doc),
    }))
}
