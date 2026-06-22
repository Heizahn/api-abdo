use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use mongodb::bson::oid::ObjectId;

use crate::modules::whatsapp::shared::{
    authz::{
        ensure_transfer_target_allowed_for_workspace, require_can_chat,
        require_workspace_actor_for_conversation,
    },
    mappers::{
        resolve_assigned_agent_name_one, resolve_customer_name, resolve_last_message_agent_name_one,
    },
    response::conv_to_item,
    service::resolve_service_for_phone,
    workspace::resolve_workspace_name,
};
use crate::modules::whatsapp::ws::{
    broadcast_all, broadcast_except, broadcast_to_chat_users, send_to_user,
    ConversacionNoLeidaData, WsServerEvent,
};
use crate::{
    auth::user_jwt::UserProfileClaims,
    db::WhatsAppRepository,
    error::ApiError,
    models::whatsapp::{
        ConversationDetailResponse, MarkReadData, MarkReadResponse, TakeConversationResponse,
        TransferConversationRequest, WaConversationEventInput,
    },
    state::AppState,
};

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct ResetAiStateResponse {
    pub ok: bool,
    pub conversation_id: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct InterveneData {
    pub conversation_id: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct InterveneResponse {
    pub ok: bool,
    pub data: InterveneData,
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/close",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Conversación cerrada", body = ConversationDetailResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn close_conversation_handler(
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

    // Capturar al agente ANTES de cerrar — `close_conversation` desasigna.
    let prev_agent = conv.assigned_to.clone();

    state
        .db
        .close_conversation(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    if let Some(agent) = prev_agent.as_deref() {
        state.redis.decr_agent_load(agent).await;
    }

    // Limpieza de counters AI por conversación al cerrar.
    state.redis.clear_ai_conv_counters(&id).await;

    let ev = WsServerEvent::ChatCerrado {
        conversation_id: id.clone(),
    };
    broadcast_all(&state.ws_registry, &ev).await;

    // EMIT BADGE: CONVERSACION_NO_LEIDA — cerrar puede bajar el conteo si había mensajes sin leer.
    if conv.unread_count > 0 {
        let pending_total = state.db.count_unread_conversations().await.unwrap_or(0);
        let unread_ev = WsServerEvent::ConversacionNoLeida {
            data: ConversacionNoLeidaData {
                pending_total,
                conversation_id: id.clone(),
                delta: -1,
            },
        };
        if let Ok(badge_payload) = serde_json::to_string(&unread_ev) {
            let _ = broadcast_to_chat_users(&state, badge_payload).await;
        }
    }

    if let Err(e) = state
        .db
        .record_conversation_event(WaConversationEventInput {
            conversation_id: &oid,
            business_phone: &conv.business_phone,
            event_type: "closed",
            actor_id: Some(claims.id.as_str()),
            actor_name: Some(claims.name.as_str()),
            target_id: None,
            target_name: None,
            note: None,
        })
        .await
    {
        tracing::warn!("record_conversation_event failed: {}", e);
    }

    let conv_after = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let opens = state
        .db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv_after.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv_after).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv_after).await;
    let assigned_name = resolve_assigned_agent_name_one(&state, &conv_after).await;

    Ok(Json(ConversationDetailResponse {
        ok: true,
        data: conv_to_item(
            conv_after,
            true,
            last_opened,
            workspace_name,
            resolved,
            agent_name,
            assigned_name,
        ),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/reopen",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación cerrada")),
    responses(
        (status = 200, description = "Conversación reabierta (status: pending, assigned_to: null) o detalle actual si ya estaba abierta (idempotente).", body = ConversationDetailResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "El usuario no tiene `bCanChat=true`"),
        (status = 404, description = "Conversación no encontrada"),
    )
)]
pub async fn reopen_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    // Pre-check de existencia: `reopen_conversation` sólo actúa si status==closed.
    // Distinguir "no existe" (404) de "ya abierta" (idempotente) requiere este paso.
    if state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }

    let reopened = state
        .db
        .reopen_conversation(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;

    let conv_after = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let opens = state
        .db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(ApiError::DatabaseError)?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv_after.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv_after).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv_after).await;
    let assigned_name = resolve_assigned_agent_name_one(&state, &conv_after).await;
    let business_phone_for_audit = conv_after.business_phone.clone();
    let conversation_item = conv_to_item(
        conv_after,
        true,
        last_opened,
        workspace_name,
        resolved,
        agent_name,
        assigned_name,
    );

    // Sólo emitimos el evento si realmente se reabrió (transición real).
    // Si era una llamada idempotente sobre una conv ya abierta, no disparamos
    // nada para no confundir a los otros clientes conectados.
    if reopened {
        // Reopen = arranque limpio: limpiamos counters AI por conv.
        state.redis.clear_ai_conv_counters(&id).await;

        let ev = WsServerEvent::ChatReabierto {
            conversation_id: id.clone(),
            conversation: conversation_item.clone(),
        };
        broadcast_all(&state.ws_registry, &ev).await;

        // Notificar al front que ai_conv_state fue limpiado (null = borrado).
        let ev_ia = WsServerEvent::ConversacionEstadoIa {
            conversation_id: id.clone(),
            ai_conv_state: None,
        };
        broadcast_all(&state.ws_registry, &ev_ia).await;

        if let Err(e) = state
            .db
            .record_conversation_event(WaConversationEventInput {
                conversation_id: &oid,
                business_phone: &business_phone_for_audit,
                event_type: "reopened",
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(claims.name.as_str()),
                target_id: None,
                target_name: None,
                note: None,
            })
            .await
        {
            tracing::warn!("record_conversation_event failed: {}", e);
        }
    }

    Ok(Json(ConversationDetailResponse {
        ok: true,
        data: conversation_item,
    }))
}

// ============================================
// RESET AI CONVERSATION STATE
// ============================================

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/agent-state/reset",
    tag = "WhatsApp — Conversaciones",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la conversación")),
    responses(
        (status = 200, description = "Estado IA reseteado", body = ResetAiStateResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat y rol supervisor"),
        (status = 404, description = "Conversación no encontrada"),
        (status = 409, description = "dispatch_in_progress — el dispatch está corriendo, reintentá en segundos"),
    )
)]
pub async fn reset_ai_conv_state_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<ResetAiStateResponse>, ApiError> {
    // Requiere bCanChat Y rol supervisor (superadmin / operador / contador).
    let caller = require_can_chat(&state, &claims.id).await?;
    if caller.role != 0.0 && caller.role != 0.5 && caller.role != 1.0 {
        return Err(ApiError::Forbidden);
    }

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    // Verificar que la conv existe.
    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // Acquire dispatch lock — DEBE mantenerse durante todo el reset (DB write + audit + WS)
    // para evitar que un dispatch concurrente sobrescriba el estado que estamos limpiando.
    if !state.redis.try_lock_ai_dispatch(&id).await {
        return Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "dispatch_in_progress",
            "El agente IA está procesando esta conversación. Reintentá en unos segundos.",
        ));
    }

    // Borrar el estado IA INSIDE the lock window.
    let write_result = state.db.update_conversation_ai_conv_state(&oid, None).await;

    // Auditoría también dentro del lock (mejor consistencia: si el write falló, no auditamos
    // un reset que no ocurrió).
    if write_result.is_ok() {
        if let Err(e) = state
            .db
            .record_conversation_event(WaConversationEventInput {
                conversation_id: &oid,
                business_phone: &conv.business_phone,
                event_type: "ai_state_reset",
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(claims.name.as_str()),
                target_id: None,
                target_name: None,
                note: Some("Reset manual del estado IA por supervisor"),
            })
            .await
        {
            tracing::warn!("record_conversation_event failed: {}", e);
        }
    }

    // Liberar el lock antes del broadcast (broadcast es best-effort, no necesita exclusión).
    state.redis.release_ai_dispatch_lock(&id).await;

    write_result.map_err(ApiError::DatabaseError)?;

    tracing::info!(
        "[ai_agent] ai_conv_state reset manual (conv={}, by={})",
        id,
        claims.id
    );

    // Broadcast WS: ai_conv_state = null (limpiado).
    let ev = WsServerEvent::ConversacionEstadoIa {
        conversation_id: id.clone(),
        ai_conv_state: None,
    };
    broadcast_all(&state.ws_registry, &ev).await;

    Ok(Json(ResetAiStateResponse {
        ok: true,
        conversation_id: id,
    }))
}

// ============================================
// INTERVENIR (take-over manual de IA → humano)
// ============================================

/// Take-over manual: el agente humano interrumpe a la IA y se queda con la
/// conversación. En un solo shot: asigna al caller, pasa a `in_progress` y
/// setea `ai_disabled=true`. CONSERVA `ai_active_agent_id` (pausa reversible).
/// Emite `CHAT_TOMADO` (assigned_to + status) + `IA_PAUSADA{reason:"manual"}`.
#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/intervene",
    tag = "WhatsApp — Conversaciones",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la conversación")),
    responses(
        (status = 200, description = "Take-over OK: conv asignada al caller, status in_progress, IA pausada", body = InterveneResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere bCanChat"),
        (status = 404, description = "conversation_not_found"),
        (status = 409, description = "ai_not_active (la IA no atiende esta conv) o dispatch_in_progress (turno IA en vuelo)"),
    )
)]
pub async fn intervene_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<InterveneResponse>, ApiError> {
    require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let existing = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "conversation_not_found",
                "Conversación no encontrada.",
            )
        })?;

    // Gate: la IA debe estar atendiendo (status=pending && !ai_disabled). Si ya
    // está pausada o un humano la tomó (in_progress) o está cerrada → ai_not_active.
    if existing.ai_disabled || existing.status != "pending" {
        return Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "ai_not_active",
            "La IA no está atendiendo esta conversación (ya pausada, tomada por un humano o cerrada).",
        ));
    }

    // Lock — evita pisarse con un dispatch en vuelo. Si está tomado, el turno IA
    // está corriendo: que el front reintente en unos segundos.
    if !state.redis.try_lock_ai_dispatch(&id).await {
        return Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "dispatch_in_progress",
            "El agente IA está procesando esta conversación. Reintentá en unos segundos.",
        ));
    }

    // Take-over atómico dentro del lock. ai_active_agent_id y aiConvState quedan intactos.
    let taken = state.db.intervene_conversation(&oid, &claims.id).await;

    state.redis.release_ai_dispatch_lock(&id).await;

    let conv = match taken {
        Ok(Some(c)) => c,
        // El filtro atómico no matcheó: otro actor cambió el estado entre el
        // pre-check y el lock. Para el caller es lo mismo que ai_not_active.
        Ok(None) => {
            return Err(ApiError::domain_simple(
                StatusCode::CONFLICT,
                "ai_not_active",
                "La IA dejó de atender esta conversación antes de la intervención.",
            ));
        }
        Err(e) => return Err(ApiError::DatabaseError(e)),
    };

    // La conv pasa a manos del caller → sube su carga (espejo de /take).
    state.redis.incr_agent_load(&claims.id).await;

    // Auditoría.
    if let Err(e) = state
        .db
        .record_conversation_event(WaConversationEventInput {
            conversation_id: &oid,
            business_phone: &conv.business_phone,
            event_type: "ai_intervened",
            actor_id: Some(claims.id.as_str()),
            actor_name: Some(claims.name.as_str()),
            target_id: Some(claims.id.as_str()),
            target_name: Some(claims.name.as_str()),
            note: Some("Take-over manual de conversación atendida por IA"),
        })
        .await
    {
        tracing::warn!("record_conversation_event failed: {}", e);
    }

    // WS — dos eventos, ambos broadcast_all (los handlers ya existen en el front):
    // 1) CHAT_TOMADO — assigned_to + status (patchea sidebar/cache).
    let ev_tomado = WsServerEvent::ChatTomado {
        conversation_id: id.clone(),
        taken_by: claims.id.clone(),
        taken_by_name: Some(claims.name.clone()),
        status: conv.status.clone(),
        previous_status: "pending".to_string(),
    };
    broadcast_all(&state.ws_registry, &ev_tomado).await;

    // 2) IA_PAUSADA — ai_disabled (actualiza el indicador IA del chat).
    let ev_pausada = WsServerEvent::IaPausada {
        conversation_id: id.clone(),
        reason: "manual".to_string(),
        by: claims.id.clone(),
    };
    broadcast_all(&state.ws_registry, &ev_pausada).await;

    tracing::info!(
        "[whatsapp] intervene manual (conv={}, by={})",
        id,
        claims.id
    );

    Ok(Json(InterveneResponse {
        ok: true,
        data: InterveneData {
            conversation_id: id,
        },
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/mark-read",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Mensajes marcados como leídos", body = MarkReadResponse),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn mark_read_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<MarkReadResponse>, ApiError> {
    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    // Actualizar status de inbound en DB y obtener los que cambiaron.
    // El `agent_id` queda persistido en `read_by_user_id` (first-read-wins)
    // para que la auditoría pueda atribuir el inbound a quien lo atendió.
    let changed_ids = state
        .db
        .mark_inbound_as_read(&oid, &claims.id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    // Capture old unread_count BEFORE reset (conv was fetched above).
    let old_unread = conv.unread_count;

    // Resetear contador local en la conversación.
    let _ = state.db.reset_unread(&oid).await;

    // EMIT BADGE: CONVERSACION_NO_LEIDA — only if there was something to clear.
    if old_unread > 0 {
        let pending_total = state.db.count_unread_conversations().await.unwrap_or(0);
        let unread_ev = WsServerEvent::ConversacionNoLeida {
            data: ConversacionNoLeidaData {
                pending_total,
                conversation_id: id.clone(),
                delta: -1,
            },
        };
        if let Ok(badge_payload) = serde_json::to_string(&unread_ev) {
            let _ = broadcast_to_chat_users(&state, badge_payload).await;
        }
    }

    // Notificar a Meta (ticks azules + mic azul en voice notes) para cada
    // inbound del batch. Meta NO propaga `read` a mensajes anteriores — en
    // particular, los audios sólo muestran el mic azul en el teléfono del
    // cliente si se llama `status: "read"` sobre ese `wa_message_id` puntual.
    // Best-effort: si falta credencial o Meta responde error, logueamos y
    // seguimos (no bloquea el endpoint, va en spawn).
    if !changed_ids.is_empty() {
        match resolve_service_for_phone(&state, &conv.business_phone).await {
            Ok(wa) => {
                let ids_to_ack = changed_ids.clone();
                let conv_hex = oid.to_hex();
                tokio::spawn(async move {
                    let mut ok = 0usize;
                    let mut err = 0usize;
                    for wamid in &ids_to_ack {
                        match wa.mark_as_read(wamid).await {
                            Ok(()) => ok += 1,
                            Err(e) => {
                                err += 1;
                                tracing::warn!(
                                    "[mark-read] Meta mark_as_read falló conv={} wamid={}: {}",
                                    conv_hex,
                                    wamid,
                                    e
                                );
                            }
                        }
                    }
                    tracing::debug!(
                        "[mark-read] Meta ACK conv={} total={} ok={} err={}",
                        conv_hex,
                        ids_to_ack.len(),
                        ok,
                        err
                    );
                });
            }
            Err(e) => {
                tracing::warn!("[mark-read] no se pudo resolver WhatsAppService: {:?}", e);
            }
        }
    }

    // Broadcast del batch. El front propaga `status: "read"` en la UI local.
    if !changed_ids.is_empty() {
        let ev = WsServerEvent::MensajesVistos {
            conversation_id: id.clone(),
            message_ids: changed_ids.clone(),
            status: "read".to_string(),
        };
        broadcast_all(&state.ws_registry, &ev).await;
    }

    Ok(Json(MarkReadResponse {
        ok: true,
        data: MarkReadData {
            message_ids: changed_ids,
        },
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/take",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    responses(
        (status = 200, description = "Conversación tomada, reasignada o reabierta. Acepta: `pending` (toma/reasignación, transiciona a `in_progress`) y `closed` (reopen+take, también transiciona a `in_progress`).", body = TakeConversationResponse),
        (status = 409, description = "La conversación no es tomable (está en `in_progress`)"),
        (status = 404, description = "Conversación no encontrada"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn take_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<TakeConversationResponse>, ApiError> {
    let actor = require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let existing = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    require_workspace_actor_for_conversation(&state, &actor, &existing.business_phone).await?;

    let previous_status = existing.status.clone();
    let prev_owner = existing.assigned_to.clone();
    let was_already_mine = prev_owner.as_deref() == Some(claims.id.as_str());

    // Sólo `pending` y `closed` son tomables. `in_progress` ya tiene dueño activo → 409.
    if previous_status != "pending" && previous_status != "closed" {
        return Err(ApiError::ConversationNotTakeable);
    }
    if previous_status == "pending"
        && !existing.ai_disabled
        && existing.ai_active_agent_id.is_some()
    {
        return Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "ai_active_use_intervene",
            "Esta conversación está siendo atendida por IA. Usá la intervención manual para tomarla.",
        ));
    }

    // `take_conversation` acepta `pending` (toma/reasignación) y `closed`
    // (reopen+take). En ambos casos el resultado queda en `in_progress`.
    let taken = state
        .db
        .take_conversation(&oid, &claims.id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let conv = match taken {
        Some(c) => c,
        None => return Err(ApiError::ConversationNotTakeable),
    };

    // Ajuste de carga: si había un dueño distinto a mí, le bajamos la carga.
    // Si yo no era dueño, me sube la carga.
    if !was_already_mine {
        state.redis.incr_agent_load(&claims.id).await;
        if let Some(prev) = prev_owner.as_deref() {
            if prev != claims.id {
                state.redis.decr_agent_load(prev).await;
            }
        }
    }

    // Resolver datos adicionales que van tanto en la respuesta HTTP como en el
    // evento WS (para que el resto de agentes vea la conversación actualizada).
    let opens = state
        .db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(ApiError::DatabaseError)?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv).await;
    // Acabamos de asignar la conv al `claims.id`, así que el `assigned_to_name`
    // es directamente el `claims.name` que ya tenemos del JWT (sin DB lookup).
    let assigned_name = Some(claims.name.clone());

    // Broadcast a los demás agentes según el estado previo y el dueño previo:
    // - `closed` → siempre CHAT_TOMADO con broadcast_all (el chat vuelve al mundo).
    // - `pending` sin dueño previo → CHAT_TOMADO con broadcast_except (toma nueva).
    // - `pending` con dueño distinto → CHAT_TRANSFERIDO (reasignación manual).
    // - `pending` ya era mío → idempotente, no emitir.
    if previous_status == "closed" {
        let ev = WsServerEvent::ChatTomado {
            conversation_id: id.clone(),
            taken_by: claims.id.clone(),
            taken_by_name: assigned_name.clone(),
            status: conv.status.clone(),
            previous_status: "closed".to_string(),
        };
        broadcast_all(&state.ws_registry, &ev).await;
        if let Err(e) = state
            .db
            .record_conversation_event(WaConversationEventInput {
                conversation_id: &oid,
                business_phone: &conv.business_phone,
                event_type: "taken",
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(claims.name.as_str()),
                target_id: Some(claims.id.as_str()),
                target_name: Some(claims.name.as_str()),
                note: Some("after_reopen"),
            })
            .await
        {
            tracing::warn!("record_conversation_event failed: {}", e);
        }
    } else if !was_already_mine {
        let conv_item = conv_to_item(
            conv.clone(),
            true,
            last_opened,
            workspace_name.clone(),
            resolved.clone(),
            agent_name.clone(),
            assigned_name.clone(),
        );
        let is_takeover = matches!(prev_owner.as_deref(), Some(prev) if prev != claims.id);
        let ev = if is_takeover {
            WsServerEvent::ChatTransferido {
                conversation_id: id.clone(),
                from_user_id: prev_owner.clone(),
                to_user_id: claims.id.clone(),
                conversation: conv_item,
            }
        } else {
            WsServerEvent::ChatTomado {
                conversation_id: id.clone(),
                taken_by: claims.id.clone(),
                taken_by_name: assigned_name.clone(),
                status: conv.status.clone(),
                previous_status: "pending".to_string(),
            }
        };
        // is_takeover: broadcast_all para que el agente destino también reciba
        // el status actualizado (`in_progress`) sin depender solo de la respuesta HTTP.
        // toma nueva: broadcast_except es suficiente (el tomador ya tiene la resp).
        if is_takeover {
            broadcast_all(&state.ws_registry, &ev).await;
            let json = serde_json::to_string(&ev).unwrap_or_default();
            send_to_user(&state.ws_registry, &claims.id, json).await;
            tracing::debug!("[take/takeover] targeted push sent to {}", claims.id);
        } else {
            broadcast_except(&state.ws_registry, &claims.id, &ev).await;
        }
        if let Err(e) = state
            .db
            .record_conversation_event(WaConversationEventInput {
                conversation_id: &oid,
                business_phone: &conv.business_phone,
                event_type: if is_takeover { "transferred" } else { "taken" },
                actor_id: Some(claims.id.as_str()),
                actor_name: Some(claims.name.as_str()),
                target_id: Some(claims.id.as_str()),
                target_name: Some(claims.name.as_str()),
                note: None,
            })
            .await
        {
            tracing::warn!("record_conversation_event failed: {}", e);
        }
    }

    Ok(Json(TakeConversationResponse {
        ok: true,
        data: conv_to_item(
            conv,
            true,
            last_opened,
            workspace_name,
            resolved,
            agent_name,
            assigned_name,
        ),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/transfer",
    tag = "WhatsApp — Soporte",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ID de la conversación")),
    request_body = TransferConversationRequest,
    responses(
        (status = 200, description = "Conversación transferida", body = ConversationDetailResponse),
        (status = 404, description = "Conversación o usuario destino no encontrado"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn transfer_conversation_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(payload): Json<TransferConversationRequest>,
) -> Result<Json<ConversationDetailResponse>, ApiError> {
    let actor = require_can_chat(&state, &claims.id).await?;

    let oid = ObjectId::parse_str(&id).map_err(|_| ApiError::BadRequest("id inválido".into()))?;

    let conv = state
        .db
        .find_conversation_by_id(&oid)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    let current_workspace =
        require_workspace_actor_for_conversation(&state, &actor, &conv.business_phone).await?;

    use crate::db::UserRepository;
    let target = state
        .db
        .find_user_by_id(&payload.user_id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or_else(|| ApiError::NotFound)?;
    ensure_transfer_target_allowed_for_workspace(&state, &target, current_workspace.id.as_ref())
        .await?;

    let from_agent = conv.assigned_to.clone();

    let conv_after = state
        .db
        .transfer_conversation(&oid, &payload.user_id)
        .await
        .map_err(|e| ApiError::DatabaseError(e))?
        .ok_or(ApiError::NotFound)?;

    if let Some(prev) = from_agent.as_deref() {
        if prev != payload.user_id {
            state.redis.decr_agent_load(prev).await;
        }
    }
    state.redis.incr_agent_load(&payload.user_id).await;

    if let Some(note) = payload.note.as_deref() {
        tracing::info!(
            "[transfer] conv={} de {:?} → {} por {} ({}): {}",
            id,
            from_agent,
            payload.user_id,
            claims.id,
            claims.name,
            note
        );
    }

    let opens = state
        .db
        .get_conversation_opens(&claims.id, &[oid])
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = resolve_workspace_name(&state, &conv_after.business_phone).await;
    let resolved = resolve_customer_name(&state, &conv_after).await;
    let agent_name = resolve_last_message_agent_name_one(&state, &conv_after).await;
    let assigned_name = resolve_assigned_agent_name_one(&state, &conv_after).await;
    let conv_item = conv_to_item(
        conv_after,
        true,
        last_opened,
        workspace_name,
        resolved,
        agent_name,
        assigned_name,
    );

    // Emitir tras tener el item listo — incluye el estado actualizado con workspace_name y assigned_to nuevo.
    let ev = WsServerEvent::ChatTransferido {
        conversation_id: id.clone(),
        from_user_id: from_agent.clone(),
        to_user_id: payload.user_id.clone(),
        conversation: conv_item.clone(),
    };
    broadcast_all(&state.ws_registry, &ev).await;
    let json = serde_json::to_string(&ev).unwrap_or_default();
    send_to_user(&state.ws_registry, &payload.user_id, json).await;
    tracing::debug!("[transfer] targeted push sent to {}", payload.user_id);

    if let Err(e) = state
        .db
        .record_conversation_event(WaConversationEventInput {
            conversation_id: &oid,
            business_phone: &conv.business_phone,
            event_type: "transferred",
            actor_id: Some(claims.id.as_str()),
            actor_name: Some(claims.name.as_str()),
            target_id: Some(payload.user_id.as_str()),
            target_name: Some(target.name.as_str()),
            note: payload.note.as_deref(),
        })
        .await
    {
        tracing::warn!("record_conversation_event failed: {}", e);
    }

    Ok(Json(ConversationDetailResponse {
        ok: true,
        data: conv_item,
    }))
}
