use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use mongodb::bson::{oid::ObjectId, DateTime};

use crate::{
    auth::user_jwt::UserProfileClaims,
    crypto::aes::decrypt_payload,
    db::{
        WaTemplateListFilter, WaTemplateMediaRef, WaTemplateMediaRepository, WaTemplateRepository,
        WaTemplateUpdatePatch, WhatsAppRepository,
    },
    error::ApiError,
    models::whatsapp::*,
    state::AppState,
};

use super::super::{
    messaging::media::swap_header_handles_in_components,
    service::WhatsAppService,
    shared::{
        authz::{require_can_chat, require_superadmin},
        settings_secret,
    },
    ws::{
        build_template_created_event, build_template_deleted_event, build_template_updated_event,
        emit_to_phone_number_agents,
    },
};
use super::meta::{
    flat_to_components, generate_template_name, map_meta_error, parse_meta_template_category,
    template_not_found, to_template_item, validate_components,
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

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/templates",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    request_body = CreateWaTemplateRequest,
    responses(
        (status = 200, description = "Plantilla creada", body = WaTemplateResponse),
        (status = 400, description = "Datos inválidos (name_required, name_invalid, invalid_component)"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "phone_number_not_found"),
        (status = 409, description = "name_already_exists"),
        (status = 502, description = "meta_rejected"),
    )
)]
pub async fn create_template_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(body): Json<CreateWaTemplateRequest>,
) -> Result<Json<WaTemplateResponse>, ApiError> {
    let creator = require_can_chat(&state, &claims.id).await?;

    if body.name_input.trim().is_empty() {
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "name_required",
            "name_input",
            "El nombre es requerido",
        ));
    }

    let settings = state
        .db
        .find_wa_settings_by_phone_number_id(&body.phone_number_id)
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

    let name = generate_template_name(&body.name_input, body.is_system);

    {
        let re = regex::Regex::new(r"^[a-z][a-z0-9_]{0,511}$").expect("regex válido");
        if !re.is_match(&name) {
            return Err(ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "name_invalid",
                "name_input",
                "Nombre inválido. Usa solo letras minúsculas, números y guión bajo (debe empezar con letra)",
            ));
        }
    }

    let components = flat_to_components(
        body.header.as_ref(),
        &body.body,
        body.body_samples.as_ref(),
        body.footer.as_deref(),
        body.buttons.as_ref(),
    );
    let body_placeholders = validate_components(&components)?;
    let created_by_name = creator.name.clone();

    let existing = state
        .db
        .find_template_by_phone_name_lang(&body.phone_number_id, &name, &body.language)
        .await
        .map_err(ApiError::DatabaseError)?;
    if existing.is_some() {
        return Err(ApiError::domain_with_field(
            StatusCode::CONFLICT,
            "name_already_exists",
            "name_input",
            "Ya existe una plantilla con ese nombre en este idioma",
        ));
    }

    let now = DateTime::now();
    let mut status = WaTemplateStatus::Draft;
    let mut meta_template_id: Option<String> = None;

    if body.submit_to_meta {
        if settings.access_token.is_empty() {
            return Err(ApiError::Internal(
                "workspace sin access_token configurado".into(),
            ));
        }
        let token = decrypt_payload(&settings_secret(), &settings.access_token)
            .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;

        let waba_id = settings.whatsapp_business_account_id.trim().to_string();
        if waba_id.is_empty() {
            return Err(ApiError::Internal(
                "workspace sin whatsapp_business_account_id configurado".into(),
            ));
        }

        let category_str = match body.category {
            WaTemplateCategory::Marketing => "MARKETING",
            WaTemplateCategory::Utility => "UTILITY",
            WaTemplateCategory::Authentication => "AUTHENTICATION",
        };

        let mut components_for_meta = components.clone();
        swap_header_handles_in_components(&state, &mut components_for_meta, &token).await?;
        let components_val = serde_json::Value::Array(components_for_meta);

        let wa = WhatsAppService::new(
            state.reqwest_client.clone(),
            settings.phone_number_id.clone(),
            token,
        );

        match wa
            .create_template_meta(
                &waba_id,
                &name,
                &body.language,
                category_str,
                &components_val,
            )
            .await
        {
            Ok(resp) => {
                status = WaTemplateStatus::Pending;
                meta_template_id = Some(resp.id);
            }
            Err(e) => {
                return Err(map_meta_error(&e, "Meta rechazó la plantilla"));
            }
        }
    }

    let doc = WaTemplate {
        id: None,
        phone_number_id: body.phone_number_id.clone(),
        name: name.clone(),
        display_name: body.name_input.clone(),
        name_input: body.name_input.clone(),
        language: body.language.clone(),
        category: body.category,
        components,
        body_placeholders,
        status,
        rejection_reason: None,
        meta_template_id,
        is_system: body.is_system,
        submit_to_meta: body.submit_to_meta,
        created_by: claims.id.clone(),
        created_by_name,
        created_at: now,
        updated_at: now,
    };

    let saved = state.db.create_template(doc).await.map_err(|e| {
        if e == "name_already_exists" {
            ApiError::domain_with_field(
                StatusCode::CONFLICT,
                "name_already_exists",
                "name_input",
                "Ya existe una plantilla con ese nombre en este idioma",
            )
        } else {
            ApiError::DatabaseError(e)
        }
    })?;

    let item = to_template_item(saved);
    let ws_payload = build_template_created_event(&item);
    emit_to_phone_number_agents(&state, &body.phone_number_id, ws_payload).await;

    Ok(Json(WaTemplateResponse {
        ok: true,
        data: item,
    }))
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

    let mut data = Vec::with_capacity(templates.len());
    for template in templates {
        data.push(to_template_item_with_default_media_binding(&state, template).await?);
    }

    Ok(Json(WaTemplatesListResponse {
        ok: true,
        data,
        next_cursor,
    }))
}

async fn to_template_item_with_default_media_binding(
    state: &Arc<AppState>,
    template: WaTemplate,
) -> Result<WaTemplateItem, ApiError> {
    let default_media_binding = load_default_media_binding(state, &template).await?;
    let mut item = to_template_item(template);
    item.default_media_binding = default_media_binding;
    Ok(item)
}

async fn load_default_media_binding(
    state: &Arc<AppState>,
    template: &WaTemplate,
) -> Result<Option<WaTemplateDefaultMediaBinding>, ApiError> {
    let Some((media_type, media_id)) = template_header_media_ref(&template.components) else {
        return Ok(None);
    };
    let Ok(oid) = ObjectId::parse_str(&media_id) else {
        return Ok(None);
    };

    let Some(media) = state
        .db
        .find_template_media_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
    else {
        return Ok(None);
    };

    Ok(Some(template_default_media_binding(media_type, media)))
}

fn template_header_media_ref(components: &[serde_json::Value]) -> Option<(String, String)> {
    let header = components.iter().find(|component| {
        component
            .get("type")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("HEADER"))
    })?;
    let media_type = match header
        .get("format")?
        .as_str()?
        .to_ascii_uppercase()
        .as_str()
    {
        "IMAGE" => "image",
        "VIDEO" => "video",
        "DOCUMENT" => "document",
        _ => return None,
    };
    let media_id = header.pointer("/example/header_handle/0")?.as_str()?.trim();
    if media_id.is_empty() {
        return None;
    }
    Some((media_type.to_string(), media_id.to_string()))
}

fn template_default_media_binding(
    media_type: String,
    media: WaTemplateMediaRef,
) -> WaTemplateDefaultMediaBinding {
    WaTemplateDefaultMediaBinding {
        component: "header".to_string(),
        media_type,
        source: "template_media_id".to_string(),
        value: media.id.to_hex(),
        mime_type: media.mime_type,
        file_size: media.file_size,
        sha256: media.sha256,
        display_name: "Media guardada en plantilla".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_image_object_id_can_derive_default_media_binding() {
        let media_id = ObjectId::parse_str("665f00000000000000000001").unwrap();
        let components = vec![serde_json::json!({
            "type": "HEADER",
            "format": "IMAGE",
            "example": { "header_handle": [media_id.to_hex()] }
        })];
        let media = WaTemplateMediaRef {
            id: media_id,
            phone_number_id: "123".to_string(),
            format: "IMAGE".to_string(),
            mime_type: "image/jpeg".to_string(),
            sha256: "abc".to_string(),
            file_size: 12345,
        };

        let (media_type, value) = template_header_media_ref(&components).unwrap();
        let binding = template_default_media_binding(media_type, media);

        assert_eq!(value, "665f00000000000000000001");
        assert_eq!(binding.component, "header");
        assert_eq!(binding.media_type, "image");
        assert_eq!(binding.source, "template_media_id");
        assert_eq!(binding.value, "665f00000000000000000001");
        assert_eq!(binding.mime_type, "image/jpeg");
        assert_eq!(binding.file_size, 12345);
        assert_eq!(binding.sha256, "abc");
    }

    #[test]
    fn header_handle_non_object_id_is_not_local_media() {
        let components = vec![serde_json::json!({
            "type": "HEADER",
            "format": "IMAGE",
            "example": { "header_handle": ["4::meta-header-handle"] }
        })];

        let (_, value) = template_header_media_ref(&components).unwrap();
        assert!(ObjectId::parse_str(value).is_err());
    }
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
        data: to_template_item_with_default_media_binding(&state, doc).await?,
    }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/templates/{id}",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la plantilla")),
    request_body = UpdateWaTemplateRequest,
    responses(
        (status = 200, description = "Plantilla actualizada", body = WaTemplateResponse),
        (status = 400, description = "invalid_component"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "cannot_edit_approved o Sólo SUPERADMIN"),
        (status = 404, description = "template_not_found"),
        (status = 409, description = "cannot_edit_pending, name_already_exists"),
        (status = 429, description = "meta_edit_rate_limited"),
        (status = 502, description = "meta_rejected"),
    )
)]
pub async fn update_template_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(body): Json<UpdateWaTemplateRequest>,
) -> Result<Json<WaTemplateResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| template_not_found())?;

    let doc = state
        .db
        .find_template_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(template_not_found)?;

    let prev_status = doc.status;

    let any_flat_components = body.header.is_some()
        || body.body.is_some()
        || body.body_samples.is_some()
        || body.footer.is_some()
        || body.buttons.is_some();

    let new_components_opt: Option<Vec<serde_json::Value>> = if any_flat_components {
        let body_text = body.body.as_deref().ok_or_else(|| {
            ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "body_required",
                "body",
                "Para editar componentes (header/footer/buttons) debes incluir también el body",
            )
        })?;
        Some(flat_to_components(
            body.header.as_ref(),
            body_text,
            body.body_samples.as_ref(),
            body.footer.as_deref(),
            body.buttons.as_ref(),
        ))
    } else {
        None
    };

    match prev_status {
        WaTemplateStatus::Pending | WaTemplateStatus::Paused | WaTemplateStatus::Disabled => {
            return Err(ApiError::domain_simple(
                StatusCode::CONFLICT,
                "cannot_edit_pending",
                "No se puede editar una plantilla en revisión",
            ));
        }
        WaTemplateStatus::Approved => {
            let has_forbidden =
                body.name_input.is_some() || body.category.is_some() || body.is_system.is_some();
            if has_forbidden {
                return Err(ApiError::domain_simple(
                    StatusCode::FORBIDDEN,
                    "cannot_edit_approved",
                    "Solo el cuerpo es editable en plantillas aprobadas",
                ));
            }
            if let Some(ref new_comps) = new_components_opt {
                let has_non_body = new_comps.iter().any(|c| {
                    c.get("type")
                        .and_then(|v| v.as_str())
                        .map(|t| !t.eq_ignore_ascii_case("BODY"))
                        .unwrap_or(false)
                });
                if has_non_body {
                    return Err(ApiError::domain_simple(
                        StatusCode::FORBIDDEN,
                        "cannot_edit_approved",
                        "Solo el cuerpo es editable en plantillas aprobadas",
                    ));
                }
            }
        }
        WaTemplateStatus::Draft | WaTemplateStatus::Rejected => {}
    }

    let mut patch = WaTemplateUpdatePatch {
        name: None,
        display_name: None,
        name_input: None,
        category: body.category,
        components: None,
        body_placeholders: None,
        status: None,
        rejection_reason: None,
        meta_template_id: None,
        is_system: body.is_system,
        submit_to_meta: None,
    };

    if let Some(ref new_name_input) = body.name_input {
        if new_name_input.trim().is_empty() {
            return Err(ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "name_required",
                "name_input",
                "El nombre es requerido",
            ));
        }
        let is_system = body.is_system.unwrap_or(doc.is_system);
        let new_name = generate_template_name(new_name_input, is_system);
        {
            let re = regex::Regex::new(r"^[a-z][a-z0-9_]{0,511}$").expect("regex válido");
            if !re.is_match(&new_name) {
                return Err(ApiError::domain_with_field(
                    StatusCode::BAD_REQUEST,
                    "name_invalid",
                    "name_input",
                    "Nombre inválido. Usa solo letras minúsculas, números y guión bajo (debe empezar con letra)",
                ));
            }
        }
        if new_name != doc.name {
            let existing = state
                .db
                .find_template_by_phone_name_lang(&doc.phone_number_id, &new_name, &doc.language)
                .await
                .map_err(ApiError::DatabaseError)?;
            if existing.is_some() {
                return Err(ApiError::domain_with_field(
                    StatusCode::CONFLICT,
                    "name_already_exists",
                    "name_input",
                    "Ya existe una plantilla con ese nombre en este idioma",
                ));
            }
            patch.name = Some(new_name);
        }
        patch.display_name = Some(new_name_input.clone());
        patch.name_input = Some(new_name_input.clone());
    }

    if body.submit_to_meta == Some(true) && !doc.submit_to_meta {
        let settings = state
            .db
            .find_wa_settings_by_phone_number_id(&doc.phone_number_id)
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

        let token = decrypt_payload(&settings_secret(), &settings.access_token)
            .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
        let waba_id = settings.whatsapp_business_account_id.trim().to_string();

        let name_for_meta = patch.name.as_deref().unwrap_or(&doc.name);
        let category_str = match patch.category.unwrap_or(doc.category) {
            WaTemplateCategory::Marketing => "MARKETING",
            WaTemplateCategory::Utility => "UTILITY",
            WaTemplateCategory::Authentication => "AUTHENTICATION",
        };

        let mut comps_for_meta = patch.components.as_ref().unwrap_or(&doc.components).clone();
        swap_header_handles_in_components(&state, &mut comps_for_meta, &token).await?;
        let comps_val = serde_json::Value::Array(comps_for_meta);

        let wa = WhatsAppService::new(
            state.reqwest_client.clone(),
            settings.phone_number_id.clone(),
            token,
        );

        match wa
            .create_template_meta(
                &waba_id,
                name_for_meta,
                &doc.language,
                category_str,
                &comps_val,
            )
            .await
        {
            Ok(resp) => {
                patch.status = Some(WaTemplateStatus::Pending);
                patch.meta_template_id = Some(Some(resp.id));
                patch.submit_to_meta = Some(true);
            }
            Err(e) => {
                return Err(map_meta_error(&e, "Meta rechazó la plantilla"));
            }
        }
    }

    if prev_status == WaTemplateStatus::Approved {
        if let Some(ref new_comps) = new_components_opt {
            let settings = state
                .db
                .find_wa_settings_by_phone_number_id(&doc.phone_number_id)
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
            let token = decrypt_payload(&settings_secret(), &settings.access_token)
                .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;

            let meta_id = doc.meta_template_id.as_deref().ok_or_else(|| {
                ApiError::Internal("plantilla aprobada sin meta_template_id".into())
            })?;

            let mut comps_for_meta = new_comps.clone();
            swap_header_handles_in_components(&state, &mut comps_for_meta, &token).await?;
            let comps_val = serde_json::Value::Array(comps_for_meta);

            let wa = WhatsAppService::new(
                state.reqwest_client.clone(),
                settings.phone_number_id.clone(),
                token,
            );

            if let Err(e) = wa.update_template_body_meta(meta_id, &comps_val).await {
                return Err(map_meta_error(&e, "Meta rechazó la edición del template"));
            }
        }
    }

    if let Some(ref new_comps) = new_components_opt {
        let bp = validate_components(new_comps)?;
        patch.components = Some(new_comps.clone());
        patch.body_placeholders = Some(bp);
    }

    let updated = state
        .db
        .update_template(&oid, patch)
        .await
        .map_err(|e| {
            if e == "name_already_exists" {
                ApiError::domain_with_field(
                    StatusCode::CONFLICT,
                    "name_already_exists",
                    "name_input",
                    "Ya existe una plantilla con ese nombre en este idioma",
                )
            } else {
                ApiError::DatabaseError(e)
            }
        })?
        .ok_or_else(template_not_found)?;

    let item = to_template_item(updated);
    let prev_for_ws = if item.status != prev_status {
        Some(prev_status)
    } else {
        None
    };
    let ws_payload = build_template_updated_event(&item, prev_for_ws);
    emit_to_phone_number_agents(&state, &item.phone_number_id, ws_payload).await;

    Ok(Json(WaTemplateResponse {
        ok: true,
        data: item,
    }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/templates/{id}",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la plantilla")),
    responses(
        (status = 200, description = "Plantilla eliminada", body = DeleteWaTemplateResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sólo SUPERADMIN"),
        (status = 404, description = "template_not_found"),
        (status = 409, description = "template_in_use_cannot_delete"),
        (status = 502, description = "meta_rejected"),
    )
)]
pub async fn delete_template_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<DeleteWaTemplateResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| template_not_found())?;

    let doc = state
        .db
        .find_template_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(template_not_found)?;

    let in_use = state
        .db
        .count_templates_in_purposes(&doc.phone_number_id, &doc.name)
        .await
        .map_err(ApiError::DatabaseError)?;

    if !in_use.is_empty() {
        return Err(ApiError::domain_with_details(
            StatusCode::CONFLICT,
            "template_in_use_cannot_delete",
            "La plantilla está en uso en propósitos del sistema",
            serde_json::json!({ "purposes": in_use }),
        ));
    }

    if let Some(ref meta_id) = doc.meta_template_id {
        let settings = state
            .db
            .find_wa_settings_by_phone_number_id(&doc.phone_number_id)
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
        let token = decrypt_payload(&settings_secret(), &settings.access_token)
            .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
        let waba_id = settings.whatsapp_business_account_id.trim().to_string();

        let wa = WhatsAppService::new(
            state.reqwest_client.clone(),
            settings.phone_number_id.clone(),
            token,
        );

        if let Err(e) = wa.delete_template_meta(&waba_id, meta_id, &doc.name).await {
            return Err(map_meta_error(&e, "Meta rechazó el borrado del template"));
        }
    }

    state
        .db
        .delete_template(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;

    let ws_payload = build_template_deleted_event(
        &oid.to_hex(),
        &doc.name,
        &doc.language,
        &doc.phone_number_id,
    );
    emit_to_phone_number_agents(&state, &doc.phone_number_id, ws_payload).await;

    Ok(Json(DeleteWaTemplateResponse {
        ok: true,
        data: DeleteWaTemplateData { id: oid.to_hex() },
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/templates/{id}/resync",
    tag = "WhatsApp — Templates",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la plantilla")),
    responses(
        (status = 200, description = "Estado/categoría sincronizados desde Meta", body = WaTemplateResponse),
        (status = 400, description = "draft_cannot_resync (la plantilla está en DRAFT)"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sólo SUPERADMIN"),
        (status = 404, description = "template_not_found"),
        (status = 502, description = "meta_rejected (Meta no devolvió el template)"),
    )
)]
pub async fn resync_template_handler(
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

    let meta_id = doc.meta_template_id.as_deref().ok_or_else(|| {
        ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "draft_cannot_resync",
            "La plantilla está en DRAFT — todavía no fue enviada a Meta, no hay nada que sincronizar",
        )
    })?;

    let settings = state
        .db
        .find_wa_settings_by_phone_number_id(&doc.phone_number_id)
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

    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;

    let wa = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id.clone(),
        token,
    );

    let info = wa
        .get_template_meta(meta_id)
        .await
        .map_err(|e| map_meta_error(&e, "Meta no devolvió el template"))?;

    let (new_status, rejection_reason): (WaTemplateStatus, Option<String>) =
        match info.status.to_uppercase().as_str() {
            "APPROVED" => (WaTemplateStatus::Approved, None),
            "REJECTED" => (WaTemplateStatus::Rejected, info.rejected_reason),
            "FLAGGED" => (
                WaTemplateStatus::Rejected,
                Some("flagged_by_meta_quality".to_string()),
            ),
            "PAUSED" => (WaTemplateStatus::Paused, info.rejected_reason),
            "DISABLED" => (WaTemplateStatus::Disabled, info.rejected_reason),
            "PENDING" | "IN_REVIEW" | "" => (WaTemplateStatus::Pending, None),
            other => {
                return Err(ApiError::Internal(format!(
                    "Meta devolvió un status desconocido: '{}'",
                    other
                )));
            }
        };
    let prev_status = doc.status;
    let prev_category = doc.category;
    let new_category = parse_meta_template_category(info.category.as_deref());

    let updated_doc = state
        .db
        .update_template(
            &oid,
            WaTemplateUpdatePatch {
                name: None,
                display_name: None,
                name_input: None,
                category: new_category,
                components: None,
                body_placeholders: None,
                status: Some(new_status),
                rejection_reason: Some(rejection_reason),
                meta_template_id: None,
                is_system: None,
                submit_to_meta: None,
            },
        )
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(template_not_found)?;

    let item = to_template_item(updated_doc);
    let category_changed = new_category.is_some_and(|cat| cat != prev_category);
    let status_changed = item.status != prev_status;

    if status_changed || category_changed {
        let payload = build_template_updated_event(
            &item,
            if status_changed {
                Some(prev_status)
            } else {
                None
            },
        );
        emit_to_phone_number_agents(&state, &item.phone_number_id, payload).await;
    }

    Ok(Json(WaTemplateResponse {
        ok: true,
        data: item,
    }))
}
