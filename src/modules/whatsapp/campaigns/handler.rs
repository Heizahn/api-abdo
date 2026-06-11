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
        CampaignListQuery, CampaignListResponse, CampaignPreviewRequest, CampaignPreviewResponse,
        CampaignRecipientsQuery, CampaignRecipientsResponse, CampaignSummaryResponse,
        CreateCampaignRequest, UpdateCampaignRecipientExclusionsRequest,
        UpdateCampaignRecipientExclusionsResponse, UpdateCampaignRequest, UpdateCampaignResponse,
    },
    service::{
        confirm_campaign, create_campaign, get_campaign, get_campaign_recipients, list_campaigns,
        preview_recipients, send_campaign, start_campaign, update_campaign,
        update_campaign_recipient_exclusions,
    },
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
        (status = 200, description = "Create a draft WhatsApp campaign and optionally auto-confirm/start dry-run validation when auto_prepare=true", body = CampaignSummaryResponse),
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
    post,
    path = "/v1/admin/whatsapp-campaigns/{id}/confirm",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "Campaign ObjectId")),
    responses(
        (status = 200, description = "Confirm a WhatsApp campaign and lock it as queued without sending messages", body = CampaignSummaryResponse),
        (status = 400, description = "Invalid campaign id or no effective recipients"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
        (status = 404, description = "Campaign not found"),
        (status = 409, description = "Campaign status is not confirmable"),
    )
)]
pub async fn confirm_campaign_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<CampaignSummaryResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    confirm_campaign(&state, &id, &claims.id).await.map(Json)
}

#[utoipa::path(
    post,
    path = "/v1/admin/whatsapp-campaigns/{id}/start",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "Campaign ObjectId")),
    responses(
        (status = 200, description = "Start a queued WhatsApp campaign by moving it to running without processing recipients", body = CampaignSummaryResponse),
        (status = 400, description = "Invalid campaign id, missing required campaign data, or no effective recipients"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
        (status = 404, description = "Campaign not found"),
        (status = 409, description = "Campaign status is not startable"),
    )
)]
pub async fn start_campaign_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<CampaignSummaryResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    start_campaign(&state, &id, &claims.id).await.map(Json)
}

#[utoipa::path(
    post,
    path = "/v1/admin/whatsapp-campaigns/{id}/send",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "Campaign ObjectId")),
    responses(
        (status = 200, description = "Mark a dry-run-completed WhatsApp campaign as ready for real sending without processing recipients", body = CampaignSummaryResponse),
        (status = 400, description = "Invalid campaign id, missing WhatsApp account data, missing validated recipients, or invalid template media bindings"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
        (status = 404, description = "Campaign not found"),
        (status = 409, description = "Campaign status is not sendable"),
    )
)]
pub async fn send_campaign_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
) -> Result<Json<CampaignSummaryResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    send_campaign(&state, &id, &claims.id).await.map(Json)
}

#[utoipa::path(
    get,
    path = "/v1/admin/whatsapp-campaigns",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    params(
        ("page" = Option<u32>, Query, description = "Page number, starting at 1. Default 1"),
        ("limit" = Option<u32>, Query, description = "Page size. Default 20, max 100"),
        ("status" = Option<String>, Query, description = "Campaign status exact match"),
        ("search" = Option<String>, Query, description = "Case-insensitive search in name and template_name"),
        ("created_from" = Option<String>, Query, description = "ISO-8601 lower bound for created_at"),
        ("created_to" = Option<String>, Query, description = "ISO-8601 upper bound for created_at"),
    ),
    responses(
        (status = 200, description = "List WhatsApp campaigns ordered by creation date descending", body = CampaignListResponse),
        (status = 400, description = "Invalid date filter"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
    )
)]
pub async fn list_campaigns_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(query): Query<CampaignListQuery>,
) -> Result<Json<CampaignListResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    list_campaigns(&state, query).await.map(Json)
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
    patch,
    path = "/v1/admin/whatsapp-campaigns/{id}",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "Campaign ObjectId")),
    request_body = UpdateCampaignRequest,
    responses(
        (status = 200, description = "Edit a draft or previewed WhatsApp campaign", body = UpdateCampaignResponse),
        (status = 400, description = "Invalid campaign id, fields, filters, or template variable bindings"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
        (status = 404, description = "Campaign not found"),
        (status = 409, description = "Campaign status is not editable"),
    )
)]
pub async fn update_campaign_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(request): Json<UpdateCampaignRequest>,
) -> Result<Json<UpdateCampaignResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    update_campaign(&state, &id, &claims.id, request)
        .await
        .map(Json)
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
        ("status" = Option<String>, Query, description = "Recipient status exact match"),
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

#[utoipa::path(
    patch,
    path = "/v1/admin/whatsapp-campaigns/{id}/recipients/exclusions",
    tag = "WhatsApp — Campaigns",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "Campaign ObjectId")),
    request_body = UpdateCampaignRecipientExclusionsRequest,
    responses(
        (status = 200, description = "Toggle manual exclusion for technically sendable campaign recipient snapshot rows", body = UpdateCampaignRecipientExclusionsResponse),
        (status = 400, description = "Invalid campaign id, empty recipient_ids, or invalid recipient_ids"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "SUPERADMIN role required"),
        (status = 404, description = "Campaign not found"),
        (status = 409, description = "Campaign status is not editable"),
    )
)]
pub async fn update_campaign_recipient_exclusions_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Path(id): Path<String>,
    Json(request): Json<UpdateCampaignRecipientExclusionsRequest>,
) -> Result<Json<UpdateCampaignRecipientExclusionsResponse>, ApiError> {
    require_superadmin(&state, &claims.id).await?;
    update_campaign_recipient_exclusions(&state, &id, request)
        .await
        .map(Json)
}
