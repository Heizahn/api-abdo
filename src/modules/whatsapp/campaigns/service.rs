use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;

use axum::http::StatusCode;
use futures::stream::StreamExt;
use futures::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId, Bson, DateTime, Document};
use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument, UpdateModifications};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, modules::whatsapp::shared::time::iso8601, state::AppState};

use super::{
    dto::{
        BalanceFilter, CampaignListItem, CampaignListQuery, CampaignListResponse,
        CampaignPreviewRecipient, CampaignPreviewRequest, CampaignPreviewResponse,
        CampaignPreviewTotals, CampaignRecipientItem, CampaignRecipientsQuery,
        CampaignRecipientsResponse, CampaignSummary, CampaignSummaryResponse, ClientStateFilter,
        CreateCampaignRequest, DerivedClientState, PhoneStatus,
        UpdateCampaignRecipientExclusionsData, UpdateCampaignRecipientExclusionsRequest,
        UpdateCampaignRecipientExclusionsResponse,
    },
    phone::normalize_phone_to_whatsapp,
};

const DEFAULT_PER_PAGE: u32 = 100;
const MAX_PER_PAGE: u32 = 500;
const DEFAULT_CAMPAIGN_LIST_LIMIT: u32 = 20;
const MAX_CAMPAIGN_LIST_LIMIT: u32 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WaCampaignDoc {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    id: Option<ObjectId>,
    name: String,
    template_name: String,
    template_language: String,
    #[serde(default)]
    template_components: Option<Vec<serde_json::Value>>,
    filters: CampaignPreviewRequest,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confirming_from: Option<String>,
    total_recipients: u64,
    total_can_send: u64,
    total_invalid_phone: u64,
    total_duplicated_phone: u64,
    total_excluded: u64,
    created_by: String,
    #[serde(default)]
    confirmed_by: Option<String>,
    #[serde(default)]
    confirmed_at: Option<DateTime>,
    created_at: DateTime,
    updated_at: DateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WaCampaignRecipientDoc {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    id: Option<ObjectId>,
    campaign_id: ObjectId,
    client_id: String,
    client_name: String,
    provider_id: Option<String>,
    provider_name: Option<String>,
    sector_id: Option<String>,
    sector_name: Option<String>,
    customer_status_raw: String,
    customer_status_derived: DerivedClientState,
    balance: f64,
    phone_original: String,
    phone_normalized: Option<String>,
    phone_status: PhoneStatus,
    can_send: bool,
    reason: Option<String>,
    excluded: bool,
    status: String,
    created_at: DateTime,
    updated_at: DateTime,
}

struct CandidateClient {
    id: String,
    name: String,
    phone: String,
    provider_id: Option<String>,
    provider_name: Option<String>,
    provider_tag: Option<String>,
    sector_id: Option<String>,
    sector_name: Option<String>,
    state: String,
    balance: f64,
}

pub async fn preview_recipients(
    state: &AppState,
    request: CampaignPreviewRequest,
) -> Result<CampaignPreviewResponse, ApiError> {
    if !has_allowed_filter(&request) {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "campaign_preview_requires_filter",
            "Provide at least one filter, or explicitly request all active clients.",
        ));
    }

    let page = request.page.unwrap_or(1).max(1);
    let per_page = request
        .per_page
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);
    let (totals, recipients) = build_recipients_snapshot(state, &request).await?;
    let start = pagination_skip_usize(page, per_page);
    let recipients = recipients
        .into_iter()
        .skip(start)
        .take(per_page as usize)
        .collect();

    Ok(CampaignPreviewResponse {
        ok: true,
        totals,
        recipients,
        page,
        per_page,
    })
}

pub async fn create_campaign(
    state: &AppState,
    created_by: &str,
    request: CreateCampaignRequest,
) -> Result<CampaignSummaryResponse, ApiError> {
    if request.name.trim().is_empty() {
        return Err(ApiError::BadRequest("campaign_name_required".to_string()));
    }
    if request.template_name.trim().is_empty() {
        return Err(ApiError::BadRequest("template_name_required".to_string()));
    }
    if request.template_language.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "template_language_required".to_string(),
        ));
    }

    let (totals, recipients) = build_recipients_snapshot(state, &request.filters).await?;
    let now = DateTime::now();
    let campaign_id = ObjectId::new();
    let campaign = WaCampaignDoc {
        id: Some(campaign_id.clone()),
        name: request.name.trim().to_string(),
        template_name: request.template_name.trim().to_string(),
        template_language: request.template_language.trim().to_string(),
        template_components: request.template_components,
        filters: request.filters,
        status: "draft".to_string(),
        confirming_from: None,
        total_recipients: totals.matched as u64,
        total_can_send: totals.can_send as u64,
        total_invalid_phone: totals.invalid_phone as u64,
        total_duplicated_phone: totals.duplicated_phone as u64,
        total_excluded: 0,
        created_by: created_by.to_string(),
        confirmed_by: None,
        confirmed_at: None,
        created_at: now,
        updated_at: now,
    };

    let recipient_docs = recipients
        .into_iter()
        .map(|recipient| preview_to_snapshot_recipient(campaign_id.clone(), recipient, now))
        .collect::<Vec<_>>();

    if !recipient_docs.is_empty() {
        let recipients_result = state
            .db
            .db
            .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients")
            .insert_many(recipient_docs)
            .await
            .map_err(|e| {
                ApiError::DatabaseError(format!("campaign snapshot recipients insert failed: {e}"))
            });
        if let Err(err) = recipients_result {
            if let Err(cleanup_err) = cleanup_campaign_snapshot(state, campaign_id.clone()).await {
                return Err(cleanup_err);
            }
            return Err(err);
        }
    }

    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");
    let campaign_insert_result = campaigns
        .insert_one(&campaign)
        .await
        .map_err(|e| ApiError::DatabaseError(format!("campaign insert failed: {e}")));
    if let Err(err) = campaign_insert_result {
        if let Err(cleanup_err) = cleanup_campaign_snapshot(state, campaign_id).await {
            return Err(cleanup_err);
        }
        return Err(err);
    }

    Ok(CampaignSummaryResponse {
        ok: true,
        data: campaign_to_summary(campaign),
    })
}

pub async fn get_campaign(
    state: &AppState,
    campaign_id: &str,
) -> Result<CampaignSummaryResponse, ApiError> {
    let campaign_id = parse_campaign_id(campaign_id)?;
    let campaign = state
        .db
        .db
        .collection::<WaCampaignDoc>("WaCampaigns")
        .find_one(doc! { "_id": campaign_id })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .ok_or(ApiError::NotFound)?;

    Ok(CampaignSummaryResponse {
        ok: true,
        data: campaign_to_summary(campaign),
    })
}

pub async fn confirm_campaign(
    state: &AppState,
    campaign_id: &str,
    confirmed_by: &str,
) -> Result<CampaignSummaryResponse, ApiError> {
    let campaign_id = parse_campaign_id(campaign_id)?;
    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");
    let recipients = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");

    let campaign = claim_campaign_for_confirmation(&campaigns, &campaign_id).await?;

    let effective_recipients = recipients
        .count_documents(effective_recipient_filter(campaign_id.clone()))
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
    if effective_recipients == 0 {
        restore_campaign_after_failed_confirmation(
            &campaigns,
            &campaign_id,
            campaign.status.as_str(),
        )
        .await?;
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "campaign_has_no_effective_recipients",
            "Campaign must have at least one non-excluded pending recipient that can be sent.",
        ));
    }

    let now = DateTime::now();
    let result = campaigns
        .update_one(
            doc! { "_id": campaign_id, "status": "confirming", "confirming_from": &campaign.status },
            doc! {
                "$set": {
                    "status": "queued",
                    "confirmed_by": confirmed_by,
                    "confirmed_at": now,
                    "updated_at": now,
                },
                "$unset": { "confirming_from": "" }
            },
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    if result.matched_count == 0 {
        let campaign = campaigns
            .find_one(doc! { "_id": campaign_id })
            .await
            .map_err(|e| ApiError::DatabaseError(e.to_string()))?
            .ok_or(ApiError::NotFound)?;

        if campaign.status == "confirming" || campaign.status == "queued" {
            return Err(ApiError::domain_simple(
                StatusCode::CONFLICT,
                "campaign_confirmation_in_progress",
                "Campaign confirmation is already in progress or completed.",
            ));
        }

        if !is_confirmable_campaign_status(&campaign.status) {
            return Err(ApiError::domain_simple(
                StatusCode::CONFLICT,
                "campaign_not_confirmable",
                "Only draft or previewed campaigns can be confirmed.",
            ));
        }
    }

    let campaign = campaigns
        .find_one(doc! { "_id": campaign_id })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .ok_or(ApiError::NotFound)?;

    Ok(CampaignSummaryResponse {
        ok: true,
        data: campaign_to_summary(campaign),
    })
}

pub async fn list_campaigns(
    state: &AppState,
    query: CampaignListQuery,
) -> Result<CampaignListResponse, ApiError> {
    let page = query.page.unwrap_or(1).max(1);
    let limit = query
        .limit
        .unwrap_or(DEFAULT_CAMPAIGN_LIST_LIMIT)
        .clamp(1, MAX_CAMPAIGN_LIST_LIMIT);
    let skip = pagination_skip(page, limit);
    let filter = build_campaign_list_filter(&query)?;
    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");
    let total = campaigns
        .count_documents(filter.clone())
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
    let campaign_items = campaigns
        .find(filter)
        .sort(doc! { "created_at": -1, "_id": -1 })
        .skip(skip)
        .limit(limit as i64)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .into_iter()
        .map(campaign_to_list_item)
        .collect();

    Ok(CampaignListResponse {
        ok: true,
        page,
        limit,
        total,
        total_pages: total_pages(total, limit),
        campaigns: campaign_items,
    })
}

pub async fn get_campaign_recipients(
    state: &AppState,
    campaign_id: &str,
    query: CampaignRecipientsQuery,
) -> Result<CampaignRecipientsResponse, ApiError> {
    let campaign_id = parse_campaign_id(campaign_id)?;
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query
        .per_page
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);
    let skip = pagination_skip(page, per_page);
    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");
    let campaign_exists = campaigns
        .find_one(doc! { "_id": campaign_id.clone() })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .is_some();
    if !campaign_exists {
        return Err(ApiError::NotFound);
    }
    let collection = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");
    let filter = doc! { "campaign_id": campaign_id };
    let total = collection
        .count_documents(filter.clone())
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
    let recipients = collection
        .find(filter)
        .sort(doc! { "client_name": 1, "_id": 1 })
        .skip(skip)
        .limit(per_page as i64)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .into_iter()
        .map(recipient_to_item)
        .collect();

    Ok(CampaignRecipientsResponse {
        ok: true,
        data: recipients,
        page,
        per_page,
        total,
    })
}

pub async fn update_campaign_recipient_exclusions(
    state: &AppState,
    campaign_id: &str,
    request: UpdateCampaignRecipientExclusionsRequest,
) -> Result<UpdateCampaignRecipientExclusionsResponse, ApiError> {
    let campaign_id = parse_campaign_id(campaign_id)?;
    let requested = request.recipient_ids.len() as u64;
    if request.recipient_ids.is_empty() {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "recipient_ids_required",
            "Provide at least one campaign recipient id.",
        ));
    }

    let recipient_ids = parse_recipient_object_ids(&request.recipient_ids)?;

    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");
    let campaign = claim_campaign_for_editing(&campaigns, &campaign_id).await?;
    let original_status = campaign.status.clone();
    let recipients = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");

    let updated = if recipient_ids.is_empty() {
        0
    } else {
        match recipients
            .update_many(
                doc! {
                    "_id": { "$in": recipient_ids },
                    "campaign_id": campaign_id,
                    "can_send": true,
                    "excluded": { "$ne": request.excluded },
                    "status": "pending",
                },
                doc! { "$set": { "excluded": request.excluded, "updated_at": DateTime::now() } },
            )
            .await
        {
            Ok(result) => result.modified_count,
            Err(e) => {
                let _ = restore_campaign_after_failed_exclusion(
                    &campaigns,
                    &campaign_id,
                    &original_status,
                    None,
                )
                .await;
                return Err(ApiError::DatabaseError(e.to_string()));
            }
        }
    };

    let total_excluded = match count_effectively_excluded_recipients(&recipients, campaign_id).await
    {
        Ok(total) => total,
        Err(err) => {
            let _ = restore_campaign_after_failed_exclusion(
                &campaigns,
                &campaign_id,
                &original_status,
                None,
            )
            .await;
            return Err(err);
        }
    };
    let total_effective_can_send =
        match count_effective_can_send_recipients(&recipients, campaign_id).await {
            Ok(total) => total,
            Err(err) => {
                let _ = restore_campaign_after_failed_exclusion(
                    &campaigns,
                    &campaign_id,
                    &original_status,
                    None,
                )
                .await;
                return Err(err);
            }
        };
    if let Err(e) = restore_campaign_after_failed_exclusion(
        &campaigns,
        &campaign_id,
        &original_status,
        Some(total_excluded),
    )
    .await
    {
        return Err(e);
    }

    Ok(UpdateCampaignRecipientExclusionsResponse {
        ok: true,
        data: UpdateCampaignRecipientExclusionsData {
            campaign_id: campaign_id.to_hex(),
            requested,
            updated,
            total_excluded,
            total_can_send: campaign.total_can_send,
            total_effective_can_send,
        },
    })
}

async fn count_effectively_excluded_recipients(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign_id: ObjectId,
) -> Result<u64, ApiError> {
    recipients
        .count_documents(doc! {
            "campaign_id": campaign_id,
            "can_send": true,
            "excluded": true,
            "status": "pending",
        })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))
}

async fn count_effective_can_send_recipients(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign_id: ObjectId,
) -> Result<u64, ApiError> {
    recipients
        .count_documents(effective_recipient_filter(campaign_id))
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))
}

fn effective_recipient_filter(campaign_id: ObjectId) -> Document {
    doc! {
        "campaign_id": campaign_id,
        "can_send": true,
        "excluded": false,
        "status": "pending",
    }
}

fn parse_recipient_object_ids(ids: &[String]) -> Result<Vec<ObjectId>, ApiError> {
    let mut parsed = Vec::with_capacity(ids.len());

    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            return Err(ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "invalid_recipient_ids",
                "recipient_ids",
                "Provide only non-empty valid ObjectId recipient ids.",
            ));
        }

        let object_id = ObjectId::parse_str(id).map_err(|_| {
            ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "invalid_recipient_ids",
                "recipient_ids",
                "Provide only non-empty valid ObjectId recipient ids.",
            )
        })?;
        parsed.push(object_id);
    }

    Ok(parsed)
}

fn is_editable_campaign_status(status: &str) -> bool {
    matches!(status, "draft" | "previewed")
}

fn is_confirmable_campaign_status(status: &str) -> bool {
    matches!(status, "draft" | "previewed")
}

fn confirmable_campaign_statuses() -> Vec<&'static str> {
    vec!["draft", "previewed"]
}

async fn claim_campaign_for_confirmation(
    campaigns: &mongodb::Collection<WaCampaignDoc>,
    campaign_id: &ObjectId,
) -> Result<WaCampaignDoc, ApiError> {
    let opts = FindOneAndUpdateOptions::builder()
        .return_document(ReturnDocument::Before)
        .build();
    let filter =
        doc! { "_id": campaign_id.clone(), "status": { "$in": confirmable_campaign_statuses() } };
    let pipeline = vec![doc! {
        "$set": {
            "status": "confirming",
            "confirming_from": "$status",
            "updated_at": DateTime::now(),
        }
    }];

    let campaign = campaigns
        .find_one_and_update(filter, UpdateModifications::Pipeline(pipeline))
        .with_options(opts)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    if let Some(campaign) = campaign {
        return Ok(campaign);
    }

    let current = campaigns
        .find_one(doc! { "_id": campaign_id.clone() })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    match current {
        None => Err(ApiError::NotFound),
        Some(campaign) if campaign.status == "confirming" || campaign.status == "queued" => {
            Err(ApiError::domain_simple(
                StatusCode::CONFLICT,
                "campaign_confirmation_in_progress",
                "Campaign confirmation is already in progress or completed.",
            ))
        }
        Some(_) => Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "campaign_not_confirmable",
            "Only draft or previewed campaigns can be confirmed.",
        )),
    }
}

async fn claim_campaign_for_editing(
    campaigns: &mongodb::Collection<WaCampaignDoc>,
    campaign_id: &ObjectId,
) -> Result<WaCampaignDoc, ApiError> {
    let opts = FindOneAndUpdateOptions::builder()
        .return_document(ReturnDocument::Before)
        .build();
    let filter =
        doc! { "_id": campaign_id.clone(), "status": { "$in": confirmable_campaign_statuses() } };
    let pipeline = vec![doc! {
        "$set": {
            "status": "editing",
            "updated_at": DateTime::now(),
        }
    }];

    let campaign = campaigns
        .find_one_and_update(filter, UpdateModifications::Pipeline(pipeline))
        .with_options(opts)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    if let Some(campaign) = campaign {
        return Ok(campaign);
    }

    let current = campaigns
        .find_one(doc! { "_id": campaign_id.clone() })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    match current {
        None => Err(ApiError::NotFound),
        Some(_) => Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "campaign_not_editable",
            "Only draft or previewed campaigns can update recipient exclusions.",
        )),
    }
}

async fn restore_campaign_after_failed_confirmation(
    campaigns: &mongodb::Collection<WaCampaignDoc>,
    campaign_id: &ObjectId,
    original_status: &str,
) -> Result<(), ApiError> {
    let restore_status = if is_editable_campaign_status(original_status) {
        original_status
    } else {
        "draft"
    };

    campaigns
        .update_one(
            doc! { "_id": campaign_id.clone(), "status": "confirming" },
            doc! {
                "$set": { "status": restore_status, "updated_at": DateTime::now() },
                "$unset": { "confirming_from": "" }
            },
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    Ok(())
}

async fn restore_campaign_after_failed_exclusion(
    campaigns: &mongodb::Collection<WaCampaignDoc>,
    campaign_id: &ObjectId,
    original_status: &str,
    total_excluded: Option<u64>,
) -> Result<(), ApiError> {
    let restore_status = if is_editable_campaign_status(original_status) {
        original_status
    } else {
        "draft"
    };

    let mut set = doc! {
        "status": restore_status,
        "updated_at": DateTime::now(),
    };
    if let Some(total_excluded) = total_excluded {
        set.insert("total_excluded", Bson::Int64(total_excluded as i64));
    }

    let result = campaigns
        .update_one(
            doc! { "_id": campaign_id.clone(), "status": "editing" },
            doc! { "$set": set },
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    if result.matched_count == 0 {
        let current = campaigns
            .find_one(doc! { "_id": campaign_id.clone() })
            .await
            .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

        return match current {
            None => Err(ApiError::NotFound),
            Some(_) => Err(ApiError::domain_simple(
                StatusCode::CONFLICT,
                "campaign_not_editable",
                "Only draft or previewed campaigns can update recipient exclusions.",
            )),
        };
    }

    Ok(())
}

async fn cleanup_campaign_snapshot(
    state: &AppState,
    campaign_id: ObjectId,
) -> Result<(), ApiError> {
    state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients")
        .delete_many(doc! { "campaign_id": campaign_id })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
    state
        .db
        .db
        .collection::<WaCampaignDoc>("WaCampaigns")
        .delete_many(doc! { "_id": campaign_id.clone() })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
    Ok(())
}

fn pagination_skip(page: u32, per_page: u32) -> u64 {
    u64::from(page.saturating_sub(1)).saturating_mul(u64::from(per_page))
}

fn pagination_skip_usize(page: u32, per_page: u32) -> usize {
    usize::try_from(pagination_skip(page, per_page)).unwrap_or(usize::MAX)
}

fn total_pages(total: u64, limit: u32) -> u64 {
    if total == 0 {
        0
    } else {
        total.div_ceil(u64::from(limit.max(1)))
    }
}

fn total_effective_can_send(total_can_send: u64, total_excluded: u64) -> u64 {
    total_can_send.saturating_sub(total_excluded)
}

fn build_campaign_list_filter(query: &CampaignListQuery) -> Result<Document, ApiError> {
    let mut filter = Document::new();

    if let Some(status) = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        filter.insert("status", status);
    }

    if let Some(search) = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let escaped = regex_escape(search);
        filter.insert(
            "$or",
            vec![
                doc! { "name": { "$regex": &escaped, "$options": "i" } },
                doc! { "template_name": { "$regex": &escaped, "$options": "i" } },
            ],
        );
    }

    let created_from = match query
        .created_from
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(value) => Some(parse_campaign_list_iso_date(value, "created_from")?),
        None => None,
    };
    let created_to = match query
        .created_to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(value) => Some(parse_campaign_list_iso_date(value, "created_to")?),
        None => None,
    };
    if let (Some(from), Some(to)) = (created_from, created_to) {
        if from > to {
            return Err(ApiError::ValidationError {
                code: "invalid_date_range".to_string(),
                field: "created_from".to_string(),
                message: "'created_from' must be before or equal to 'created_to'".to_string(),
            });
        }
    }
    let mut created_at = Document::new();
    if let Some(value) = created_from {
        created_at.insert("$gte", value);
    }
    if let Some(value) = created_to {
        created_at.insert("$lte", value);
    }
    if !created_at.is_empty() {
        filter.insert("created_at", created_at);
    }

    Ok(filter)
}

fn parse_campaign_list_iso_date(value: &str, field: &str) -> Result<DateTime, ApiError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| DateTime::from_millis(dt.timestamp_millis()))
        .map_err(|_| ApiError::ValidationError {
            code: "invalid_date".to_string(),
            field: field.to_string(),
            message: format!("'{}' must be ISO-8601", field),
        })
}

fn regex_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() * 2);
    for ch in value.chars() {
        if matches!(
            ch,
            '\\' | '^' | '$' | '.' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

async fn build_recipients_snapshot(
    state: &AppState,
    request: &CampaignPreviewRequest,
) -> Result<(CampaignPreviewTotals, Vec<CampaignPreviewRecipient>), ApiError> {
    if !has_allowed_filter(request) {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "campaign_preview_requires_filter",
            "Provide at least one filter, or explicitly request all active clients.",
        ));
    }

    let filter = build_client_filter(request)?;
    let projection = doc! {
        "_id": 1,
        "sName": 1,
        "sPhone": 1,
        "idOwner": 1,
        "idSector": 1,
        "sState": 1,
        "nBalance": 1,
    };

    let mut cursor = state
        .db
        .db
        .collection::<Document>("Clients")
        .find(filter)
        .projection(projection)
        .sort(doc! { "sName": 1, "_id": 1 })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let mut candidates = Vec::new();
    let mut provider_ids = HashSet::new();
    let mut sector_ids = HashSet::new();

    while let Some(result) = cursor.next().await {
        let doc = result.map_err(|e| ApiError::DatabaseError(e.to_string()))?;
        let provider_id = get_string_or_object_id(&doc, "idOwner");
        let sector_id = get_string_or_object_id(&doc, "idSector");

        if let Some(id) = provider_id.as_ref() {
            provider_ids.insert(id.clone());
        }
        if let Some(id) = sector_id.as_ref() {
            sector_ids.insert(id.clone());
        }

        candidates.push(CandidateClient {
            id: doc
                .get_object_id("_id")
                .map(|id| id.to_hex())
                .unwrap_or_default(),
            name: doc.get_str("sName").unwrap_or_default().to_string(),
            phone: doc.get_str("sPhone").unwrap_or_default().to_string(),
            provider_id,
            provider_name: None,
            provider_tag: None,
            sector_id,
            sector_name: None,
            state: doc.get_str("sState").unwrap_or_default().to_string(),
            balance: get_bson_amount(&doc, "nBalance"),
        });
    }

    let providers = load_providers(state, provider_ids).await?;
    let sectors = load_sectors(state, sector_ids).await?;

    Ok(build_preview_recipients(candidates, &providers, &sectors))
}

fn parse_campaign_id(id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(id).map_err(|_| ApiError::BadRequest("invalid_campaign_id".to_string()))
}

fn preview_to_snapshot_recipient(
    campaign_id: ObjectId,
    recipient: CampaignPreviewRecipient,
    now: DateTime,
) -> WaCampaignRecipientDoc {
    let status = snapshot_status(&recipient.phone_status, recipient.can_send, false);
    WaCampaignRecipientDoc {
        id: None,
        campaign_id,
        client_id: recipient.client_id,
        client_name: recipient.name,
        provider_id: recipient.provider_id,
        provider_name: recipient.provider_name,
        sector_id: recipient.sector_id,
        sector_name: recipient.sector_name,
        customer_status_raw: recipient.client_state_raw,
        customer_status_derived: recipient.client_state_derived,
        balance: recipient.balance,
        phone_original: recipient.phone_original,
        phone_normalized: recipient.phone_normalized,
        phone_status: recipient.phone_status,
        can_send: recipient.can_send,
        reason: recipient.reason,
        excluded: false,
        status,
        created_at: now,
        updated_at: now,
    }
}

fn snapshot_status(phone_status: &PhoneStatus, can_send: bool, _excluded: bool) -> String {
    if matches!(phone_status, PhoneStatus::Invalid) {
        "invalid_phone".to_string()
    } else if matches!(phone_status, PhoneStatus::Duplicated) {
        "duplicated_phone".to_string()
    } else if can_send {
        "pending".to_string()
    } else {
        "invalid_phone".to_string()
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
struct RecipientExclusionCounters {
    total_excluded: u64,
    total_can_send: u64,
    total_effective_can_send: u64,
}

#[cfg(test)]
fn calculate_recipient_exclusion_counters<'a>(
    rows: impl IntoIterator<Item = (bool, bool, &'a str)>,
) -> RecipientExclusionCounters {
    let mut counters = RecipientExclusionCounters {
        total_excluded: 0,
        total_can_send: 0,
        total_effective_can_send: 0,
    };

    for (can_send, excluded, status) in rows {
        if can_send {
            counters.total_can_send += 1;
        }
        if can_send && status == "pending" && excluded {
            counters.total_excluded += 1;
        }
        if can_send && status == "pending" && !excluded {
            counters.total_effective_can_send += 1;
        }
    }

    counters
}

fn campaign_to_summary(campaign: WaCampaignDoc) -> CampaignSummary {
    CampaignSummary {
        id: campaign.id.map(|id| id.to_hex()).unwrap_or_default(),
        name: campaign.name,
        template_name: campaign.template_name,
        template_language: campaign.template_language,
        template_components: campaign.template_components,
        filters: campaign.filters,
        status: campaign.status,
        total_recipients: campaign.total_recipients,
        total_can_send: campaign.total_can_send,
        total_invalid_phone: campaign.total_invalid_phone,
        total_duplicated_phone: campaign.total_duplicated_phone,
        total_excluded: campaign.total_excluded,
        total_effective_can_send: total_effective_can_send(
            campaign.total_can_send,
            campaign.total_excluded,
        ),
        created_by: campaign.created_by,
        confirmed_by: campaign.confirmed_by,
        confirmed_at: campaign.confirmed_at.map(iso8601),
        created_at: iso8601(campaign.created_at),
        updated_at: iso8601(campaign.updated_at),
    }
}

fn campaign_to_list_item(campaign: WaCampaignDoc) -> CampaignListItem {
    CampaignListItem {
        id: campaign.id.map(|id| id.to_hex()).unwrap_or_default(),
        name: campaign.name,
        template_name: campaign.template_name,
        template_language: campaign.template_language,
        status: campaign.status,
        total_recipients: campaign.total_recipients,
        total_can_send: campaign.total_can_send,
        total_invalid_phone: campaign.total_invalid_phone,
        total_duplicated_phone: campaign.total_duplicated_phone,
        total_excluded: campaign.total_excluded,
        total_effective_can_send: total_effective_can_send(
            campaign.total_can_send,
            campaign.total_excluded,
        ),
        created_by: campaign.created_by,
        created_at: iso8601(campaign.created_at),
        updated_at: iso8601(campaign.updated_at),
    }
}

fn recipient_to_item(recipient: WaCampaignRecipientDoc) -> CampaignRecipientItem {
    CampaignRecipientItem {
        id: recipient.id.map(|id| id.to_hex()).unwrap_or_default(),
        campaign_id: recipient.campaign_id.to_hex(),
        client_id: recipient.client_id,
        client_name: recipient.client_name,
        provider_id: recipient.provider_id,
        provider_name: recipient.provider_name,
        sector_id: recipient.sector_id,
        sector_name: recipient.sector_name,
        customer_status_raw: recipient.customer_status_raw,
        customer_status_derived: recipient.customer_status_derived,
        balance: recipient.balance,
        phone_original: recipient.phone_original,
        phone_normalized: recipient.phone_normalized,
        phone_status: recipient.phone_status,
        can_send: recipient.can_send,
        reason: recipient.reason,
        excluded: recipient.excluded,
        status: recipient.status,
        created_at: iso8601(recipient.created_at),
        updated_at: iso8601(recipient.updated_at),
    }
}

fn build_preview_recipients(
    candidates: Vec<CandidateClient>,
    providers: &HashMap<String, ProviderInfo>,
    sectors: &HashMap<String, String>,
) -> (CampaignPreviewTotals, Vec<CampaignPreviewRecipient>) {
    let mut totals = CampaignPreviewTotals {
        matched: candidates.len(),
        ..Default::default()
    };
    let mut seen_phones = HashSet::new();
    let mut recipients = Vec::with_capacity(candidates.len());

    for mut candidate in candidates {
        if let Some(provider) = candidate
            .provider_id
            .as_ref()
            .and_then(|id| providers.get(id))
        {
            candidate.provider_name = provider.name.clone();
            candidate.provider_tag = provider.tag.clone();
        }
        if let Some(sector_name) = candidate.sector_id.as_ref().and_then(|id| sectors.get(id)) {
            candidate.sector_name = Some(sector_name.clone());
        }

        let derived = derive_client_state(&candidate.state, candidate.balance);
        let normalized = normalize_phone_to_whatsapp(&candidate.phone).ok();
        let (phone_status, can_send, reason) = match normalized.as_ref() {
            None => {
                totals.invalid_phone += 1;
                (
                    PhoneStatus::Invalid,
                    false,
                    Some("invalid_phone".to_string()),
                )
            }
            Some(phone) if !seen_phones.insert(phone.clone()) => {
                totals.duplicated_phone += 1;
                (
                    PhoneStatus::Duplicated,
                    false,
                    Some("duplicated_phone".to_string()),
                )
            }
            Some(_) => {
                totals.can_send += 1;
                (PhoneStatus::Valid, true, None)
            }
        };

        recipients.push(CampaignPreviewRecipient {
            client_id: candidate.id,
            name: candidate.name,
            phone_original: candidate.phone,
            phone_normalized: normalized,
            phone_status,
            can_send,
            reason,
            provider_id: candidate.provider_id,
            provider_name: candidate.provider_name,
            provider_tag: candidate.provider_tag,
            sector_id: candidate.sector_id,
            sector_name: candidate.sector_name,
            client_state_raw: candidate.state,
            client_state_derived: derived,
            balance: candidate.balance,
        });
    }

    (totals, recipients)
}

fn has_allowed_filter(request: &CampaignPreviewRequest) -> bool {
    has_values(&request.provider_ids)
        || has_values(&request.sector_ids)
        || request.balance_filter.is_some()
        || matches!(request.client_state, Some(ClientStateFilter::Active))
        || request.include_all_active.unwrap_or(false)
        || matches!(
            request.client_state,
            Some(
                ClientStateFilter::Suspended
                    | ClientStateFilter::Moroso
                    | ClientStateFilter::Solvente
            )
        )
}

fn has_values(values: &Option<Vec<String>>) -> bool {
    values
        .as_ref()
        .is_some_and(|items| items.iter().any(|item| !item.trim().is_empty()))
}

fn build_client_filter(request: &CampaignPreviewRequest) -> Result<Document, ApiError> {
    let mut clauses: Vec<Document> = Vec::new();

    if let Some(provider_ids) = non_empty_ids(&request.provider_ids) {
        clauses.push(doc! { "idOwner": { "$in": string_or_object_id_bsons(provider_ids) } });
    }

    if let Some(sector_ids) = non_empty_ids(&request.sector_ids) {
        clauses.push(doc! { "idSector": { "$in": string_or_object_id_bsons(sector_ids) } });
    }

    if let Some(balance_filter) = &request.balance_filter {
        clauses.push(doc! { "nBalance": build_balance_filter(balance_filter)? });
    }

    match request.client_state.as_ref() {
        Some(ClientStateFilter::Active) => clauses.push(doc! { "sState": "Activo" }),
        Some(ClientStateFilter::Suspended) => clauses.push(doc! { "sState": "Suspendido" }),
        Some(ClientStateFilter::Moroso) => {
            clauses.push(doc! { "sState": "Activo", "nBalance": { "$lt": 0.0 } })
        }
        Some(ClientStateFilter::Solvente) => {
            clauses.push(doc! { "sState": "Activo", "nBalance": { "$gte": 0.0 } })
        }
        Some(ClientStateFilter::Any) | None => {
            if request.include_all_active.unwrap_or(false) {
                clauses.push(doc! { "sState": "Activo" });
            }
        }
    }

    Ok(if clauses.len() == 1 {
        clauses.remove(0)
    } else {
        doc! { "$and": clauses }
    })
}

fn build_balance_filter(filter: &BalanceFilter) -> Result<Document, ApiError> {
    let mut doc = Document::new();
    if let Some(value) = filter.lt {
        doc.insert("$lt", value);
    }
    if let Some(value) = filter.lte {
        doc.insert("$lte", value);
    }
    if let Some(value) = filter.gt {
        doc.insert("$gt", value);
    }
    if let Some(value) = filter.gte {
        doc.insert("$gte", value);
    }
    if let Some(value) = filter.eq {
        doc.insert("$eq", value);
    }
    if let Some(range) = &filter.between {
        doc.insert("$gte", range.min);
        doc.insert("$lte", range.max);
    }

    if doc.is_empty() {
        return Err(ApiError::BadRequest("invalid_balance_filter".to_string()));
    }

    Ok(doc)
}

fn non_empty_ids(ids: &Option<Vec<String>>) -> Option<Vec<String>> {
    ids.as_ref()
        .map(|values| {
            values
                .iter()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|values| !values.is_empty())
}

fn string_or_object_id_bsons(ids: Vec<String>) -> Vec<Bson> {
    ids.into_iter()
        .flat_map(|id| {
            let mut values = vec![Bson::String(id.clone())];
            if let Ok(object_id) = ObjectId::parse_str(&id) {
                values.push(Bson::ObjectId(object_id));
            }
            values
        })
        .collect()
}

#[derive(Default)]
struct ProviderInfo {
    name: Option<String>,
    tag: Option<String>,
}

async fn load_providers(
    state: &AppState,
    provider_ids: HashSet<String>,
) -> Result<std::collections::HashMap<String, ProviderInfo>, ApiError> {
    if provider_ids.is_empty() {
        return Ok(Default::default());
    }

    let id_values = string_or_object_id_bsons(provider_ids.into_iter().collect());
    let mut cursor = state
        .db
        .db
        .collection::<Document>("Users")
        .find(doc! { "_id": { "$in": id_values } })
        .projection(doc! { "_id": 1, "sName": 1, "nTag": 1 })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let mut providers = std::collections::HashMap::new();
    while let Some(result) = cursor.next().await {
        let doc = result.map_err(|e| ApiError::DatabaseError(e.to_string()))?;
        let id = get_string_or_object_id(&doc, "_id").unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let tag = get_numeric_tag(&doc, "nTag").map(|tag| format!("ABDO77-{tag}"));
        providers.insert(
            id,
            ProviderInfo {
                name: doc.get_str("sName").ok().map(|name| name.to_string()),
                tag,
            },
        );
    }

    Ok(providers)
}

async fn load_sectors(
    state: &AppState,
    sector_ids: HashSet<String>,
) -> Result<std::collections::HashMap<String, String>, ApiError> {
    if sector_ids.is_empty() {
        return Ok(Default::default());
    }

    let id_values = string_or_object_id_bsons(sector_ids.into_iter().collect());
    let mut cursor = state
        .db
        .db
        .collection::<Document>("Sectors")
        .find(doc! { "_id": { "$in": id_values } })
        .projection(doc! { "_id": 1, "sName": 1 })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let mut sectors = std::collections::HashMap::new();
    while let Some(result) = cursor.next().await {
        let doc = result.map_err(|e| ApiError::DatabaseError(e.to_string()))?;
        if let (Some(id), Ok(name)) = (get_string_or_object_id(&doc, "_id"), doc.get_str("sName")) {
            sectors.insert(id, name.to_string());
        }
    }

    Ok(sectors)
}

fn get_string_or_object_id(doc: &Document, field: &str) -> Option<String> {
    doc.get_str(field)
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| doc.get_object_id(field).ok().map(|id| id.to_hex()))
}

fn get_numeric_tag(doc: &Document, field: &str) -> Option<i64> {
    doc.get_i32(field)
        .ok()
        .map(i64::from)
        .or_else(|| doc.get_i64(field).ok())
}

fn derive_client_state(raw: &str, balance: f64) -> DerivedClientState {
    if raw != "Activo" {
        DerivedClientState::Suspended
    } else if balance < 0.0 {
        DerivedClientState::Moroso
    } else {
        DerivedClientState::Solvente
    }
}

fn get_bson_amount(doc: &Document, key: &str) -> f64 {
    doc.get_f64(key)
        .or_else(|_| doc.get_i32(key).map(|v| v as f64))
        .or_else(|_| doc.get_i64(key).map(|v| v as f64))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, name: &str, phone: &str) -> CandidateClient {
        CandidateClient {
            id: id.to_string(),
            name: name.to_string(),
            phone: phone.to_string(),
            provider_id: None,
            provider_name: None,
            provider_tag: None,
            sector_id: None,
            sector_name: None,
            state: "Activo".to_string(),
            balance: 0.0,
        }
    }

    fn base_campaign(status: &str) -> WaCampaignDoc {
        let now = DateTime::from_millis(1_800_000_000_000);
        WaCampaignDoc {
            id: Some(ObjectId::parse_str("64f000000000000000000001").unwrap()),
            name: "June Promo".to_string(),
            template_name: "promo_template".to_string(),
            template_language: "es".to_string(),
            template_components: None,
            filters: CampaignPreviewRequest {
                provider_ids: None,
                sector_ids: None,
                balance_filter: None,
                client_state: Some(ClientStateFilter::Active),
                include_all_active: None,
                page: None,
                per_page: None,
            },
            status: status.to_string(),
            confirming_from: None,
            total_recipients: 5,
            total_can_send: 4,
            total_invalid_phone: 1,
            total_duplicated_phone: 0,
            total_excluded: 1,
            created_by: "creator-1".to_string(),
            confirmed_by: Some("admin-1".to_string()),
            confirmed_at: Some(now),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn suspended_filter_matches_only_suspended_state() {
        let request = CampaignPreviewRequest {
            provider_ids: None,
            sector_ids: None,
            balance_filter: None,
            client_state: Some(ClientStateFilter::Suspended),
            include_all_active: None,
            page: None,
            per_page: None,
        };

        assert_eq!(
            build_client_filter(&request).unwrap(),
            doc! { "sState": "Suspendido" }
        );
    }

    #[test]
    fn duplicate_detection_is_global_and_keeps_first_valid_occurrence() {
        let (totals, recipients) = build_preview_recipients(
            vec![
                candidate("1", "First", "0412 123 4567"),
                candidate("2", "Second", "4121234567"),
                candidate("3", "Third", "0414 111 1111"),
            ],
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(totals.matched, 3);
        assert_eq!(totals.can_send, 2);
        assert_eq!(totals.duplicated_phone, 1);
        assert_eq!(totals.invalid_phone, 0);

        assert!(recipients[0].can_send);
        assert!(matches!(recipients[0].phone_status, PhoneStatus::Valid));
        assert!(!recipients[1].can_send);
        assert!(matches!(
            recipients[1].phone_status,
            PhoneStatus::Duplicated
        ));
        assert!(recipients[2].can_send);

        let page_two = recipients.into_iter().skip(1).take(1).collect::<Vec<_>>();
        assert!(matches!(page_two[0].phone_status, PhoneStatus::Duplicated));
        assert!(!page_two[0].can_send);
    }

    #[test]
    fn parse_recipient_object_ids_rejects_blank_ids() {
        let err = parse_recipient_object_ids(&[" ".to_string()]).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "invalid_recipient_ids"
                    && field.as_deref() == Some("recipient_ids")
        ));
    }

    #[test]
    fn parse_recipient_object_ids_rejects_malformed_ids() {
        let err = parse_recipient_object_ids(&["not-an-object-id".to_string()]).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "invalid_recipient_ids"
                    && field.as_deref() == Some("recipient_ids")
        ));
    }

    #[test]
    fn parse_recipient_object_ids_accepts_valid_ids() {
        let id = ObjectId::new();
        let parsed = parse_recipient_object_ids(&[id.to_hex()]).unwrap();

        assert_eq!(parsed, vec![id]);
    }

    #[test]
    fn snapshot_maps_invalid_phone_to_non_sendable_invalid_status() {
        let campaign_id = ObjectId::new();
        let now = DateTime::now();
        let recipient = CampaignPreviewRecipient {
            client_id: "client-1".to_string(),
            name: "Invalid Phone".to_string(),
            phone_original: "not-a-phone".to_string(),
            phone_normalized: None,
            phone_status: PhoneStatus::Invalid,
            can_send: false,
            reason: Some("invalid_phone".to_string()),
            provider_id: None,
            provider_name: None,
            provider_tag: None,
            sector_id: None,
            sector_name: None,
            client_state_raw: "Activo".to_string(),
            client_state_derived: DerivedClientState::Solvente,
            balance: 0.0,
        };

        let snapshot = preview_to_snapshot_recipient(campaign_id, recipient, now);

        assert!(!snapshot.can_send);
        assert!(!snapshot.excluded);
        assert_eq!(snapshot.status, "invalid_phone");
        assert!(matches!(snapshot.phone_status, PhoneStatus::Invalid));
    }

    #[test]
    fn snapshot_maps_duplicated_phone_to_non_sendable_duplicated_status() {
        let campaign_id = ObjectId::new();
        let now = DateTime::now();
        let recipient = CampaignPreviewRecipient {
            client_id: "client-2".to_string(),
            name: "Duplicated Phone".to_string(),
            phone_original: "0412 123 4567".to_string(),
            phone_normalized: Some("584121234567".to_string()),
            phone_status: PhoneStatus::Duplicated,
            can_send: false,
            reason: Some("duplicated_phone".to_string()),
            provider_id: None,
            provider_name: None,
            provider_tag: None,
            sector_id: None,
            sector_name: None,
            client_state_raw: "Activo".to_string(),
            client_state_derived: DerivedClientState::Solvente,
            balance: 0.0,
        };

        let snapshot = preview_to_snapshot_recipient(campaign_id, recipient, now);

        assert!(!snapshot.can_send);
        assert!(!snapshot.excluded);
        assert_eq!(snapshot.status, "duplicated_phone");
        assert!(matches!(snapshot.phone_status, PhoneStatus::Duplicated));
    }

    #[test]
    fn pagination_skip_uses_wide_math() {
        let skip = pagination_skip(u32::MAX, MAX_PER_PAGE);
        assert_eq!(skip, u64::from(u32::MAX - 1) * u64::from(MAX_PER_PAGE));
        assert!(skip > u64::from(u32::MAX));
    }

    #[test]
    fn total_pages_rounds_up_and_handles_empty_results() {
        assert_eq!(total_pages(0, 20), 0);
        assert_eq!(total_pages(1, 20), 1);
        assert_eq!(total_pages(20, 20), 1);
        assert_eq!(total_pages(21, 20), 2);
        assert_eq!(total_pages(100, 20), 5);
    }

    #[test]
    fn total_effective_can_send_uses_saturating_subtraction() {
        assert_eq!(total_effective_can_send(90, 10), 80);
        assert_eq!(total_effective_can_send(5, 10), 0);
    }

    #[test]
    fn effective_recipient_filter_requires_campaign_sendable_non_excluded_pending_rows() {
        let campaign_id = ObjectId::parse_str("64f000000000000000000001").unwrap();

        assert_eq!(
            effective_recipient_filter(campaign_id),
            doc! {
                "campaign_id": campaign_id,
                "can_send": true,
                "excluded": false,
                "status": "pending",
            }
        );
    }

    #[test]
    fn confirmable_campaign_status_allows_only_draft_and_previewed() {
        assert!(is_confirmable_campaign_status("draft"));
        assert!(is_confirmable_campaign_status("previewed"));
        assert!(!is_confirmable_campaign_status("confirming"));
        assert!(!is_confirmable_campaign_status("queued"));
        assert!(!is_confirmable_campaign_status("editing"));
        assert!(!is_confirmable_campaign_status("running"));
        assert!(!is_confirmable_campaign_status("completed"));
        assert!(!is_confirmable_campaign_status("cancelled"));
        assert_eq!(confirmable_campaign_statuses(), vec!["draft", "previewed"]);
    }

    #[test]
    fn campaign_summary_maps_effective_total_and_confirmed_fields() {
        let summary = campaign_to_summary(base_campaign("queued"));

        assert_eq!(summary.status, "queued");
        assert_eq!(summary.total_effective_can_send, 3);
        assert_eq!(summary.confirmed_by.as_deref(), Some("admin-1"));
        assert!(summary.confirmed_at.is_some());
    }

    #[test]
    fn campaign_list_filter_matches_status_and_escaped_search() {
        let query = CampaignListQuery {
            page: None,
            limit: None,
            status: Some("draft".to_string()),
            search: Some("promo.2026".to_string()),
            created_from: None,
            created_to: None,
        };

        assert_eq!(
            build_campaign_list_filter(&query).unwrap(),
            doc! {
                "status": "draft",
                "$or": [
                    { "name": { "$regex": "promo\\.2026", "$options": "i" } },
                    { "template_name": { "$regex": "promo\\.2026", "$options": "i" } },
                ],
            }
        );
    }

    #[test]
    fn campaign_list_filter_rejects_invalid_created_date() {
        let query = CampaignListQuery {
            page: None,
            limit: None,
            status: None,
            search: None,
            created_from: Some("not-a-date".to_string()),
            created_to: None,
        };

        let err = build_campaign_list_filter(&query).unwrap_err();

        assert!(matches!(
            err,
            ApiError::ValidationError { ref code, ref field, .. }
                if code == "invalid_date" && field == "created_from"
        ));
    }

    #[test]
    fn campaign_list_filter_rejects_inverted_created_date_range() {
        let query = CampaignListQuery {
            page: None,
            limit: None,
            status: None,
            search: None,
            created_from: Some("2026-06-10T00:00:00Z".to_string()),
            created_to: Some("2026-06-09T00:00:00Z".to_string()),
        };

        let err = build_campaign_list_filter(&query).unwrap_err();

        assert!(matches!(
            err,
            ApiError::ValidationError { ref code, ref field, .. }
                if code == "invalid_date_range" && field == "created_from"
        ));
    }

    #[test]
    fn exclusion_counters_exclude_sendable_pending_recipients_without_changing_total_can_send() {
        let counters = calculate_recipient_exclusion_counters(vec![
            (true, true, "pending"),
            (true, false, "pending"),
            (false, true, "invalid_phone"),
        ]);

        assert_eq!(counters.total_excluded, 1);
        assert_eq!(counters.total_can_send, 2);
        assert_eq!(counters.total_effective_can_send, 1);
    }

    #[test]
    fn exclusion_counters_reinclude_sendable_pending_recipients_without_changing_total_can_send() {
        let before = calculate_recipient_exclusion_counters(vec![
            (true, true, "pending"),
            (true, false, "pending"),
        ]);
        let after = calculate_recipient_exclusion_counters(vec![
            (true, false, "pending"),
            (true, false, "pending"),
        ]);

        assert_eq!(before.total_excluded, 1);
        assert_eq!(after.total_excluded, 0);
        assert_eq!(before.total_can_send, after.total_can_send);
        assert_eq!(after.total_effective_can_send, 2);
    }

    #[test]
    fn exclusion_counters_are_idempotent_for_repeated_same_state() {
        let exclude_once = calculate_recipient_exclusion_counters(vec![(true, true, "pending")]);
        let exclude_twice = calculate_recipient_exclusion_counters(vec![(true, true, "pending")]);
        let include_once = calculate_recipient_exclusion_counters(vec![(true, false, "pending")]);
        let include_twice = calculate_recipient_exclusion_counters(vec![(true, false, "pending")]);

        assert_eq!(exclude_once, exclude_twice);
        assert_eq!(include_once, include_twice);
    }

    #[test]
    fn exclusion_counters_ignore_invalid_and_duplicated_phone_rows() {
        let counters = calculate_recipient_exclusion_counters(vec![
            (false, true, "invalid_phone"),
            (false, true, "duplicated_phone"),
            (true, true, "pending"),
        ]);

        assert_eq!(counters.total_excluded, 1);
        assert_eq!(counters.total_can_send, 1);
        assert_eq!(counters.total_effective_can_send, 0);
    }

    #[test]
    fn editable_campaign_status_allows_only_draft_and_previewed() {
        assert!(is_editable_campaign_status("draft"));
        assert!(is_editable_campaign_status("previewed"));
        assert!(!is_editable_campaign_status("editing"));
        assert!(!is_editable_campaign_status("confirming"));
        assert!(!is_editable_campaign_status("queued"));
        assert!(!is_editable_campaign_status("running"));
        assert!(!is_editable_campaign_status("completed"));
    }

    #[test]
    fn exclusion_guard_rejects_queued_campaigns() {
        assert!(!is_editable_campaign_status("queued"));
    }

    #[test]
    fn exclusion_guard_rejects_confirming_campaigns() {
        assert!(!is_editable_campaign_status("confirming"));
    }

    #[test]
    fn exclusion_guard_rejects_editing_campaigns() {
        assert!(!is_editable_campaign_status("editing"));
    }
}
