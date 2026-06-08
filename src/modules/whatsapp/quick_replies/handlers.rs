use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use mongodb::bson::{oid::ObjectId, DateTime};

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{UpdateQuickReplyPatch, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::*,
    state::AppState,
};

use super::super::{
    quick_reply_validation::{validate_quick_reply, ValidatedQuickReply},
    shared::{authz::require_can_chat, response::quick_reply_to_item},
};

#[derive(serde::Deserialize)]
pub struct QuickRepliesQuery {
    /// Hex de `WaSettings._id`. Si viene, filtra a ese workspace puntual
    /// (el agente debe pertenecer a él o devuelve lista vacía).
    pub workspace_id: Option<String>,
    /// Si viene, filtra por `active = bool`. Omitir para traer todos.
    pub active: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/quick-replies",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(
        ("workspace_id" = Option<String>, Query, description = "Filtrar por workspace puntual (hex de WaSettings._id)"),
        ("active" = Option<bool>, Query, description = "Filtrar por estado activo (true/false)"),
    ),
    responses(
        (status = 200, description = "Lista completa de quick replies. Con `?workspace_id=X` filtra a items que tengan X en `workspace_ids`. Cada item incluye `can_edit` calculado para el caller (flag de delete).", body = QuickRepliesListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sin `bCanChat=true`"),
    )
)]
pub async fn list_quick_replies_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(q): Query<QuickRepliesQuery>,
) -> Result<Json<QuickRepliesListResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let filter_oid = match q.workspace_id.as_deref() {
        Some(hex) => Some(
            ObjectId::parse_str(hex)
                .map_err(|_| ApiError::BadRequest("workspace_id inválido".into()))?,
        ),
        None => None,
    };

    let docs = state
        .db
        .list_quick_replies(filter_oid.as_ref(), q.active)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(QuickRepliesListResponse {
        ok: true,
        data: docs
            .into_iter()
            .map(|q| quick_reply_to_item(q, &caller, &caller_workspaces))
            .collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/quick-replies",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = CreateQuickReplyRequest,
    responses(
        (status = 200, description = "Snippet creado", body = QuickReplyResponse),
        (status = 400, description = "Validación falló, `workspace_ids` vacío, o algún id no existe"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sin `bCanChat=true`, o no es agente en todos los workspaces indicados (y no es superadmin)"),
    )
)]
pub async fn create_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(payload): Json<CreateQuickReplyRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let title = payload.title.trim().to_string();
    let content = payload.content.trim().to_string();
    let workspace_oids = parse_and_validate_workspaces(&state, &payload.workspace_ids).await?;
    require_create_permission(caller.role, &caller_workspaces, &workspace_oids)?;
    let footer = payload.footer.as_ref().map(|s| s.trim().to_string());

    validate_quick_reply(&ValidatedQuickReply {
        title: &title,
        content: &content,
        workspace_ids_len: workspace_oids.len(),
        header: payload.header.as_ref(),
        footer: footer.as_deref(),
        buttons: payload.buttons.as_deref(),
        list: payload.list.as_ref(),
        cta_url: payload.cta_url.as_ref(),
    })?;

    let now = DateTime::now();
    let doc = WaQuickReply {
        id: None,
        title,
        content,
        workspace_ids: workspace_oids,
        created_by: claims.id.clone(),
        created_by_name: claims.name.clone(),
        created_at: now,
        updated_at: now,
        active: payload.active.unwrap_or(true),
        header: payload.header,
        footer,
        buttons: payload.buttons,
        list: payload.list,
        cta_url: payload.cta_url,
        use_count: 0,
        last_used_at: None,
    };

    let saved = state
        .db
        .create_quick_reply(doc)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(saved, &caller, &caller_workspaces),
    }))
}

#[utoipa::path(
    put,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet")),
    request_body = UpdateQuickReplyRequest,
    responses(
        (status = 200, description = "Snippet actualizado", body = QuickReplyResponse),
        (status = 400, description = "Validación falló"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene `bCanChat=true`"),
        (status = 404, description = "Snippet no encontrado"),
    )
)]
pub async fn update_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateQuickReplyRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let existing = state
        .db
        .find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // Normalización + parse de campos planos
    let title_new = payload.title.as_ref().map(|t| t.trim().to_string());
    let content_new = payload.content.as_ref().map(|c| c.trim().to_string());
    let workspace_oids = match &payload.workspace_ids {
        Some(list) => Some(parse_and_validate_workspaces(&state, list).await?),
        None => None,
    };
    let footer_patch: Option<Option<String>> = payload
        .footer
        .as_ref()
        .map(|opt| opt.as_ref().map(|s| s.trim().to_string()));

    // Merge patch + existing → estado final, para validar el doc completo.
    let merged_title = title_new.clone().unwrap_or_else(|| existing.title.clone());
    let merged_content = content_new
        .clone()
        .unwrap_or_else(|| existing.content.clone());
    let merged_ws_len = workspace_oids
        .as_ref()
        .map(|v| v.len())
        .unwrap_or(existing.workspace_ids.len());

    // Campos nullable: Some(Some) → nuevo valor, Some(None) → clear, None → mantener existente.
    let merged_header: Option<QuickReplyHeader> = match &payload.header {
        Some(Some(h)) => Some(h.clone()),
        Some(None) => None,
        None => existing.header.clone(),
    };
    let merged_footer: Option<String> = match &footer_patch {
        Some(Some(f)) => Some(f.clone()),
        Some(None) => None,
        None => existing.footer.clone(),
    };
    let merged_buttons: Option<Vec<QuickReplyButton>> = match &payload.buttons {
        Some(Some(b)) => Some(b.clone()),
        Some(None) => None,
        None => existing.buttons.clone(),
    };
    let merged_list: Option<QuickReplyList> = match &payload.list {
        Some(Some(l)) => Some(l.clone()),
        Some(None) => None,
        None => existing.list.clone(),
    };
    let merged_cta: Option<QuickReplyCtaUrl> = match &payload.cta_url {
        Some(Some(c)) => Some(c.clone()),
        Some(None) => None,
        None => existing.cta_url.clone(),
    };

    validate_quick_reply(&ValidatedQuickReply {
        title: &merged_title,
        content: &merged_content,
        workspace_ids_len: merged_ws_len,
        header: merged_header.as_ref(),
        footer: merged_footer.as_deref(),
        buttons: merged_buttons.as_deref(),
        list: merged_list.as_ref(),
        cta_url: merged_cta.as_ref(),
    })?;

    let patch = UpdateQuickReplyPatch {
        title: title_new,
        content: content_new,
        workspace_ids: workspace_oids,
        active: payload.active,
        header: payload.header,
        footer: footer_patch,
        buttons: payload.buttons,
        list: payload.list,
        cta_url: payload.cta_url,
    };

    let updated = state
        .db
        .update_quick_reply(&oid, patch)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(updated, &caller, &caller_workspaces),
    }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet")),
    responses(
        (status = 200, description = "Snippet eliminado", body = UpdateResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sin `bCanChat=true`, o sin overlap entre workspaces del caller y del item (y no es superadmin)"),
        (status = 404, description = "Snippet no encontrado"),
    )
)]
pub async fn delete_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let existing = state
        .db
        .find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;
    if !compute_can_edit(caller.role, &caller_workspaces, &existing.workspace_ids) {
        return Err(ApiError::Forbidden);
    }

    let deleted = state
        .db
        .delete_quick_reply(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !deleted {
        return Err(ApiError::NotFound);
    }
    Ok(Json(UpdateResponse { ok: true }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}/active",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet")),
    request_body = ToggleActiveRequest,
    responses(
        (status = 200, description = "Estado actualizado", body = QuickReplyResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene `bCanChat=true`"),
        (status = 404, description = "Snippet no encontrado"),
    )
)]
pub async fn set_quick_reply_active_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<ToggleActiveRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let updated = state
        .db
        .set_quick_reply_active(&oid, payload.active)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(updated, &caller, &caller_workspaces),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/quick-replies/{id}/duplicate",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del snippet original")),
    request_body = DuplicateQuickReplyRequest,
    responses(
        (status = 200, description = "Snippet duplicado. Se aplica la misma regla que crear sobre los workspaces del item resultante (los del payload si vienen, los del original si no).", body = QuickReplyResponse),
        (status = 400, description = "Validación falló o algún `workspace_id` no existe"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sin `bCanChat=true`, o no es agente en todos los workspaces del item resultante (y no es superadmin)"),
        (status = 404, description = "Snippet original no encontrado"),
    )
)]
pub async fn duplicate_quick_reply_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<DuplicateQuickReplyRequest>,
) -> Result<Json<QuickReplyResponse>, ApiError> {
    let caller = require_can_chat(&state, &claims.id).await?;
    let caller_workspaces = state
        .db
        .get_user_workspaces(&caller.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;
    let original = state
        .db
        .find_quick_reply_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let title = match payload.title.as_deref() {
        Some(t) => {
            let trimmed = t.trim().to_string();
            if trimmed.is_empty() || trimmed.chars().count() > 100 {
                return Err(ApiError::ValidationError {
                    code: "quick_reply_title_length".into(),
                    field: "title".into(),
                    message: "El título debe tener entre 1 y 100 caracteres.".into(),
                });
            }
            trimmed
        }
        None => {
            let proposed = format!("{} (copia)", original.title);
            // Truncar si supera 100 chars — nunca falla por el suffix.
            proposed.chars().take(100).collect::<String>()
        }
    };
    let workspace_oids = match payload.workspace_ids {
        Some(list) => parse_and_validate_workspaces(&state, &list).await?,
        None => original.workspace_ids.clone(),
    };
    // Duplicate es "create con campos heredados" — misma regla de autorización.
    require_create_permission(caller.role, &caller_workspaces, &workspace_oids)?;

    let now = DateTime::now();
    let doc = WaQuickReply {
        id: None,
        title,
        content: original.content.clone(),
        workspace_ids: workspace_oids,
        created_by: claims.id.clone(),
        created_by_name: claims.name.clone(),
        created_at: now,
        updated_at: now,
        // La copia nace activa, con use_count en 0. El resto de campos
        // interactivos se heredan tal cual del original.
        active: true,
        header: original.header.clone(),
        footer: original.footer.clone(),
        buttons: original.buttons.clone(),
        list: original.list.clone(),
        cta_url: original.cta_url.clone(),
        use_count: 0,
        last_used_at: None,
    };

    let saved = state
        .db
        .create_quick_reply(doc)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(QuickReplyResponse {
        ok: true,
        data: quick_reply_to_item(saved, &caller, &caller_workspaces),
    }))
}

fn require_create_permission(
    caller_role: f32,
    caller_workspaces: &[ObjectId],
    target_workspaces: &[ObjectId],
) -> Result<(), ApiError> {
    if caller_role == 0.0 {
        return Ok(());
    }
    for w in target_workspaces {
        if !caller_workspaces.contains(w) {
            return Err(ApiError::Forbidden);
        }
    }
    Ok(())
}

fn compute_can_edit(
    caller_role: f32,
    caller_workspaces: &[ObjectId],
    qr_workspace_ids: &[ObjectId],
) -> bool {
    if caller_role == 0.0 {
        return true;
    }

    qr_workspace_ids
        .iter()
        .any(|workspace_id| caller_workspaces.contains(workspace_id))
}

async fn parse_and_validate_workspaces(
    state: &Arc<AppState>,
    raw: &[String],
) -> Result<Vec<ObjectId>, ApiError> {
    if raw.is_empty() {
        return Err(ApiError::BadRequest(
            "workspace_ids requiere al menos 1".into(),
        ));
    }
    let mut oids = Vec::with_capacity(raw.len());
    for s in raw {
        let oid = ObjectId::parse_str(s)
            .map_err(|_| ApiError::BadRequest(format!("workspace_id inválido: {}", s)))?;
        oids.push(oid);
    }
    oids.sort();
    oids.dedup();

    if !state
        .db
        .wa_settings_exist(&oids)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        return Err(ApiError::BadRequest("algún workspace_id no existe".into()));
    }
    Ok(oids)
}
