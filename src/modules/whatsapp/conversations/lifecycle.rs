use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use mongodb::bson::oid::ObjectId;

use crate::modules::whatsapp::shared::{
    authz::{require_can_chat, require_workspace_actor_for_conversation},
    mappers::{resolve_customer_name, resolve_last_message_agent_name_one},
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
        MarkReadData, MarkReadResponse, TakeConversationResponse, WaConversationEventInput,
    },
    state::AppState,
};

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
