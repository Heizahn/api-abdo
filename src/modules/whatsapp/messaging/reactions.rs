use std::sync::Arc;

use axum::extract::{Extension, Json, Path, State};
use axum::http::StatusCode;
use mongodb::bson::oid::ObjectId;

use crate::auth::user_jwt::UserProfileClaims;
use crate::db::WhatsAppRepository;
use crate::error::ApiError;
use crate::models::whatsapp::InboundMessage;
use crate::modules::whatsapp::shared;
use crate::modules::whatsapp::ws::{broadcast_all, WsServerEvent};
use crate::state::AppState;

/// REACCIONES A MENSAJES
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct ReactMessageRequest {
    /// Emoji crudo (ej: "👍", "❤️"). Cadena vacía `""` significa "remover mi reacción".
    pub emoji: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct ReactMessageResponse {
    pub ok: bool,
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/messages/{id}/react",
    tag = "WhatsApp — Messages",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId del WaMessage al que se reacciona")),
    request_body = ReactMessageRequest,
    responses(
        (status = 200, description = "Reacción aplicada", body = ReactMessageResponse),
        (status = 400, description = "id inválido o payload malformado"),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "Mensaje no encontrado"),
        (status = 409, description = "reaction_window_expired — ventana de 24h expirada"),
        (status = 502, description = "meta_upstream_error — Meta rechazó la reacción"),
    )
)]
pub async fn react_message_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(body): Json<ReactMessageRequest>,
) -> Result<Json<ReactMessageResponse>, ApiError> {
    // 1. Parsear ObjectId
    let oid = ObjectId::parse_str(&id).map_err(|_| {
        ApiError::domain_simple(StatusCode::BAD_REQUEST, "invalid_id", "id inválido")
    })?;

    // 2. Cargar el WaMessage target
    let message = state
        .db
        .find_message_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "message_not_found",
                "Mensaje no encontrado",
            )
        })?;

    // 3. Cargar la conversación para obtener customer_phone + business_phone
    let conv = state
        .db
        .find_conversation_by_id(&message.conversation_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "conversation_not_found",
                "Conversación no encontrada",
            )
        })?;

    // 4. Resolver WhatsAppService para el business_phone
    let wa = shared::service::resolve_service_for_phone(&state, &conv.business_phone).await?;

    // 5. Llamar a Meta (Meta acepta emoji vacío para remover)
    wa.send_reaction(&conv.phone, &message.wa_message_id, &body.emoji)
        .await?;

    // 6. Aplicar update en DB (sólo si Meta aceptó)
    let updated = state
        .db
        .update_message_reactions(
            &message.wa_message_id,
            "agent",
            &body.emoji,
            Some(&claims.name),
        )
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "message_not_found",
                "Mensaje desapareció entre lookup y update",
            )
        })?;

    // 7. Broadcast WS
    let event = WsServerEvent::ReaccionMensaje {
        conversation_id: updated.conversation_id.to_hex(),
        message_id: updated.id.map(|o| o.to_hex()).unwrap_or_default(),
        wa_message_id: updated.wa_message_id.clone(),
        emoji: body.emoji.clone(),
        from: "agent".to_string(),
        sender_name: Some(claims.name.clone()),
    };
    broadcast_all(&state.ws_registry, &event).await;

    Ok(Json(ReactMessageResponse { ok: true }))
}

pub(crate) async fn handle_inbound_reaction(state: &Arc<AppState>, msg: &InboundMessage) -> bool {
    let reaction = match &msg.reaction {
        Some(v) => v,
        None => {
            tracing::warn!(
                "[webhook] reaction sin payload, ignorando: from={}",
                msg.from
            );
            return true;
        }
    };

    let target_wamid = reaction
        .get("message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let emoji = reaction.get("emoji").and_then(|v| v.as_str()).unwrap_or("");
    if target_wamid.is_empty() {
        tracing::warn!("[webhook] reaction sin message_id, ignorando");
        return true;
    }

    match state
        .db
        .update_message_reactions(target_wamid, "customer", emoji, None)
        .await
    {
        Ok(Some(updated)) => {
            let event = WsServerEvent::ReaccionMensaje {
                conversation_id: updated.conversation_id.to_hex(),
                message_id: updated.id.map(|o| o.to_hex()).unwrap_or_default(),
                wa_message_id: target_wamid.to_string(),
                emoji: emoji.to_string(),
                from: "customer".to_string(),
                sender_name: None,
            };
            broadcast_all(&state.ws_registry, &event).await;
        }
        Ok(None) => {
            tracing::debug!(
                "[webhook] reaction sobre wamid desconocido (ignorada): {}",
                target_wamid
            );
        }
        Err(e) => {
            tracing::error!("[webhook] update_message_reactions error: {}", e);
        }
    }

    true
}
