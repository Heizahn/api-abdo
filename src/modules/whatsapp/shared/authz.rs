use std::sync::Arc;

use axum::http::StatusCode;
use mongodb::bson::oid::ObjectId;

use crate::{
    db::{UserRepository, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::WaSettings,
    state::AppState,
};

/// Exige `bCanChat == true` (o `nRole == 0`, super admin) y devuelve el `User`
/// completo para que el caller tenga el rol sin re-consultar DB.
pub(crate) async fn require_can_chat(
    state: &Arc<AppState>,
    user_id: &str,
) -> Result<crate::models::users::User, ApiError> {
    let user = state
        .db
        .find_user_by_id(user_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::Forbidden)?;

    if user.role != 0.0 && !user.can_chat {
        return Err(ApiError::Forbidden);
    }

    Ok(user)
}

pub(crate) fn is_superadmin(user: &crate::models::users::User) -> bool {
    user.role == 0.0
}

pub(crate) fn is_chat_workspace_match(
    user_workspace_ids: &[ObjectId],
    conversation_workspace_id: &ObjectId,
) -> bool {
    // Global users (sin workspaces explícitos) pueden operar en cualquier workspace.
    user_workspace_ids.is_empty()
        || user_workspace_ids
            .iter()
            .any(|id| id == conversation_workspace_id)
}

pub(crate) async fn require_workspace_actor_for_conversation(
    state: &Arc<AppState>,
    actor: &crate::models::users::User,
    business_phone: &str,
) -> Result<WaSettings, ApiError> {
    let settings = state
        .db
        .find_wa_settings_by_phone(business_phone)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::NOT_FOUND,
                "whatsapp_workspace_not_found",
                "No hay un workspace activo configurado para esta conversación",
            )
        })?;

    if is_superadmin(actor) {
        return Ok(settings);
    }

    let actor_workspaces = state
        .db
        .get_user_workspaces(&actor.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let Some(conversation_workspace_id) = settings.id.as_ref() else {
        return Err(ApiError::DatabaseError("workspace id no disponible".into()));
    };

    if !is_chat_workspace_match(&actor_workspaces, conversation_workspace_id) {
        return Err(ApiError::domain_simple(
            StatusCode::FORBIDDEN,
            "whatsapp_workspace_membership_required",
            "No tienes permiso sobre el workspace de esta conversación",
        ));
    }

    Ok(settings)
}

pub(crate) async fn ensure_transfer_target_allowed_for_workspace(
    state: &Arc<AppState>,
    target: &crate::models::users::User,
    workspace_id: Option<&ObjectId>,
) -> Result<(), ApiError> {
    if is_transfer_target_allowed_for_workspace(state, target, workspace_id).await? {
        return Ok(());
    }

    Err(ApiError::domain_simple(
        StatusCode::FORBIDDEN,
        "whatsapp_transfer_target_not_allowed",
        "El usuario destino no puede atender chats de WhatsApp",
    ))
}

pub(crate) async fn is_transfer_target_allowed_for_workspace(
    state: &Arc<AppState>,
    target: &crate::models::users::User,
    workspace_id: Option<&ObjectId>,
) -> Result<bool, ApiError> {
    // Regla base del target: debe estar visible, no ser bot y poder atender chats.
    if !target.visible || target.is_bot || !target.can_chat {
        return Ok(false);
    }

    // Superadmin sigue respetando bCanChat=true como gate de recepción.
    if is_superadmin(target) {
        return Ok(true);
    }

    let Some(workspace_id) = workspace_id else {
        return Ok(true);
    };

    let target_workspaces = state
        .db
        .get_user_workspaces(&target.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    if !is_chat_workspace_match(&target_workspaces, workspace_id) {
        return Ok(false);
    }

    Ok(true)
}

pub(crate) async fn is_transfer_target_allowed_for_actor_workspaces(
    state: &Arc<AppState>,
    target: &crate::models::users::User,
    actor_workspace_ids: &[ObjectId],
) -> Result<bool, ApiError> {
    if !target.visible || target.is_bot || !target.can_chat {
        return Ok(false);
    }

    if is_superadmin(target) || actor_workspace_ids.is_empty() {
        return Ok(true);
    }

    let target_workspaces = state
        .db
        .get_user_workspaces(&target.id)
        .await
        .map_err(ApiError::DatabaseError)?;

    if target_workspaces.is_empty() {
        return Ok(true);
    }

    Ok(target_workspaces.iter().any(|target_workspace_id| {
        actor_workspace_ids
            .iter()
            .any(|id| id == target_workspace_id)
    }))
}

/// Exige `nRole == 0` (SUPERADMIN). Devuelve `403` si no se cumple.
pub(crate) async fn require_superadmin(
    state: &Arc<AppState>,
    user_id: &str,
) -> Result<crate::models::users::User, ApiError> {
    let user = state
        .db
        .find_user_by_id(user_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::Forbidden)?;

    if user.role != 0.0 {
        return Err(ApiError::Forbidden);
    }

    Ok(user)
}
