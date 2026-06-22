use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Extension, Json};
use mongodb::bson::{oid::ObjectId, DateTime};

use crate::{
    auth::user_jwt::UserProfileClaims,
    crypto::aes::decrypt_payload,
    db::{
        ProfileRepository, UserRepository, WaTemplateMediaRepository, WaTemplateRepository,
        WhatsAppRepository,
    },
    error::ApiError,
    models::whatsapp::*,
    state::AppState,
};

use crate::modules::whatsapp::{
    service::{MetaApiError, WhatsAppService},
    shared::{authz, mappers, response::conv_to_item, service::settings_secret, time::iso8601},
    ws::{broadcast_all, WsServerEvent},
};

/// INICIAR CONVERSACIÓN (agent outbound first)
#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/initiate",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    request_body = InitiateConversationRequest,
    responses(
        (status = 200, description = "Template enviado y conversación creada/reutilizada", body = SendMessageResponse),
        (status = 400, description = "Parámetros inválidos o template mal formado"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene permiso de chat o no pertenece al workspace"),
        (status = 404, description = "Workspace (business_phone_id) no encontrado"),
    )
    )]
pub async fn initiate_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(payload): Json<InitiateConversationRequest>,
) -> Result<Json<SendMessageResponse>, ApiError> {
    // Contrato explícito para frontend: distinguir "sin permiso de chat"
    // de otros 403 del flujo de workspace.
    let caller = {
        let user = state
            .db
            .find_user_by_id(&claims.id)
            .await
            .map_err(ApiError::DatabaseError)?
            .ok_or_else(|| {
                ApiError::domain_simple(
                    StatusCode::FORBIDDEN,
                    "whatsapp_chat_permission_required",
                    "Usuario no autorizado para mensajeria WhatsApp",
                )
            })?;
        if user.role != 0.0 && !user.can_chat {
            return Err(ApiError::domain_simple(
                StatusCode::FORBIDDEN,
                "whatsapp_chat_permission_required",
                "Este usuario requiere can_chat=true para iniciar conversaciones",
            ));
        }
        user
    };
    let business_phone_id = payload.business_phone_id.trim().to_string();
    tracing::debug!(
        user_id = %caller.id,
        business_phone_id = %business_phone_id,
        expected_field = "WaSettings._id (ObjectId hex de 24 chars)",
        lookup = "find_wa_settings_by_id({_id: ObjectId(...)})",
        "initiate: validando workspace emisor para template outbound"
    );
    let workspace_oid = ObjectId::parse_str(&business_phone_id).map_err(|_| {
        tracing::warn!(
            user_id = %caller.id,
            business_phone_id = %business_phone_id,
            expected_field = "WaSettings._id (ObjectId hex de 24 chars)",
            hint = "No enviar WaSettings.phone_number_id (Meta) ni phone E.164",
            "initiate: business_phone_id invalido para parse ObjectId"
        );
        ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "whatsapp_workspace_id_invalid",
            "business_phone_id",
            "business_phone_id debe ser el _id del workspace (ObjectId hex)",
        )
    })?;

    let settings_opt = state
        .db
        .find_wa_settings_by_id(&workspace_oid)
        .await
        .map_err(ApiError::DatabaseError)?;
    let settings = match settings_opt {
        Some(s) => s,
        None => {
            tracing::warn!(
                user_id = %caller.id,
                workspace_id = %workspace_oid.to_hex(),
                lookup = "find_wa_settings_by_id({_id: ObjectId(...)})",
                "initiate: workspace no encontrado por _id"
            );
            return Err(ApiError::domain_with_field(
                StatusCode::NOT_FOUND,
                "whatsapp_workspace_not_found",
                "business_phone_id",
                "No existe workspace para el business_phone_id enviado",
            ));
        }
    };
    tracing::debug!(
        user_id = %caller.id,
        workspace_id = %workspace_oid.to_hex(),
        phone_number_id = %settings.phone_number_id,
        active = settings.active,
        agents_count = settings.agents.len(),
        "initiate: workspace resuelto para envio de template"
    );

    if !authz::is_superadmin(&caller) {
        let caller_workspaces = state
            .db
            .get_user_workspaces(&caller.id)
            .await
            .map_err(ApiError::DatabaseError)?;

        if caller_workspaces.is_empty()
            || !caller_workspaces
                .iter()
                .any(|workspace_id| workspace_id == &workspace_oid)
        {
            tracing::warn!(
                user_id = %caller.id,
                workspace_id = %workspace_oid.to_hex(),
                assigned_workspaces_count = caller_workspaces.len(),
                "initiate: usuario autenticado no pertenece al workspace requerido"
            );
            return Err(ApiError::domain_simple(
                StatusCode::FORBIDDEN,
                "whatsapp_workspace_membership_required",
                "No tienes permiso sobre este workspace",
            ));
        }
    }

    if !settings.active {
        tracing::warn!(
            user_id = %caller.id,
            workspace_id = %workspace_oid.to_hex(),
            "initiate: workspace inactivo"
        );
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "whatsapp_workspace_inactive",
            "El workspace esta inactivo",
        ));
    }

    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        tracing::warn!(
            user_id = %caller.id,
            workspace_id = %workspace_oid.to_hex(),
            phone_number_id_empty = settings.phone_number_id.is_empty(),
            access_token_empty = settings.access_token.is_empty(),
            "initiate: workspace sin credenciales WhatsApp completas"
        );
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "whatsapp_workspace_credentials_missing",
            "Workspace sin phone_number_id o access_token configurados",
        ));
    }

    let mut tpl = payload.template;
    // Owned para que el borrow de `tpl.name`/`tpl.language` se libere antes
    // del `&mut tpl.components` que necesita `auto_fill_template_header_media`.
    let tpl_name: String = tpl.name.trim().to_string();
    let tpl_lang: String = tpl.language.trim().to_string();
    if tpl_name.is_empty() || tpl_lang.is_empty() {
        return Err(ApiError::MissingTemplateParams);
    }

    let idempotency_key = payload.idempotency_key.trim().to_string();
    if idempotency_key.is_empty() {
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "whatsapp_idempotency_key_required",
            "idempotency_key",
            "idempotency_key es requerido",
        ));
    }

    let to = normalize_to_e164(&payload.to);
    if to.is_empty() {
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "whatsapp_recipient_invalid",
            "to",
            "Numero de destino invalido. Usa formato internacional, ej: 58414XXXXXXX",
        ));
    }

    // Linkear con cliente ISP si el teléfono matchea (best-effort — el link
    // sirve para mostrar datos del cliente en la UI, no bloquea el envío).
    let client_id = {
        let clients = state
            .db
            .find_clients_by_phone(&to)
            .await
            .ok()
            .and_then(|list| list.into_iter().next().map(|c| c._id));
        clients
    };

    // Upsert conversación. El nombre lo dejamos en None — si hay inbound
    // posterior, Meta lo trae y se actualiza automáticamente.
    let (conv, conv_created) = state
        .db
        .upsert_conversation(&to, &settings.phone, None)
        .await
        .map_err(ApiError::DatabaseError)?;
    let conv_id = conv
        .id
        .ok_or_else(|| ApiError::Internal("conversación sin _id tras upsert".into()))?;

    // Outbound first → registrar `created` con el agente como actor.
    if conv_created {
        let input = WaConversationEventInput {
            conversation_id: &conv_id,
            business_phone: &conv.business_phone,
            event_type: "created",
            actor_id: Some(claims.id.as_str()),
            actor_name: Some(claims.name.as_str()),
            target_id: None,
            target_name: None,
            note: Some("outbound_initiate"),
        };
        if let Err(e) = state.db.record_conversation_event(input).await {
            tracing::warn!("record_conversation_event failed: {}", e);
        }
    }

    // Engagement throttle (Meta error 131049): si la conversación ya está en
    // cooldown, no llamamos a la Cloud API — Meta rechazaría igual y gastaríamos
    // request. Mismo error que `resolve_send_mode` para uniformidad en el front.
    if let Some(until) = conv.meta_throttle_until {
        let now_ms = DateTime::now().timestamp_millis();
        if until.timestamp_millis() > now_ms {
            return Err(ApiError::Domain {
                status: StatusCode::CONFLICT,
                code: "template_throttled_by_meta".into(),
                field: None,
            message: "Meta bloqueó los envíos a este contacto temporalmente (recibió demasiados mensajes sin responder). Espera a que responda o vuelve a intentarlo más tarde.".into(),
                details: Some(serde_json::json!({
                    "meta_throttle_until": iso8601(until),
                })),
            });
        }
    }

    // Si se creó nueva y matcheó cliente, persistir el link. No reescribimos
    // client_id en conversaciones existentes para no pisar un link manual.
    if conv_created {
        if let Some(cid) = client_id {
            if let Err(e) = state.db.update_conversation_client_id(&conv_id, &cid).await {
                tracing::warn!("initiate: no se pudo vincular client_id: {}", e);
            }
        }
    }

    // Asignar al iniciador si la conversación no tiene dueño. Esto evita que
    // el auto-assign la reasigne a otro agente al primer inbound.
    if conv.assigned_to.is_none() {
        if let Err(e) = state
            .db
            .assign_conversation(&conv_id, Some(&claims.id))
            .await
        {
            tracing::warn!("initiate: assign_conversation error: {}", e);
        } else {
            state.redis.incr_agent_load(&claims.id).await;
        }
    }

    // Idempotencia: si ya existe un mensaje con la misma key para esta
    // conversación, devolverlo sin re-enviar (salvo que esté `failed`).
    if let Some(existing) = state
        .db
        .find_message_by_idempotency(&conv_id, &idempotency_key)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        let is_failed = existing.status.as_deref() == Some("failed");
        if !is_failed {
            let rto = mappers::resolve_reply_to_for_one(&state, &existing).await;
            let item = mappers::msg_to_item(existing, Some(claims.name.clone()), rto);
            return Ok(Json(SendMessageResponse {
                ok: true,
                data: SendMessageData {
                    message_id: item.id.clone(),
                    message: item,
                },
            }));
        }
    }

    // Descifrar access_token y construir el cliente Meta.
    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;
    let wa = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id.clone(),
        token,
    );

    // Auto-fill del HEADER si la plantilla tiene IMAGE/VIDEO y el front no lo mandó.
    auto_fill_template_header_media(
        &state,
        &mut tpl.components,
        &tpl_name,
        &tpl_lang,
        &settings.phone,
        &wa,
    )
    .await?;

    let components_value = tpl
        .components
        .as_ref()
        .map(|v| serde_json::Value::Array(v.clone()));
    let wa_id = wa
        .send_template(&to, &tpl_name, &tpl_lang, components_value.as_ref())
        .await
        .map_err(|e| map_template_send_error(&e))?;

    let preview = tpl
        .rendered_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("[plantilla: {}]", tpl_name));

    let msg = WaMessage {
        id: None,
        conversation_id: conv_id,
        wa_message_id: wa_id,
        direction: "out".to_string(),
        msg_type: "template".to_string(),
        body: Some(preview.clone()),
        media_id: None,
        media_mime_type: None,
        media_filename: None,
        status: Some("sent".to_string()),
        meta_error_code: None,
        meta_error_title: None,
        meta_error_message: None,
        meta_error_details: None,
        failed_at: None,
        sent_by: Some(claims.id.clone()),
        source: None,
        campaign_id: None,
        campaign_recipient_id: None,
        read_by_user_id: None,
        read_at: None,
        idempotency_key: Some(idempotency_key),
        reply_to_wa_message_id: None,
        is_forwarded: None,
        is_frequently_forwarded: None,
        url_preview: None,
        voice: false,
        template_name: Some(tpl_name.to_string()),
        template_language: Some(tpl_lang.to_string()),
        template_components: components_value,
        interactive_payload: None,
        contacts_payload: None,
        location: None,
        reactions: vec![],
        raw_payload: None,
        audio_transcription: None,
        ai_processed_at: None,
        timestamp: DateTime::now(),
    };

    let saved = state
        .db
        .save_message(msg)
        .await
        .map_err(ApiError::DatabaseError)?;
    let touch = crate::db::ConversationTouch {
        preview: &preview,
        msg_type: &saved.msg_type,
        direction: "out",
        wa_message_id: &saved.wa_message_id,
        from_user_id: Some(claims.id.as_str()),
        media_filename: saved.media_filename.as_deref(),
        status: Some("sent"),
        increment_unread: false,
        last_message_at: None,
    };
    state
        .db
        .touch_conversation(&conv_id, touch)
        .await
        .map_err(ApiError::DatabaseError)?;

    // Releer para emitir `ConversacionNueva` con el estado final (assigned_to,
    // client_id, etc).
    let conv_now = state
        .db
        .find_conversation_by_id(&conv_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .unwrap_or(conv);

    let rto = mappers::resolve_reply_to_for_one(&state, &saved).await;
    let item = mappers::msg_to_item(saved, Some(claims.name.clone()), rto);

    if conv_created {
        let ws_name = Some(settings.workspace_name.clone()).filter(|w| !w.is_empty());
        let resolved = mappers::resolve_customer_name(&state, &conv_now).await;
        // El último mensaje es el template recién enviado por `claims`, así que
        // el nombre del agente sale del token — evitamos un round-trip a Users.
        let agent_name = Some(claims.name.clone());
        // assigned_to acá es claims.id (template envía con el agente como dueño).
        let assigned_name = conv_now.assigned_to.as_ref().map(|_| claims.name.clone());
        let new_ev = WsServerEvent::ConversacionNueva {
            conversation: conv_to_item(
                conv_now,
                false,
                None,
                ws_name,
                resolved,
                agent_name,
                assigned_name,
            ),
        };
        broadcast_all(&state.ws_registry, &new_ev).await;
    }

    let msg_ev = WsServerEvent::MensajeNuevo {
        conversation_id: conv_id.to_hex(),
        message: item.clone(),
    };
    broadcast_all(&state.ws_registry, &msg_ev).await;

    Ok(Json(SendMessageResponse {
        ok: true,
        data: SendMessageData {
            message_id: item.id.clone(),
            message: item,
        },
    }))
}

// ============================================
// HELPERS — INITIATION
// ============================================

/// Si el front no incluyó componente HEADER en `components` y la plantilla
/// guardada en nuestra DB tiene header `IMAGE` o `VIDEO`, levanta el binario
/// del GridFS, lo sube a la Cloud Media API de Meta y mete el componente
/// HEADER al inicio del array.
///
/// **NO aplica para `DOCUMENT`** — los documentos típicamente cambian por
/// envío (recibos, facturas, comprobantes) y deben venir explícitos del front.
/// **NO aplica para `TEXT`** — Meta no exige `parameters` para headers TEXT
/// sin placeholder. Si tiene placeholder, el front manda el HEADER explícito.
///
/// No-ops si: ya hay HEADER en `components`, la plantilla no está en nuestra
/// DB, no se encuentra el binario en GridFS, o el `header_handle` no es un
/// ObjectId nuestro (caso de plantilla migrada con handle Meta legacy).
pub(crate) async fn auto_fill_template_header_media(
    state: &Arc<AppState>,
    components: &mut Option<Vec<serde_json::Value>>,
    template_name: &str,
    template_language: &str,
    business_phone: &str,
    wa: &WhatsAppService,
) -> Result<(), ApiError> {
    // ¿Ya tiene HEADER? Passthrough — el front quiso personalizar.
    let has_header = components.as_deref().unwrap_or(&[]).iter().any(|c| {
        c.get("type")
            .and_then(|v| v.as_str())
            .map(|t| t.eq_ignore_ascii_case("HEADER"))
            .unwrap_or(false)
    });
    if has_header {
        return Ok(());
    }

    // Resolver phone_number_id desde business_phone
    let settings = match state
        .db
        .find_wa_settings_by_phone(business_phone)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        Some(s) => s,
        None => return Ok(()),
    };

    // Buscar plantilla en nuestra DB
    let doc = match state
        .db
        .find_template_by_phone_name_lang(
            &settings.phone_number_id,
            template_name,
            template_language,
        )
        .await
        .map_err(ApiError::DatabaseError)?
    {
        Some(d) => d,
        None => return Ok(()),
    };

    // Buscar componente HEADER en los components guardados
    let header_comp = match doc.components.iter().find(|c| {
        c.get("type")
            .and_then(|v| v.as_str())
            .map(|t| t.eq_ignore_ascii_case("HEADER"))
            .unwrap_or(false)
    }) {
        Some(c) => c,
        None => return Ok(()),
    };

    // Sólo IMAGE y VIDEO se auto-rellenan
    let format = header_comp
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();
    let media_kind: &str = match format.as_str() {
        "IMAGE" => "image",
        "VIDEO" => "video",
        _ => return Ok(()),
    };

    // Extraer ObjectId nuestro de example.header_handle[0]
    let our_media_id = match header_comp
        .pointer("/example/header_handle/0")
        .and_then(|v| v.as_str())
    {
        Some(s) => s,
        None => return Ok(()),
    };
    let oid = match ObjectId::parse_str(our_media_id) {
        Ok(o) => o,
        Err(_) => return Ok(()),
    };

    // Leer binario del GridFS
    let (bytes, mime) = match state
        .db
        .read_template_media_bytes(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        Some(t) => t,
        None => {
            return Err(ApiError::Internal(format!(
                "Template {} tiene header media {} pero el binario no existe en GridFS",
                template_name, our_media_id
            )))
        }
    };

    // Upload a la Cloud Media API de Meta (≠ Resumable Upload del approval)
    let meta_media_id = wa.upload_media(bytes, &mime, None).await.map_err(|e| {
        ApiError::Internal(format!(
            "auto-fill: falló upload del header media a Meta: {}",
            e
        ))
    })?;

    // Construir el componente HEADER.
    let mut media_obj = serde_json::Map::new();
    media_obj.insert("id".to_string(), serde_json::Value::String(meta_media_id));

    let mut param = serde_json::Map::new();
    param.insert(
        "type".to_string(),
        serde_json::Value::String(media_kind.to_string()),
    );
    param.insert(media_kind.to_string(), serde_json::Value::Object(media_obj));

    let header_param = serde_json::json!({
        "type": "HEADER",
        "parameters": [serde_json::Value::Object(param)]
    });

    let mut comps = components.take().unwrap_or_default();
    comps.insert(0, header_param);
    *components = Some(comps);

    Ok(())
}

pub(crate) fn map_template_send_error(err: &anyhow::Error) -> ApiError {
    if let Some(me) = err.downcast_ref::<MetaApiError>() {
        let details = serde_json::json!({
            "meta_error_code": me.code.to_string(),
            "meta_error_message": me.message,
            "meta_error_subcode": me.error_subcode,
            "meta_error_user_msg": me.error_user_msg,
        });

        if me.code == 131049 {
            return ApiError::domain_with_details(
                StatusCode::CONFLICT,
                "template_throttled_by_meta",
                "Meta bloqueo temporalmente los envios a este contacto por demasiados mensajes sin respuesta. Espera a que responda o intenta mas tarde.",
                details,
            );
        }

        return ApiError::domain_with_details(
            StatusCode::BAD_GATEWAY,
            "meta_rejected",
            "Meta rechazo el envio de la plantilla. Revisa que este aprobada, el idioma, los parametros y el numero destino.",
            details,
        );
    }

    ApiError::domain_with_details(
        StatusCode::BAD_GATEWAY,
        "meta_rejected",
        "Meta rechazo el envio de la plantilla o no devolvio una respuesta valida.",
        serde_json::json!({
            "meta_error_code": "0",
            "meta_error_message": err.to_string(),
        }),
    )
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
