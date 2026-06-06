use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use mongodb::bson::oid::ObjectId;

use crate::modules::whatsapp::shared::service::resolve_service_for_phone;
use crate::modules::whatsapp::ws::{
    broadcast_all, broadcast_to_chat_users, ConversacionNoLeidaData, WsServerEvent,
};
use crate::{
    auth::user_jwt::UserProfileClaims,
    db::WhatsAppRepository,
    error::ApiError,
    models::whatsapp::{MarkReadData, MarkReadResponse},
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
