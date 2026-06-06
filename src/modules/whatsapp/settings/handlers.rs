use std::sync::Arc;

use serde_json;

use axum::{
    extract::{Path, State},
    Json,
};
use mongodb::bson::{oid::ObjectId, DateTime};

use crate::{
    crypto::aes::{decrypt_payload, encrypt_payload},
    db::WhatsAppRepository,
    error::ApiError,
    models::whatsapp::*,
    state::AppState,
};

use super::validation::{normalize_to_e164, validate_access_token};
use crate::modules::whatsapp::{
    handler::map_meta_error,
    service::WhatsAppService,
    shared::{apply_media_relay, response::settings_to_item, settings_secret},
    ws::{broadcast_to_chat_users, ConversacionNoLeidaData, TicketPendienteData, WsServerEvent},
};

// ============================================
// SETTINGS — Configuración de números y agentes
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/settings",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Lista de configuraciones", body = SettingsListResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_settings_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SettingsListResponse>, ApiError> {
    let items = state
        .db
        .get_all_wa_settings()
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    Ok(Json(SettingsListResponse {
        ok: true,
        data: items.into_iter().map(settings_to_item).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/settings",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = CreateSettingsRequest,
    responses(
        (status = 200, description = "Configuración creada", body = SettingsResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn create_settings_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateSettingsRequest>,
) -> Result<Json<SettingsResponse>, ApiError> {
    let phone = normalize_to_e164(&payload.phone);
    let now = DateTime::now();

    let access_token = validate_access_token(&payload.access_token)?;
    if payload.phone_number_id.trim().is_empty() {
        return Err(ApiError::BadRequest("phone_number_id requerido".into()));
    }
    let waba_id = payload.whatsapp_business_account_id.trim().to_string();
    if waba_id.is_empty() {
        return Err(ApiError::BadRequest(
            "whatsapp_business_account_id requerido".into(),
        ));
    }

    let encrypted = encrypt_payload(&settings_secret(), access_token);

    let doc = WaSettings {
        id: None,
        phone,
        workspace_name: payload.workspace_name,
        phone_number_id: payload.phone_number_id,
        whatsapp_business_account_id: waba_id,
        access_token: encrypted,
        agents: payload.agents,
        active: true,
        purposes: payload.purposes.unwrap_or_default(),
        templates_synced_at: None,
        enable_guardrails: true,
        enable_conversation_state: true,
        pre_classifier_enabled: false,
        trivial_responses: Vec::new(),
        created_at: now,
        updated_at: now,
    };

    let created = state
        .db
        .create_wa_settings(doc)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    emit_chat_badges_refresh(&state, "settings_created").await;

    Ok(Json(SettingsResponse {
        ok: true,
        data: settings_to_item(created),
    }))
}

#[utoipa::path(
    put,
    path = "/v1/auth-user/whatsapp/settings/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la configuración")),
    request_body = UpdateSettingsRequest,
    responses(
        (status = 200, description = "Configuración actualizada", body = UpdateResponse),
        (status = 404, description = "No encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn update_settings_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateSettingsRequest>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let encrypted_token = match payload.access_token.as_deref() {
        Some(raw) if !raw.trim().is_empty() => {
            let clean = validate_access_token(raw)?;
            Some(encrypt_payload(&settings_secret(), clean))
        }
        _ => None,
    };

    let waba = payload.whatsapp_business_account_id.and_then(|v| {
        let t = v.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    });

    state
        .db
        .update_wa_settings(
            &oid,
            payload.workspace_name,
            payload.phone_number_id,
            waba,
            encrypted_token,
            payload.agents,
            payload.active,
            payload.purposes,
            payload.enable_guardrails,
            payload.enable_conversation_state,
            payload.pre_classifier_enabled,
            payload.trivial_responses,
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    emit_chat_badges_refresh(&state, "settings_updated").await;

    Ok(Json(UpdateResponse { ok: true }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/settings/{id}",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la configuración")),
    responses(
        (status = 200, description = "Configuración eliminada", body = UpdateResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn delete_settings_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<UpdateResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    state
        .db
        .delete_wa_settings(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    emit_chat_badges_refresh(&state, "settings_deleted").await;

    Ok(Json(UpdateResponse { ok: true }))
}

// ============================================
// TEST CONNECTION (verificación contra Meta)
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/settings/test-connection",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = WaTestConnectionRequest,
    responses(
        (status = 200, description = "Credenciales válidas", body = WaTestConnectionResponse),
        (status = 400, description = "phone_number_id o access_token faltante / inválido"),
        (status = 401, description = "No autorizado"),
        (status = 502, description = "Meta rechazó las credenciales"),
    )
)]
pub async fn test_settings_connection_raw_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<WaTestConnectionRequest>,
) -> Result<Json<WaTestConnectionResponse>, ApiError> {
    let phone_number_id = payload
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::BadRequest("phone_number_id requerido".into()))?
        .to_string();

    let token_raw = payload
        .access_token
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("access_token requerido".into()))?;

    let token = validate_access_token(token_raw)?.to_string();

    let svc = apply_media_relay(
        &state,
        WhatsAppService::new(state.reqwest_client.clone(), phone_number_id.clone(), token),
    );

    let info = svc
        .test_phone_number()
        .await
        .map_err(|e| map_meta_error(&e, "no se pudo validar las credenciales contra Meta"))?;

    Ok(Json(WaTestConnectionResponse {
        ok: true,
        data: WaTestConnectionData {
            reachable: true,
            phone_number_id: info.id,
            verified_name: info.verified_name,
            display_phone_number: info.display_phone_number,
            source: WaTestConnectionSource::Body,
        },
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/settings/{id}/test-connection",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID del WaSettings a re-validar")),
    request_body = WaTestConnectionRequest,
    responses(
        (status = 200, description = "Credenciales válidas", body = WaTestConnectionResponse),
        (status = 400, description = "id inválido o access_token de override mal formado"),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "WaSettings no encontrado"),
        (status = 502, description = "Meta rechazó las credenciales"),
    )
)]
pub async fn test_settings_connection_stored_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<WaTestConnectionRequest>,
) -> Result<Json<WaTestConnectionResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let settings = state
        .db
        .find_wa_settings_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let phone_override = payload
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let phone_number_id = phone_override
        .clone()
        .unwrap_or_else(|| settings.phone_number_id.clone());
    if phone_number_id.is_empty() {
        return Err(ApiError::BadRequest(
            "phone_number_id no configurado y no se envió override".into(),
        ));
    }

    let token_override = match payload.access_token.as_deref() {
        Some(raw) if !raw.trim().is_empty() => Some(validate_access_token(raw)?.to_string()),
        _ => None,
    };

    let token = match token_override.as_ref() {
        Some(t) => t.clone(),
        None => {
            if settings.access_token.is_empty() {
                return Err(ApiError::BadRequest(
                    "access_token no guardado y no se envió override".into(),
                ));
            }
            decrypt_payload(&settings_secret(), &settings.access_token)
                .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?
        }
    };

    let source = if phone_override.is_some() || token_override.is_some() {
        WaTestConnectionSource::Body
    } else {
        WaTestConnectionSource::Stored
    };

    let svc = apply_media_relay(
        &state,
        WhatsAppService::new(state.reqwest_client.clone(), phone_number_id.clone(), token),
    );

    let info = svc
        .test_phone_number()
        .await
        .map_err(|e| map_meta_error(&e, "no se pudo validar las credenciales contra Meta"))?;

    Ok(Json(WaTestConnectionResponse {
        ok: true,
        data: WaTestConnectionData {
            reachable: true,
            phone_number_id: info.id,
            verified_name: info.verified_name,
            display_phone_number: info.display_phone_number,
            source,
        },
    }))
}

async fn emit_chat_badges_refresh(state: &Arc<AppState>, reason: &str) {
    let unread_total = state.db.count_unread_conversations().await.unwrap_or(0);
    let unread_event = WsServerEvent::ConversacionNoLeida {
        data: ConversacionNoLeidaData {
            pending_total: unread_total,
            conversation_id: format!("__refresh__:{reason}"),
            delta: 0,
        },
    };

    if let Ok(payload) = serde_json::to_string(&unread_event) {
        let _ = broadcast_to_chat_users(state, payload).await;
    }

    let tickets_total = state.db.count_open_tickets().await.unwrap_or(0);
    let tickets_event = WsServerEvent::TicketPendiente {
        data: TicketPendienteData {
            pending_total: tickets_total,
            ticket_id: format!("__refresh__:{reason}"),
            previous_status: None,
            new_status: "refresh".to_string(),
        },
    };

    if let Ok(payload) = serde_json::to_string(&tickets_event) {
        let _ = broadcast_to_chat_users(state, payload).await;
    }
}
