use std::sync::Arc;

use axum::{extract::State, Extension, Json};

use crate::{
    auth::user_jwt::UserProfileClaims, error::ApiError,
    modules::whatsapp::shared::require_superadmin, state::AppState,
};

use super::{
    dto::{CampaignPreviewRequest, CampaignPreviewResponse},
    service::preview_recipients,
};

#[utoipa::path(
    post,
    path = "/v1/admin/whatsapp-campaigns/preview",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    request_body = CampaignPreviewRequest,
    responses(
        (status = 200, description = "Preview WhatsApp campaign recipients without sending messages", body = CampaignPreviewResponse),
        (status = 400, description = "At least one filter is required unless all active clients are explicitly requested"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
    )
)]
pub async fn preview_campaign_recipients_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(request): Json<CampaignPreviewRequest>,
) -> Result<Json<CampaignPreviewResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    preview_recipients(&state, request).await.map(Json)
}
