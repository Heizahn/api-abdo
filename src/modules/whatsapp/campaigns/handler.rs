use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};

use crate::{
    auth::user_jwt::UserProfileClaims, error::ApiError,
    modules::whatsapp::shared::require_superadmin, state::AppState,
};

use super::{
    dto::{
        CampaignPreviewRequest, CampaignPreviewResponse, CampaignRecipientsQuery,
        CampaignRecipientsResponse, CampaignSummaryResponse, CreateCampaignRequest,
    },
    service::{create_campaign, get_campaign, get_campaign_recipients, preview_recipients},
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

#[utoipa::path(
    post,
    path = "/v1/admin/whatsapp-campaigns",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    request_body = CreateCampaignRequest,
    responses(
        (status = 200, description = "Create a draft WhatsApp campaign and freeze its recipient snapshot", body = CampaignSummaryResponse),
        (status = 400, description = "Invalid request or missing campaign filters"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
    )
)]
pub async fn create_campaign_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Json(request): Json<CreateCampaignRequest>,
) -> Result<Json<CampaignSummaryResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    create_campaign(&state, &claims.id, request).await.map(Json)
}

#[utoipa::path(
    get,
    path = "/v1/admin/whatsapp-campaigns/{id}",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "Campaign ObjectId")),
    responses(
        (status = 200, description = "Get a WhatsApp campaign summary", body = CampaignSummaryResponse),
        (status = 400, description = "Invalid campaign id"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
        (status = 404, description = "Campaign not found"),
    )
)]
pub async fn get_campaign_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<CampaignSummaryResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    get_campaign(&state, &id).await.map(Json)
}

#[utoipa::path(
    get,
    path = "/v1/admin/whatsapp-campaigns/{id}/recipients",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "Campaign ObjectId"),
        ("page" = Option<u32>, Query, description = "Page number, starting at 1"),
        ("per_page" = Option<u32>, Query, description = "Page size, max 500"),
    ),
    responses(
        (status = 200, description = "List frozen recipient snapshot rows for a campaign", body = CampaignRecipientsResponse),
        (status = 400, description = "Invalid campaign id"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
        (status = 404, description = "Campaign not found"),
    )
)]
pub async fn get_campaign_recipients_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Query(query): Query<CampaignRecipientsQuery>,
) -> Result<Json<CampaignRecipientsResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    get_campaign_recipients(&state, &id, query).await.map(Json)
}
