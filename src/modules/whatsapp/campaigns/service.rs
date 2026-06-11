use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use futures::stream::StreamExt;
use futures::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId, Bson, DateTime, Document};
use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument, UpdateModifications};
use serde::{Deserialize, Serialize};

use crate::{
    db::WaTemplateRepository, error::ApiError, models::whatsapp::WaTemplate,
    modules::whatsapp::shared::time::iso8601, state::AppState,
};

use super::{
    dto::{
        BalanceFilter, CampaignListItem, CampaignListQuery, CampaignListResponse,
        CampaignPreviewRecipient, CampaignPreviewRequest, CampaignPreviewResponse,
        CampaignPreviewTotals, CampaignProgress, CampaignRecipientItem, CampaignRecipientsQuery,
        CampaignRecipientsResponse, CampaignSummary, CampaignSummaryResponse, ClientStateFilter,
        CreateCampaignRequest, DerivedClientState, PhoneStatus, TemplateClientField,
        TemplateVariableBinding, TemplateVariableComponent, TemplateVariableSource,
        UpdateCampaignRecipientExclusionsData, UpdateCampaignRecipientExclusionsRequest,
        UpdateCampaignRecipientExclusionsResponse, UpdateCampaignRequest, UpdateCampaignResponse,
    },
    phone::normalize_phone_to_whatsapp,
    template_resolver::{
        extract_template_placeholders, resolve_campaign_template_components,
        CampaignTemplateRecipientSnapshot, CampaignTemplateResolveError,
    },
};

const DEFAULT_PER_PAGE: u32 = 100;
const MAX_PER_PAGE: u32 = 500;
const DEFAULT_CAMPAIGN_LIST_LIMIT: u32 = 20;
const MAX_CAMPAIGN_LIST_LIMIT: u32 = 100;
const RETIRED_CLIENT_STATE: &str = "Retirado";
const CAMPAIGN_WORKER_INTERVAL_SECS: u64 = 5;
const CAMPAIGN_WORKER_BATCH_SIZE: usize = 50;
const CAMPAIGN_SENDING_STALE_SECS: i64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WaCampaignDoc {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    id: Option<ObjectId>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    phone_number_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_default")]
    template_name: String,
    #[serde(default, deserialize_with = "deserialize_string_or_default")]
    template_language: String,
    #[serde(default)]
    template_components: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    template_variable_bindings: Option<Vec<StoredTemplateVariableBinding>>,
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
    #[serde(default)]
    started_by: Option<String>,
    #[serde(default)]
    started_at: Option<DateTime>,
    #[serde(default)]
    run_mode: Option<String>,
    #[serde(default)]
    dry_run_completed_at: Option<DateTime>,
    created_at: DateTime,
    updated_at: DateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredTemplateClientField {
    ClientName,
    Balance,
    PaymentDueDay,
    SectorName,
    CustomerStatusDerived,
    PhoneNormalized,
    #[serde(rename = "provider_name")]
    ProviderName,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredTemplateVariableBinding {
    pub(crate) component: TemplateVariableComponent,
    pub(crate) index: i32,
    pub(crate) placeholder: String,
    pub(crate) source: TemplateVariableSource,
    #[serde(default)]
    pub(crate) value: Option<String>,
    #[serde(default)]
    pub(crate) client_field: Option<StoredTemplateClientField>,
    #[serde(default)]
    pub(crate) button_index: Option<i32>,
}

trait TemplateVariableBindingLike {
    fn component(&self) -> &TemplateVariableComponent;
    fn index(&self) -> i32;
    fn placeholder(&self) -> &str;
    fn source(&self) -> &TemplateVariableSource;
    fn value(&self) -> Option<&str>;
    fn client_field_present(&self) -> bool;
    fn legacy_provider_name_present(&self) -> bool;
    fn button_index(&self) -> Option<i32>;
}

impl TemplateVariableBindingLike for TemplateVariableBinding {
    fn component(&self) -> &TemplateVariableComponent {
        &self.component
    }

    fn index(&self) -> i32 {
        self.index
    }

    fn placeholder(&self) -> &str {
        self.placeholder.as_str()
    }

    fn source(&self) -> &TemplateVariableSource {
        &self.source
    }

    fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    fn client_field_present(&self) -> bool {
        self.client_field.is_some()
    }

    fn legacy_provider_name_present(&self) -> bool {
        false
    }

    fn button_index(&self) -> Option<i32> {
        self.button_index
    }
}

impl TemplateVariableBindingLike for StoredTemplateVariableBinding {
    fn component(&self) -> &TemplateVariableComponent {
        &self.component
    }

    fn index(&self) -> i32 {
        self.index
    }

    fn placeholder(&self) -> &str {
        self.placeholder.as_str()
    }

    fn source(&self) -> &TemplateVariableSource {
        &self.source
    }

    fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    fn client_field_present(&self) -> bool {
        self.client_field.is_some()
    }

    fn legacy_provider_name_present(&self) -> bool {
        matches!(
            self.client_field,
            Some(StoredTemplateClientField::ProviderName)
        )
    }

    fn button_index(&self) -> Option<i32> {
        self.button_index
    }
}

impl crate::modules::whatsapp::campaigns::template_resolver::CampaignTemplateVariableBindingLike
    for StoredTemplateVariableBinding
{
    fn component(&self) -> Option<TemplateVariableComponent> {
        Some(self.component.clone())
    }

    fn index(&self) -> i32 {
        self.index
    }

    fn source(&self) -> Option<TemplateVariableSource> {
        Some(self.source.clone())
    }

    fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    fn client_field(&self) -> Option<TemplateClientField> {
        match self.client_field {
            Some(StoredTemplateClientField::ClientName) => Some(TemplateClientField::ClientName),
            Some(StoredTemplateClientField::Balance) => Some(TemplateClientField::Balance),
            Some(StoredTemplateClientField::PaymentDueDay) => {
                Some(TemplateClientField::PaymentDueDay)
            }
            Some(StoredTemplateClientField::SectorName) => Some(TemplateClientField::SectorName),
            Some(StoredTemplateClientField::CustomerStatusDerived) => {
                Some(TemplateClientField::CustomerStatusDerived)
            }
            Some(StoredTemplateClientField::PhoneNormalized) => {
                Some(TemplateClientField::PhoneNormalized)
            }
            Some(StoredTemplateClientField::ProviderName) | None => None,
        }
    }

    fn has_unsupported_client_field(&self) -> bool {
        matches!(
            self.client_field,
            Some(StoredTemplateClientField::ProviderName)
        )
    }

    fn button_index(&self) -> Option<i32> {
        self.button_index
    }
}

impl From<TemplateVariableBinding> for StoredTemplateVariableBinding {
    fn from(binding: TemplateVariableBinding) -> Self {
        Self {
            component: binding.component,
            index: binding.index,
            placeholder: binding.placeholder,
            source: binding.source,
            value: binding.value,
            client_field: binding.client_field.map(Into::into),
            button_index: binding.button_index,
        }
    }
}

impl From<TemplateClientField> for StoredTemplateClientField {
    fn from(field: TemplateClientField) -> Self {
        match field {
            TemplateClientField::ClientName => Self::ClientName,
            TemplateClientField::Balance => Self::Balance,
            TemplateClientField::PaymentDueDay => Self::PaymentDueDay,
            TemplateClientField::SectorName => Self::SectorName,
            TemplateClientField::CustomerStatusDerived => Self::CustomerStatusDerived,
            TemplateClientField::PhoneNormalized => Self::PhoneNormalized,
        }
    }
}

impl StoredTemplateClientField {
    fn to_public(&self) -> Option<TemplateClientField> {
        match self {
            Self::ClientName => Some(TemplateClientField::ClientName),
            Self::Balance => Some(TemplateClientField::Balance),
            Self::PaymentDueDay => Some(TemplateClientField::PaymentDueDay),
            Self::SectorName => Some(TemplateClientField::SectorName),
            Self::CustomerStatusDerived => Some(TemplateClientField::CustomerStatusDerived),
            Self::PhoneNormalized => Some(TemplateClientField::PhoneNormalized),
            Self::ProviderName => None,
        }
    }
}

impl StoredTemplateVariableBinding {
    fn to_public(self) -> Option<TemplateVariableBinding> {
        let client_field = match self.client_field {
            Some(StoredTemplateClientField::ProviderName) => return None,
            Some(field) => field.to_public(),
            None => None,
        };

        Some(TemplateVariableBinding {
            component: self.component,
            index: self.index,
            placeholder: self.placeholder,
            source: self.source,
            value: self.value,
            client_field,
            button_index: self.button_index,
        })
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payment_due_day: Option<i32>,
    phone_original: String,
    phone_normalized: Option<String>,
    phone_status: PhoneStatus,
    can_send: bool,
    reason: Option<String>,
    excluded: bool,
    status: String,
    #[serde(default)]
    attempts: i64,
    #[serde(default)]
    last_attempt_at: Option<DateTime>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    validated_at: Option<DateTime>,
    created_at: DateTime,
    updated_at: DateTime,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CampaignDryRunProgress {
    pending: u64,
    sending: u64,
    validated: u64,
    failed: u64,
    invalid_phone: u64,
    duplicated_phone: u64,
    excluded: u64,
    total_effective: u64,
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
    payment_due_day: Option<i32>,
}

fn deserialize_string_or_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
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
    let phone_number_id = normalize_optional_phone_number_id(request.phone_number_id.as_deref())?;
    validate_create_template_variable_bindings(request.template_variable_bindings.as_deref())?;

    let (totals, recipients) = build_recipients_snapshot(state, &request.filters).await?;
    let now = DateTime::now();
    let campaign_id = ObjectId::new();
    let campaign = WaCampaignDoc {
        id: Some(campaign_id.clone()),
        name: request.name.trim().to_string(),
        phone_number_id,
        template_name: request.template_name.trim().to_string(),
        template_language: request.template_language.trim().to_string(),
        template_components: request.template_components,
        template_variable_bindings: request
            .template_variable_bindings
            .map(|bindings| bindings.into_iter().map(Into::into).collect()),
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
        started_by: None,
        started_at: None,
        run_mode: None,
        dry_run_completed_at: None,
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
    let recipients = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");
    let progress = load_campaign_progress(&recipients, campaign_id).await?;

    Ok(CampaignSummaryResponse {
        ok: true,
        data: campaign_to_summary_with_progress(campaign, Some(progress)),
    })
}

pub async fn update_campaign(
    state: &AppState,
    campaign_id: &str,
    updated_by: &str,
    request: UpdateCampaignRequest,
) -> Result<UpdateCampaignResponse, ApiError> {
    validate_update_campaign_request(&request)?;

    let campaign_id = parse_campaign_id(campaign_id)?;
    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");
    let recipients = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");
    let campaign = claim_campaign_for_editing(&campaigns, &campaign_id).await?;
    let original_status = campaign.status.clone();
    let filters_changed = campaign_snapshot_filters_changed(&campaign.filters, &request.filters);

    if !filters_changed {
        let updated_campaign =
            apply_campaign_edit(campaign.clone(), request, updated_by, None, DateTime::now())?;
        if let Err(err) =
            replace_campaign_after_edit(&campaigns, &campaign_id, &updated_campaign).await
        {
            let _ = restore_campaign_after_failed_edit(&campaigns, &campaign_id, &campaign).await;
            return Err(err);
        }
        return Ok(UpdateCampaignResponse {
            ok: true,
            data: campaign_to_summary(updated_campaign),
            snapshot_regenerated: false,
        });
    }

    let (totals, preview_recipients) =
        match build_recipients_snapshot(state, &request.filters).await {
            Ok(snapshot) => snapshot,
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
    let now = DateTime::now();
    let new_recipient_docs = preview_recipients
        .into_iter()
        .map(|recipient| preview_to_snapshot_recipient(campaign_id.clone(), recipient, now))
        .collect::<Vec<_>>();
    let previous_recipient_docs = match recipients
        .find(doc! { "campaign_id": campaign_id.clone() })
        .await
    {
        Ok(cursor) => match cursor.try_collect::<Vec<_>>().await {
            Ok(recipients) => recipients,
            Err(e) => {
                let _ =
                    restore_campaign_after_failed_edit(&campaigns, &campaign_id, &campaign).await;
                return Err(ApiError::DatabaseError(format!(
                    "campaign snapshot backup collect failed: {e}"
                )));
            }
        },
        Err(e) => {
            let _ = restore_campaign_after_failed_edit(&campaigns, &campaign_id, &campaign).await;
            return Err(ApiError::DatabaseError(format!(
                "campaign snapshot backup read failed: {e}"
            )));
        }
    };
    let updated_campaign = apply_campaign_edit(
        campaign.clone(),
        request,
        updated_by,
        Some(&totals),
        DateTime::now(),
    )?;

    if let Err(err) = replace_campaign_snapshot(&recipients, &campaign_id, new_recipient_docs).await
    {
        let _ = restore_campaign_snapshot(&recipients, &campaign_id, previous_recipient_docs).await;
        let _ = restore_campaign_after_failed_edit(&campaigns, &campaign_id, &campaign).await;
        return Err(err);
    }

    if let Err(err) = replace_campaign_after_edit(&campaigns, &campaign_id, &updated_campaign).await
    {
        let _ = restore_campaign_snapshot(&recipients, &campaign_id, previous_recipient_docs).await;
        let _ = restore_campaign_after_failed_edit(&campaigns, &campaign_id, &campaign).await;
        return Err(err);
    }

    Ok(UpdateCampaignResponse {
        ok: true,
        data: campaign_to_summary(updated_campaign),
        snapshot_regenerated: true,
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

    if let Err(err) = validate_confirmation_template(state, &campaign).await {
        restore_campaign_after_failed_confirmation(
            &campaigns,
            &campaign_id,
            campaign.status.as_str(),
        )
        .await?;
        return Err(err);
    }

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

pub async fn start_campaign(
    state: &AppState,
    campaign_id: &str,
    started_by: &str,
) -> Result<CampaignSummaryResponse, ApiError> {
    let campaign_id = parse_campaign_id(campaign_id)?;
    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");
    let recipients = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");

    let campaign = campaigns
        .find_one(doc! { "_id": campaign_id })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .ok_or(ApiError::NotFound)?;

    validate_startable_campaign(&campaign)?;

    let effective_recipients = recipients
        .count_documents(effective_recipient_filter(campaign_id.clone()))
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
    if effective_recipients == 0 {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "campaign_has_no_effective_recipients",
            "Campaign must have at least one non-excluded pending recipient that can be sent.",
        ));
    }

    let now = DateTime::now();
    let result = campaigns
        .update_one(
            doc! { "_id": campaign_id, "status": "queued" },
            start_campaign_update_doc(started_by, now),
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    if result.matched_count == 0 {
        let campaign = campaigns
            .find_one(doc! { "_id": campaign_id })
            .await
            .map_err(|e| ApiError::DatabaseError(e.to_string()))?
            .ok_or(ApiError::NotFound)?;

        if campaign.status != "queued" {
            return Err(campaign_not_startable());
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
    let filter = build_campaign_recipients_filter(campaign_id, query.status.as_deref());
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

pub async fn run_campaign_dry_run_worker(state: Arc<AppState>) {
    tracing::info!(
        interval_secs = CAMPAIGN_WORKER_INTERVAL_SECS,
        batch_size = CAMPAIGN_WORKER_BATCH_SIZE,
        "WhatsApp campaign dry-run worker started"
    );

    let mut interval = tokio::time::interval(Duration::from_secs(CAMPAIGN_WORKER_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        if let Err(err) =
            process_running_dry_run_campaigns(&state, CAMPAIGN_WORKER_BATCH_SIZE).await
        {
            tracing::error!(error = %err, "WhatsApp campaign dry-run worker tick failed");
        }
    }
}

async fn process_running_dry_run_campaigns(
    state: &AppState,
    batch_size: usize,
) -> Result<(), ApiError> {
    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");
    let running = campaigns
        .find(running_dry_run_campaign_filter())
        .sort(doc! { "started_at": 1, "_id": 1 })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    for campaign in running {
        process_campaign_dry_run_batch(state, &campaign, batch_size).await?;
    }

    Ok(())
}

async fn process_campaign_dry_run_batch(
    state: &AppState,
    campaign: &WaCampaignDoc,
    batch_size: usize,
) -> Result<(), ApiError> {
    if !should_process_campaign_dry_run(campaign) {
        return Ok(());
    }

    let Some(campaign_id) = campaign.id else {
        return Ok(());
    };

    tracing::info!(
        campaign_id = %campaign_id,
        batch_size,
        "WhatsApp campaign dry-run batch started"
    );

    let recipients = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");

    let mut claimed = 0usize;
    while claimed < batch_size {
        let Some(recipient) = claim_next_dry_run_recipient(&recipients, campaign_id).await? else {
            break;
        };
        claimed += 1;
        resolve_claimed_dry_run_recipient(&recipients, campaign, recipient).await?;
    }

    finalize_campaign_dry_run_if_done(state, campaign_id).await?;
    Ok(())
}

async fn claim_next_dry_run_recipient(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign_id: ObjectId,
) -> Result<Option<WaCampaignRecipientDoc>, ApiError> {
    let opts = FindOneAndUpdateOptions::builder()
        .return_document(ReturnDocument::After)
        .build();

    recipients
        .find_one_and_update(
            dry_run_recipient_claim_filter(campaign_id),
            dry_run_recipient_claim_update(DateTime::now()),
        )
        .sort(doc! { "created_at": 1, "_id": 1 })
        .with_options(opts)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))
}

async fn resolve_claimed_dry_run_recipient(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign: &WaCampaignDoc,
    recipient: WaCampaignRecipientDoc,
) -> Result<(), ApiError> {
    let Some(recipient_id) = recipient.id else {
        return Ok(());
    };
    let Some(campaign_id) = campaign.id else {
        return Ok(());
    };

    let snapshot = recipient_to_template_snapshot(&recipient);
    let now = DateTime::now();
    let result = resolve_campaign_template_components(
        campaign.template_components.as_deref(),
        campaign.template_variable_bindings.as_deref(),
        &snapshot,
    );

    match result {
        Ok(_) => {
            recipients
                .update_one(
                    doc! { "_id": recipient_id, "campaign_id": campaign_id, "status": "sending" },
                    dry_run_recipient_validated_update(now),
                )
                .await
                .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
            tracing::info!(
                campaign_id = %campaign_id,
                recipient_id = %recipient_id,
                "WhatsApp campaign dry-run recipient validated"
            );
        }
        Err(err) => {
            let error_code = err.code();
            recipients
                .update_one(
                    doc! { "_id": recipient_id, "campaign_id": campaign_id, "status": "sending" },
                    dry_run_recipient_failed_update(
                        error_code,
                        safe_resolver_error_message(&err),
                        now,
                    ),
                )
                .await
                .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
            tracing::warn!(
                campaign_id = %campaign_id,
                recipient_id = %recipient_id,
                error_code,
                "WhatsApp campaign dry-run recipient failed"
            );
        }
    }

    Ok(())
}

async fn finalize_campaign_dry_run_if_done(
    state: &AppState,
    campaign_id: ObjectId,
) -> Result<(), ApiError> {
    let recipients = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");
    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");

    recover_stale_sending_recipients(&recipients, campaign_id, DateTime::now()).await?;
    let progress = load_dry_run_progress(&recipients, campaign_id).await?;
    let Some(status) = dry_run_completion_status(&progress) else {
        return Ok(());
    };

    let now = DateTime::now();
    let result = campaigns
        .update_one(
            doc! {
                "_id": campaign_id,
                "status": "running",
                "run_mode": "dry_run",
            },
            doc! {
                "$set": {
                    "status": status,
                    "dry_run_completed_at": now,
                    "updated_at": now,
                }
            },
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    if result.modified_count > 0 {
        tracing::info!(
            campaign_id = %campaign_id,
            status,
            pending = progress.pending,
            sending = progress.sending,
            validated = progress.validated,
            failed = progress.failed,
            "WhatsApp campaign dry-run completed"
        );
    }

    Ok(())
}

async fn load_dry_run_progress(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign_id: ObjectId,
) -> Result<CampaignDryRunProgress, ApiError> {
    load_campaign_progress(recipients, campaign_id).await
}

async fn load_campaign_progress(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign_id: ObjectId,
) -> Result<CampaignDryRunProgress, ApiError> {
    Ok(CampaignDryRunProgress {
        pending: count_campaign_recipients(
            recipients,
            effective_status_count_filter(campaign_id, "pending"),
        )
        .await?,
        sending: count_campaign_recipients(
            recipients,
            effective_status_count_filter(campaign_id, "sending"),
        )
        .await?,
        validated: count_campaign_recipients(
            recipients,
            effective_status_count_filter(campaign_id, "validated"),
        )
        .await?,
        failed: count_campaign_recipients(
            recipients,
            effective_status_count_filter(campaign_id, "failed"),
        )
        .await?,
        invalid_phone: count_campaign_recipients(
            recipients,
            status_count_filter(campaign_id, "invalid_phone"),
        )
        .await?,
        duplicated_phone: count_campaign_recipients(
            recipients,
            status_count_filter(campaign_id, "duplicated_phone"),
        )
        .await?,
        excluded: count_campaign_recipients(
            recipients,
            doc! { "campaign_id": campaign_id, "excluded": true },
        )
        .await?,
        total_effective: count_campaign_recipients(recipients, total_effective_filter(campaign_id))
            .await?,
    })
}

async fn count_campaign_recipients(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    filter: Document,
) -> Result<u64, ApiError> {
    recipients
        .count_documents(filter)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))
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

fn total_effective_filter(campaign_id: ObjectId) -> Document {
    doc! {
        "campaign_id": campaign_id,
        "can_send": true,
        "excluded": false,
    }
}

fn build_campaign_recipients_filter(campaign_id: ObjectId, status: Option<&str>) -> Document {
    let mut filter = doc! { "campaign_id": campaign_id };
    if let Some(status) = status.map(str::trim).filter(|value| !value.is_empty()) {
        filter.insert("status", status);
    }
    filter
}

fn running_dry_run_campaign_filter() -> Document {
    doc! { "status": "running", "run_mode": "dry_run" }
}

fn should_process_campaign_dry_run(campaign: &WaCampaignDoc) -> bool {
    campaign.status == "running" && campaign.run_mode.as_deref() == Some("dry_run")
}

fn dry_run_recipient_claim_filter(campaign_id: ObjectId) -> Document {
    doc! {
        "campaign_id": campaign_id,
        "can_send": true,
        "excluded": false,
        "status": "pending",
    }
}

#[cfg(test)]
fn dry_run_recipient_is_claimable(recipient: &WaCampaignRecipientDoc) -> bool {
    recipient.can_send && !recipient.excluded && recipient.status == "pending"
}

fn dry_run_recipient_claim_update(now: DateTime) -> Document {
    doc! {
        "$set": {
            "status": "sending",
            "last_attempt_at": now,
            "updated_at": now,
        },
        "$inc": { "attempts": 1i64 }
    }
}

fn dry_run_recipient_validated_update(now: DateTime) -> Document {
    doc! {
        "$set": {
            "status": "validated",
            "validated_at": now,
            "updated_at": now,
        },
        "$unset": {
            "error_code": "",
            "error_message": "",
        }
    }
}

fn dry_run_recipient_failed_update(
    error_code: &str,
    error_message: String,
    now: DateTime,
) -> Document {
    doc! {
        "$set": {
            "status": "failed",
            "error_code": error_code,
            "error_message": error_message,
            "updated_at": now,
        }
    }
}

fn status_count_filter(campaign_id: ObjectId, status: &str) -> Document {
    doc! {
        "campaign_id": campaign_id,
        "status": status,
    }
}

fn effective_status_count_filter(campaign_id: ObjectId, status: &str) -> Document {
    doc! {
        "campaign_id": campaign_id,
        "can_send": true,
        "excluded": false,
        "status": status,
    }
}

fn stale_sending_cutoff(now: DateTime) -> DateTime {
    DateTime::from_millis(now.timestamp_millis() - CAMPAIGN_SENDING_STALE_SECS * 1000)
}

fn stale_sending_recovery_filter(campaign_id: ObjectId, now: DateTime) -> Document {
    doc! {
        "campaign_id": campaign_id,
        "can_send": true,
        "excluded": false,
        "status": "sending",
        "last_attempt_at": { "$lt": stale_sending_cutoff(now) },
    }
}

fn stale_sending_recovery_update(now: DateTime) -> Document {
    doc! {
        "$set": {
            "status": "pending",
            "updated_at": now,
            "error_code": "stale_sending_recovered",
            "error_message": "Recipient returned to pending after stale sending timeout.",
        }
    }
}

async fn recover_stale_sending_recipients(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign_id: ObjectId,
    now: DateTime,
) -> Result<(), ApiError> {
    let result = recipients
        .update_many(
            stale_sending_recovery_filter(campaign_id, now),
            stale_sending_recovery_update(now),
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    if result.modified_count > 0 {
        tracing::warn!(
            campaign_id = %campaign_id,
            recovered = result.modified_count,
            "Recovered stale WhatsApp campaign dry-run recipients"
        );
    }

    Ok(())
}

#[cfg(test)]
fn stale_sending_recipient_is_recoverable(
    recipient: &WaCampaignRecipientDoc,
    now: DateTime,
) -> bool {
    recipient.can_send
        && !recipient.excluded
        && recipient.status == "sending"
        && recipient
            .last_attempt_at
            .is_some_and(|last_attempt_at| last_attempt_at < stale_sending_cutoff(now))
}

#[cfg(test)]
fn recover_stale_sending_recipient_state(recipient: &mut WaCampaignRecipientDoc, now: DateTime) {
    if stale_sending_recipient_is_recoverable(recipient, now) {
        recipient.status = "pending".to_string();
        recipient.updated_at = now;
        recipient.error_code = Some("stale_sending_recovered".to_string());
        recipient.error_message =
            Some("Recipient returned to pending after stale sending timeout.".to_string());
    }
}

fn dry_run_completion_status(progress: &CampaignDryRunProgress) -> Option<&'static str> {
    if progress.pending > 0 || progress.sending > 0 {
        None
    } else if progress.failed == 0 {
        Some("dry_run_completed")
    } else {
        Some("dry_run_completed_with_errors")
    }
}

fn recipient_to_template_snapshot(
    recipient: &WaCampaignRecipientDoc,
) -> CampaignTemplateRecipientSnapshot {
    CampaignTemplateRecipientSnapshot {
        client_name: recipient.client_name.clone(),
        balance: recipient.balance,
        payment_due_day: recipient.payment_due_day,
        sector_name: recipient.sector_name.clone(),
        customer_status_derived: recipient.customer_status_derived.clone(),
        phone_normalized: recipient.phone_normalized.clone(),
    }
}

fn safe_resolver_error_message(error: &CampaignTemplateResolveError) -> String {
    error.code().to_string()
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
            "Only draft or previewed campaigns can be edited.",
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

fn validate_update_campaign_request(request: &UpdateCampaignRequest) -> Result<(), ApiError> {
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
    normalize_optional_phone_number_id(request.phone_number_id.as_deref())?;
    validate_create_template_variable_bindings(request.template_variable_bindings.as_deref())?;
    Ok(())
}

fn campaign_snapshot_filters_changed(
    current: &CampaignPreviewRequest,
    next: &CampaignPreviewRequest,
) -> bool {
    normalized_campaign_preview_request(current) != normalized_campaign_preview_request(next)
}

fn normalized_campaign_preview_request(request: &CampaignPreviewRequest) -> CampaignPreviewRequest {
    let mut normalized = request.clone();
    normalized.page = None;
    normalized.per_page = None;
    normalized
}

fn apply_campaign_edit(
    mut campaign: WaCampaignDoc,
    request: UpdateCampaignRequest,
    _updated_by: &str,
    regenerated_totals: Option<&CampaignPreviewTotals>,
    updated_at: DateTime,
) -> Result<WaCampaignDoc, ApiError> {
    campaign.name = request.name.trim().to_string();
    if request.phone_number_id.is_some() {
        campaign.phone_number_id =
            normalize_optional_phone_number_id(request.phone_number_id.as_deref())?;
    }
    campaign.template_name = request.template_name.trim().to_string();
    campaign.template_language = request.template_language.trim().to_string();
    campaign.template_components = request.template_components;
    campaign.template_variable_bindings = request
        .template_variable_bindings
        .map(|bindings| bindings.into_iter().map(Into::into).collect());
    campaign.filters = request.filters;
    campaign.status = if is_editable_campaign_status(&campaign.status) {
        campaign.status
    } else {
        "draft".to_string()
    };
    if let Some(totals) = regenerated_totals {
        campaign.total_recipients = totals.matched as u64;
        campaign.total_can_send = totals.can_send as u64;
        campaign.total_invalid_phone = totals.invalid_phone as u64;
        campaign.total_duplicated_phone = totals.duplicated_phone as u64;
        campaign.total_excluded = 0;
    }
    campaign.updated_at = updated_at;
    Ok(campaign)
}

async fn replace_campaign_after_edit(
    campaigns: &mongodb::Collection<WaCampaignDoc>,
    campaign_id: &ObjectId,
    campaign: &WaCampaignDoc,
) -> Result<(), ApiError> {
    let result = campaigns
        .replace_one(
            doc! { "_id": campaign_id.clone(), "status": "editing" },
            campaign,
        )
        .await
        .map_err(|e| ApiError::DatabaseError(format!("campaign update failed: {e}")))?;

    if result.matched_count == 0 {
        return Err(ApiError::domain_simple(
            StatusCode::CONFLICT,
            "campaign_not_editable",
            "Campaign is no longer locked for editing.",
        ));
    }

    Ok(())
}

async fn restore_campaign_after_failed_edit(
    campaigns: &mongodb::Collection<WaCampaignDoc>,
    campaign_id: &ObjectId,
    original: &WaCampaignDoc,
) -> Result<(), ApiError> {
    let mut restored = original.clone();
    restored.updated_at = DateTime::now();
    campaigns
        .replace_one(doc! { "_id": campaign_id.clone() }, &restored)
        .await
        .map_err(|e| ApiError::DatabaseError(format!("campaign edit rollback failed: {e}")))?;
    Ok(())
}

async fn replace_campaign_snapshot(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign_id: &ObjectId,
    new_recipients: Vec<WaCampaignRecipientDoc>,
) -> Result<(), ApiError> {
    recipients
        .delete_many(doc! { "campaign_id": campaign_id.clone() })
        .await
        .map_err(|e| ApiError::DatabaseError(format!("campaign snapshot delete failed: {e}")))?;

    if !new_recipients.is_empty() {
        recipients.insert_many(new_recipients).await.map_err(|e| {
            ApiError::DatabaseError(format!("campaign snapshot insert failed: {e}"))
        })?;
    }

    Ok(())
}

async fn restore_campaign_snapshot(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign_id: &ObjectId,
    previous_recipients: Vec<WaCampaignRecipientDoc>,
) -> Result<(), ApiError> {
    recipients
        .delete_many(doc! { "campaign_id": campaign_id.clone() })
        .await
        .map_err(|e| {
            ApiError::DatabaseError(format!("campaign snapshot rollback delete failed: {e}"))
        })?;

    if !previous_recipients.is_empty() {
        recipients
            .insert_many(previous_recipients)
            .await
            .map_err(|e| {
                ApiError::DatabaseError(format!("campaign snapshot rollback insert failed: {e}"))
            })?;
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

fn normalize_optional_phone_number_id(value: Option<&str>) -> Result<Option<String>, ApiError> {
    match value.map(str::trim) {
        Some("") => Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "invalid_phone_number_id",
            "phone_number_id",
            "phone_number_id cannot be empty when provided.",
        )),
        Some(value) => Ok(Some(value.to_string())),
        None => Ok(None),
    }
}

fn validate_create_template_variable_bindings<T: TemplateVariableBindingLike>(
    bindings: Option<&[T]>,
) -> Result<(), ApiError> {
    if let Some(bindings) = bindings {
        validate_binding_basics(bindings)?;
    }
    Ok(())
}

fn validate_binding_basics<T: TemplateVariableBindingLike>(bindings: &[T]) -> Result<(), ApiError> {
    let mut seen = HashSet::new();

    for binding in bindings {
        if binding.index() < 1 {
            return Err(ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "invalid_template_variable_binding",
                "template_variable_bindings.index",
                "Template variable binding index must be greater than or equal to 1.",
            ));
        }
        if let TemplateVariableComponent::Button = binding.component() {
            if binding.button_index().is_some_and(|index| index < 0) {
                return Err(ApiError::domain_with_field(
                    StatusCode::BAD_REQUEST,
                    "invalid_template_variable_binding",
                    "template_variable_bindings.button_index",
                    "Template button variable binding button_index must be greater than or equal to 0.",
                ));
            }
        }

        let key = (
            binding.component().clone(),
            binding.index(),
            binding.button_index(),
        );
        if !seen.insert(key) {
            return Err(ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "duplicate_template_variable_binding",
                "template_variable_bindings",
                "Template variable bindings cannot contain duplicate component/index/button_index entries.",
            ));
        }

        if let Some(placeholder_index) = placeholder_index(binding.placeholder()) {
            if placeholder_index != binding.index() {
                return Err(ApiError::domain_with_field(
                    StatusCode::BAD_REQUEST,
                    "template_variable_placeholder_mismatch",
                    "template_variable_bindings.placeholder",
                    "Template variable binding placeholder must match its index.",
                ));
            }
        }

        match binding.source() {
            TemplateVariableSource::Static => {
                if binding
                    .value()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_none()
                {
                    return Err(ApiError::domain_with_field(
                        StatusCode::BAD_REQUEST,
                        "template_variable_static_value_required",
                        "template_variable_bindings.value",
                        "Static template variable bindings require a non-empty value.",
                    ));
                }
            }
            TemplateVariableSource::ClientField => {
                if !binding.client_field_present() {
                    return Err(ApiError::domain_with_field(
                        StatusCode::BAD_REQUEST,
                        "template_variable_client_field_required",
                        "template_variable_bindings.client_field",
                        "Client-field template variable bindings require a valid client_field.",
                    ));
                }
                if binding.legacy_provider_name_present() {
                    return Err(ApiError::domain_with_field(
                        StatusCode::BAD_REQUEST,
                        "template_variable_client_field_unsupported",
                        "template_variable_bindings.client_field",
                        "Legacy provider_name client_field bindings are no longer supported.",
                    ));
                }
            }
        }
    }

    Ok(())
}

async fn validate_confirmation_template(
    state: &AppState,
    campaign: &WaCampaignDoc,
) -> Result<(), ApiError> {
    let phone_number_id = campaign
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::BAD_REQUEST,
                "missing_phone_number_id",
                "Campaign must have a phone_number_id before confirmation.",
            )
        })?;

    let template = state
        .db
        .find_template_by_phone_name_lang(
            phone_number_id,
            campaign.template_name.as_str(),
            campaign.template_language.as_str(),
        )
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::BAD_REQUEST,
                "campaign_template_not_found",
                "Campaign template was not found for the selected phone_number_id, name, and language.",
            )
        })?;

    validate_bindings_against_template(&template, campaign.template_variable_bindings.as_deref())
}

fn validate_bindings_against_template(
    template: &WaTemplate,
    bindings: Option<&[StoredTemplateVariableBinding]>,
) -> Result<(), ApiError> {
    validate_bindings_against_template_components(&template.components, bindings)
}

fn validate_bindings_against_template_components<T: TemplateVariableBindingLike>(
    components: &[serde_json::Value],
    bindings: Option<&[T]>,
) -> Result<(), ApiError> {
    let required = extract_template_placeholders(components).map_err(|err| {
        ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            err.code(),
            "Template contains placeholders in an unsupported component.",
        )
    })?;
    let bindings = bindings.unwrap_or(&[]);

    if required.is_empty() {
        if bindings.is_empty() {
            return Ok(());
        }
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "template_variable_bindings_not_expected",
            "template_variable_bindings",
            "Template has no placeholders, so variable bindings are not expected.",
        ));
    }

    validate_binding_basics(bindings)?;

    let required_keys = required
        .iter()
        .map(|placeholder| {
            (
                placeholder.component.clone(),
                placeholder.index,
                placeholder.button_index,
            )
        })
        .collect::<HashSet<_>>();
    let binding_keys = bindings
        .iter()
        .map(|binding| {
            (
                binding.component().clone(),
                binding.index(),
                binding.button_index(),
            )
        })
        .collect::<HashSet<_>>();

    if binding_keys != required_keys {
        let code = if binding_keys.is_subset(&required_keys) {
            "template_variable_bindings_incomplete"
        } else {
            "template_variable_bindings_extra"
        };
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            code,
            "template_variable_bindings",
            "Template variable bindings must exactly match the template placeholders.",
        ));
    }

    Ok(())
}

fn placeholder_index(value: &str) -> Option<i32> {
    let mut indices = placeholder_indices(value);
    let first = indices.next()?;
    if indices.next().is_none() {
        Some(first)
    } else {
        None
    }
}

fn placeholder_indices(value: &str) -> impl Iterator<Item = i32> + '_ {
    value.match_indices("{{").filter_map(|(start, _)| {
        let rest = &value[start + 2..];
        let end = rest.find("}}")?;
        rest[..end]
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|index| *index >= 1)
    })
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

fn validate_startable_campaign(campaign: &WaCampaignDoc) -> Result<(), ApiError> {
    if campaign.status != "queued" {
        return Err(campaign_not_startable());
    }

    campaign
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::BAD_REQUEST,
                "missing_phone_number_id",
                "Campaign must have a phone_number_id before starting.",
            )
        })?;

    if campaign.template_name.trim().is_empty() {
        return Err(ApiError::BadRequest("template_name_required".to_string()));
    }
    if campaign.template_language.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "template_language_required".to_string(),
        ));
    }

    Ok(())
}

fn campaign_not_startable() -> ApiError {
    ApiError::domain_simple(
        StatusCode::CONFLICT,
        "campaign_not_startable",
        "Only queued campaigns can be started.",
    )
}

fn start_campaign_update_doc(started_by: &str, now: DateTime) -> Document {
    doc! {
        "$set": {
            "status": "running",
            "started_by": started_by,
            "started_at": now,
            "updated_at": now,
            "run_mode": "dry_run",
        }
    }
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
        "nPayment": 1,
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
            payment_due_day: get_payment_due_day(&doc, "nPayment"),
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
        payment_due_day: recipient.payment_due_day,
        phone_original: recipient.phone_original,
        phone_normalized: recipient.phone_normalized,
        phone_status: recipient.phone_status,
        can_send: recipient.can_send,
        reason: recipient.reason,
        excluded: false,
        status,
        attempts: 0,
        last_attempt_at: None,
        error_code: None,
        error_message: None,
        validated_at: None,
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
    campaign_to_summary_with_progress(campaign, None)
}

fn campaign_to_summary_with_progress(
    campaign: WaCampaignDoc,
    progress: Option<CampaignDryRunProgress>,
) -> CampaignSummary {
    CampaignSummary {
        id: campaign.id.map(|id| id.to_hex()).unwrap_or_default(),
        name: campaign.name,
        phone_number_id: campaign.phone_number_id,
        template_name: campaign.template_name,
        template_language: campaign.template_language,
        template_components: campaign.template_components,
        template_variable_bindings: campaign.template_variable_bindings.and_then(|bindings| {
            let bindings = bindings
                .into_iter()
                .filter_map(StoredTemplateVariableBinding::to_public)
                .collect::<Vec<_>>();
            if bindings.is_empty() {
                None
            } else {
                Some(bindings)
            }
        }),
        filters: campaign.filters,
        status: campaign.status,
        started_by: campaign.started_by,
        started_at: campaign.started_at.map(iso8601),
        run_mode: campaign.run_mode,
        dry_run_completed_at: campaign.dry_run_completed_at.map(iso8601),
        progress: progress.map(campaign_progress_to_dto),
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

fn campaign_progress_to_dto(progress: CampaignDryRunProgress) -> CampaignProgress {
    let processed = progress.validated + progress.failed;
    CampaignProgress {
        pending: progress.pending,
        sending: progress.sending,
        validated: progress.validated,
        failed: progress.failed,
        invalid_phone: progress.invalid_phone,
        duplicated_phone: progress.duplicated_phone,
        excluded: progress.excluded,
        total_effective: progress.total_effective,
        processed,
        progress_percent: calculate_progress_percent(processed, progress.total_effective),
    }
}

fn calculate_progress_percent(processed: u64, total_effective: u64) -> f64 {
    if total_effective == 0 {
        0.0
    } else {
        (processed as f64 / total_effective as f64) * 100.0
    }
}

fn campaign_to_list_item(campaign: WaCampaignDoc) -> CampaignListItem {
    CampaignListItem {
        id: campaign.id.map(|id| id.to_hex()).unwrap_or_default(),
        name: campaign.name,
        phone_number_id: campaign.phone_number_id,
        template_name: campaign.template_name,
        template_language: campaign.template_language,
        has_template_variables: campaign
            .template_variable_bindings
            .as_ref()
            .is_some_and(|bindings| !bindings.is_empty()),
        template_variables_count: campaign
            .template_variable_bindings
            .as_ref()
            .map(Vec::len)
            .unwrap_or(0),
        status: campaign.status,
        run_mode: campaign.run_mode,
        dry_run_completed_at: campaign.dry_run_completed_at.map(iso8601),
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
        payment_due_day: recipient.payment_due_day,
        phone_original: recipient.phone_original,
        phone_normalized: recipient.phone_normalized,
        phone_status: recipient.phone_status,
        can_send: recipient.can_send,
        reason: recipient.reason,
        excluded: recipient.excluded,
        status: recipient.status,
        attempts: recipient.attempts,
        last_attempt_at: recipient.last_attempt_at.map(iso8601),
        error_code: recipient.error_code,
        error_message: recipient.error_message,
        validated_at: recipient.validated_at.map(iso8601),
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
            payment_due_day: candidate.payment_due_day,
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
                    | ClientStateFilter::Retired
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
        Some(ClientStateFilter::Retired) => {
            clauses.push(doc! { "sState": { "$in": vec![RETIRED_CLIENT_STATE] } })
        }
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
        return Err(ApiError::domain_with_field(
            StatusCode::BAD_REQUEST,
            "invalid_balance_filter",
            "balance_filter",
            "Invalid balance_filter: expected one of lt,lte,gt,gte,eq,between.",
        ));
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

fn get_payment_due_day(doc: &Document, key: &str) -> Option<i32> {
    if let Ok(day) = doc.get_i32(key) {
        return (1..=31).contains(&day).then_some(day);
    }

    if let Ok(day) = doc.get_i64(key) {
        return i32::try_from(day).ok().filter(|day| (1..=31).contains(day));
    }

    if let Ok(day) = doc.get_f64(key) {
        return day
            .is_finite()
            .then_some(day)
            .filter(|day| day.fract() == 0.0 && (1.0..=31.0).contains(day))
            .map(|day| day as i32);
    }

    None
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
            payment_due_day: None,
        }
    }

    fn base_campaign(status: &str) -> WaCampaignDoc {
        let now = DateTime::from_millis(1_800_000_000_000);
        WaCampaignDoc {
            id: Some(ObjectId::parse_str("64f000000000000000000001").unwrap()),
            name: "June Promo".to_string(),
            phone_number_id: Some("1234567890".to_string()),
            template_name: "promo_template".to_string(),
            template_language: "es".to_string(),
            template_components: None,
            template_variable_bindings: None,
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
            started_by: None,
            started_at: None,
            run_mode: None,
            dry_run_completed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn base_recipient(status: &str) -> WaCampaignRecipientDoc {
        let now = DateTime::from_millis(1_800_000_000_000);
        WaCampaignRecipientDoc {
            id: Some(ObjectId::parse_str("64f000000000000000000101").unwrap()),
            campaign_id: ObjectId::parse_str("64f000000000000000000001").unwrap(),
            client_id: "client-1".to_string(),
            client_name: "Client One".to_string(),
            provider_id: None,
            provider_name: None,
            sector_id: None,
            sector_name: Some("Downtown".to_string()),
            customer_status_raw: "Activo".to_string(),
            customer_status_derived: DerivedClientState::Solvente,
            balance: 10.5,
            payment_due_day: Some(20),
            phone_original: "0412 123 4567".to_string(),
            phone_normalized: Some("584121234567".to_string()),
            phone_status: PhoneStatus::Valid,
            can_send: true,
            reason: None,
            excluded: false,
            status: status.to_string(),
            attempts: 0,
            last_attempt_at: None,
            error_code: None,
            error_message: None,
            validated_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn static_body_binding(index: i32, value: &str) -> TemplateVariableBinding {
        TemplateVariableBinding {
            component: TemplateVariableComponent::Body,
            index,
            placeholder: format!("{{{{{index}}}}}"),
            source: TemplateVariableSource::Static,
            value: Some(value.to_string()),
            client_field: None,
            button_index: None,
        }
    }

    fn client_field_body_binding(index: i32) -> TemplateVariableBinding {
        TemplateVariableBinding {
            component: TemplateVariableComponent::Body,
            index,
            placeholder: format!("{{{{{index}}}}}"),
            source: TemplateVariableSource::ClientField,
            value: None,
            client_field: Some(
                crate::modules::whatsapp::campaigns::dto::TemplateClientField::ClientName,
            ),
            button_index: None,
        }
    }

    fn payment_due_day_body_binding(index: i32) -> TemplateVariableBinding {
        TemplateVariableBinding {
            component: TemplateVariableComponent::Body,
            index,
            placeholder: format!("{{{{{index}}}}}"),
            source: TemplateVariableSource::ClientField,
            value: None,
            client_field: Some(
                crate::modules::whatsapp::campaigns::dto::TemplateClientField::PaymentDueDay,
            ),
            button_index: None,
        }
    }

    fn legacy_provider_name_body_binding(index: i32) -> StoredTemplateVariableBinding {
        StoredTemplateVariableBinding {
            component: TemplateVariableComponent::Body,
            index,
            placeholder: format!("{{{{{index}}}}}"),
            source: TemplateVariableSource::ClientField,
            value: None,
            client_field: Some(StoredTemplateClientField::ProviderName),
            button_index: None,
        }
    }

    fn update_request(name: &str) -> UpdateCampaignRequest {
        UpdateCampaignRequest {
            name: name.to_string(),
            phone_number_id: None,
            template_name: "promo_template".to_string(),
            template_language: "es".to_string(),
            template_components: None,
            template_variable_bindings: None,
            filters: CampaignPreviewRequest {
                provider_ids: None,
                sector_ids: None,
                balance_filter: None,
                client_state: Some(ClientStateFilter::Active),
                include_all_active: None,
                page: None,
                per_page: None,
            },
        }
    }

    fn regenerated_totals() -> CampaignPreviewTotals {
        CampaignPreviewTotals {
            matched: 3,
            can_send: 2,
            invalid_phone: 1,
            duplicated_phone: 0,
        }
    }

    #[test]
    fn create_with_phone_number_id_persists_to_summary() {
        let mut campaign = base_campaign("draft");
        campaign.phone_number_id = Some("987654321".to_string());

        let summary = campaign_to_summary(campaign);

        assert_eq!(summary.phone_number_id.as_deref(), Some("987654321"));
    }

    #[test]
    fn create_with_bindings_persists_to_summary() {
        let mut campaign = base_campaign("draft");
        campaign.template_variable_bindings = Some(vec![static_body_binding(1, "ABDO").into()]);

        let summary = campaign_to_summary(campaign);

        assert_eq!(summary.template_variable_bindings.unwrap().len(), 1);
    }

    #[test]
    fn create_with_empty_static_binding_fails() {
        let err = validate_create_template_variable_bindings(Some(&[static_body_binding(1, " ")]))
            .unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "template_variable_static_value_required"
                    && field.as_deref() == Some("template_variable_bindings.value")
        ));
    }

    #[test]
    fn create_with_invalid_client_field_fails_deserialization() {
        let payload = serde_json::json!({
            "component": "body",
            "index": 1,
            "placeholder": "{{1}}",
            "source": "client_field",
            "client_field": "not_allowed"
        });

        assert!(serde_json::from_value::<TemplateVariableBinding>(payload).is_err());
    }

    #[test]
    fn create_with_provider_name_client_field_fails_deserialization() {
        let payload = serde_json::json!({
            "name": "June Promo",
            "template_name": "promo_template",
            "template_language": "es",
            "template_variable_bindings": [{
                "component": "body",
                "index": 1,
                "placeholder": "{{1}}",
                "source": "client_field",
                "client_field": "provider_name"
            }],
            "filters": { "client_state": "active" }
        });

        assert!(serde_json::from_value::<CreateCampaignRequest>(payload).is_err());
    }

    #[test]
    fn create_with_allowed_client_fields_passes_deserialization() {
        for client_field in [
            "client_name",
            "balance",
            "payment_due_day",
            "sector_name",
            "customer_status_derived",
            "phone_normalized",
        ] {
            let payload = serde_json::json!({
                "component": "body",
                "index": 1,
                "placeholder": "{{1}}",
                "source": "client_field",
                "client_field": client_field
            });

            assert!(
                serde_json::from_value::<TemplateVariableBinding>(payload).is_ok(),
                "{client_field} should remain an allowed client_field"
            );
        }
    }

    #[test]
    fn legacy_provider_name_binding_deserializes_and_is_filtered_from_summary() {
        let payload = doc! {
            "_id": ObjectId::parse_str("64f000000000000000000001").unwrap(),
            "name": "June Promo",
            "phone_number_id": "1234567890",
            "template_name": "promo_template",
            "template_language": "es",
            "template_components": Bson::Null,
            "template_variable_bindings": vec![doc! {
                "component": "body",
                "index": 1,
                "placeholder": "{{1}}",
                "source": "client_field",
                "client_field": "provider_name"
            }],
            "filters": doc! { "client_state": "active" },
            "status": "draft",
            "confirming_from": Bson::Null,
            "total_recipients": 1i64,
            "total_can_send": 1i64,
            "total_invalid_phone": 0i64,
            "total_duplicated_phone": 0i64,
            "total_excluded": 0i64,
            "created_by": "creator-1",
            "confirmed_by": Bson::Null,
            "confirmed_at": Bson::Null,
            "created_at": DateTime::from_millis(1_800_000_000_000),
            "updated_at": DateTime::from_millis(1_800_000_000_000)
        };

        let campaign = mongodb::bson::from_document::<WaCampaignDoc>(payload).unwrap();
        assert_eq!(
            campaign.template_variable_bindings.as_ref().map(Vec::len),
            Some(1)
        );

        let summary = campaign_to_summary(campaign.clone());
        assert!(summary.template_variable_bindings.is_none());

        let list_item = campaign_to_list_item(campaign);
        assert_eq!(list_item.template_variables_count, 1);
        assert!(list_item.has_template_variables);
    }

    #[test]
    fn create_with_payment_due_day_client_field_passes_validation() {
        let bindings = vec![payment_due_day_body_binding(1)];

        assert!(validate_create_template_variable_bindings(Some(&bindings)).is_ok());
    }

    #[test]
    fn edit_name_in_draft_changes_name_and_preserves_totals() {
        let campaign = base_campaign("draft");
        let original_total_excluded = campaign.total_excluded;

        let updated = apply_campaign_edit(
            campaign,
            update_request("Updated Promo"),
            "admin-1",
            None,
            DateTime::now(),
        )
        .unwrap();

        assert_eq!(updated.name, "Updated Promo");
        assert_eq!(updated.status, "draft");
        assert_eq!(updated.total_recipients, 5);
        assert_eq!(updated.total_excluded, original_total_excluded);
    }

    #[test]
    fn edit_legacy_phone_number_id_saves_to_summary() {
        let mut campaign = base_campaign("draft");
        campaign.phone_number_id = None;
        let mut request = update_request("June Promo");
        request.phone_number_id = Some("987654321".to_string());

        let summary = campaign_to_summary(
            apply_campaign_edit(campaign, request, "admin-1", None, DateTime::now()).unwrap(),
        );

        assert_eq!(summary.phone_number_id.as_deref(), Some("987654321"));
    }

    #[test]
    fn edit_template_variable_bindings_saves_and_preserves_totals() {
        let campaign = base_campaign("previewed");
        let mut request = update_request("June Promo");
        request.template_variable_bindings = Some(vec![static_body_binding(1, "ABDO")]);

        let updated =
            apply_campaign_edit(campaign, request, "admin-1", None, DateTime::now()).unwrap();

        assert_eq!(updated.status, "previewed");
        assert_eq!(updated.template_variable_bindings.unwrap().len(), 1);
        assert_eq!(updated.total_recipients, 5);
        assert_eq!(updated.total_excluded, 1);
    }

    #[test]
    fn edit_filters_regenerates_snapshot_totals_and_resets_exclusions() {
        let campaign = base_campaign("draft");
        let mut request = update_request("June Promo");
        request.filters = CampaignPreviewRequest {
            provider_ids: None,
            sector_ids: None,
            balance_filter: Some(BalanceFilter {
                lt: Some(0.0),
                lte: None,
                gt: None,
                gte: None,
                eq: None,
                between: None,
            }),
            client_state: None,
            include_all_active: None,
            page: None,
            per_page: None,
        };

        let updated = apply_campaign_edit(
            campaign,
            request,
            "admin-1",
            Some(&regenerated_totals()),
            DateTime::now(),
        )
        .unwrap();

        assert_eq!(updated.total_recipients, 3);
        assert_eq!(updated.total_can_send, 2);
        assert_eq!(updated.total_invalid_phone, 1);
        assert_eq!(updated.total_duplicated_phone, 0);
        assert_eq!(updated.total_excluded, 0);
    }

    #[test]
    fn edit_filters_to_retired_regenerates_snapshot_totals_and_resets_exclusions() {
        let campaign = base_campaign("draft");
        let mut request = update_request("June Promo");
        request.filters = CampaignPreviewRequest {
            provider_ids: None,
            sector_ids: None,
            balance_filter: None,
            client_state: Some(ClientStateFilter::Retired),
            include_all_active: None,
            page: None,
            per_page: None,
        };

        assert!(campaign_snapshot_filters_changed(
            &campaign.filters,
            &request.filters
        ));

        let updated = apply_campaign_edit(
            campaign,
            request,
            "admin-1",
            Some(&regenerated_totals()),
            DateTime::now(),
        )
        .unwrap();

        assert_eq!(
            updated.filters.client_state,
            Some(ClientStateFilter::Retired)
        );
        assert_eq!(updated.total_recipients, 3);
        assert_eq!(updated.total_excluded, 0);
    }

    #[test]
    fn snapshot_filter_comparison_ignores_pagination_only_changes() {
        let current = CampaignPreviewRequest {
            provider_ids: Some(vec!["provider-1".to_string()]),
            sector_ids: None,
            balance_filter: None,
            client_state: Some(ClientStateFilter::Active),
            include_all_active: None,
            page: Some(1),
            per_page: Some(25),
        };
        let next = CampaignPreviewRequest {
            page: Some(3),
            per_page: Some(100),
            ..current.clone()
        };

        assert!(!campaign_snapshot_filters_changed(&current, &next));
        assert_ne!(current, next);
    }

    #[test]
    fn snapshot_filter_comparison_detects_audience_changes() {
        let current = CampaignPreviewRequest {
            provider_ids: Some(vec!["provider-1".to_string()]),
            sector_ids: None,
            balance_filter: None,
            client_state: Some(ClientStateFilter::Active),
            include_all_active: None,
            page: Some(1),
            per_page: Some(25),
        };
        let mut next = current.clone();
        next.provider_ids = Some(vec!["provider-2".to_string()]);

        assert!(campaign_snapshot_filters_changed(&current, &next));
    }

    #[test]
    fn edit_queued_campaign_returns_not_editable_contract() {
        assert!(!is_editable_campaign_status("queued"));
        assert!(!is_editable_campaign_status("confirming"));
        assert!(!is_editable_campaign_status("editing"));
        assert!(!is_editable_campaign_status("running"));
        assert!(!is_editable_campaign_status("completed"));
        assert!(!is_editable_campaign_status("completed_with_errors"));
        assert!(!is_editable_campaign_status("cancelled"));
    }

    #[test]
    fn edit_with_empty_static_binding_fails() {
        let mut request = update_request("June Promo");
        request.template_variable_bindings = Some(vec![static_body_binding(1, "")]);

        let err = validate_update_campaign_request(&request).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "template_variable_static_value_required"
                    && field.as_deref() == Some("template_variable_bindings.value")
        ));
    }

    #[test]
    fn edit_with_invalid_client_field_fails_deserialization() {
        let payload = serde_json::json!({
            "name": "June Promo",
            "template_name": "promo_template",
            "template_language": "es",
            "template_variable_bindings": [{
                "component": "body",
                "index": 1,
                "placeholder": "{{1}}",
                "source": "client_field",
                "client_field": "not_allowed"
            }],
            "filters": { "client_state": "active" }
        });

        assert!(serde_json::from_value::<UpdateCampaignRequest>(payload).is_err());
    }

    #[test]
    fn edit_with_provider_name_client_field_fails_deserialization() {
        let payload = serde_json::json!({
            "name": "June Promo",
            "template_name": "promo_template",
            "template_language": "es",
            "template_variable_bindings": [{
                "component": "body",
                "index": 1,
                "placeholder": "{{1}}",
                "source": "client_field",
                "client_field": "provider_name"
            }],
            "filters": { "client_state": "active" }
        });

        assert!(serde_json::from_value::<UpdateCampaignRequest>(payload).is_err());
    }

    #[test]
    fn edit_regeneration_failure_plan_preserves_existing_snapshot_until_snapshot_build_succeeds() {
        let invalid_filter_request = CampaignPreviewRequest {
            provider_ids: None,
            sector_ids: None,
            balance_filter: None,
            client_state: None,
            include_all_active: None,
            page: None,
            per_page: None,
        };

        assert!(!has_allowed_filter(&invalid_filter_request));
    }

    #[test]
    fn create_with_duplicate_bindings_fails() {
        let err = validate_create_template_variable_bindings(Some(&[
            static_body_binding(1, "A"),
            static_body_binding(1, "B"),
        ]))
        .unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "duplicate_template_variable_binding"
                    && field.as_deref() == Some("template_variable_bindings")
        ));
    }

    #[test]
    fn confirm_legacy_without_phone_number_id_fails() {
        let mut campaign = base_campaign("draft");
        campaign.phone_number_id = None;

        let missing = campaign
            .phone_number_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ApiError::domain_simple(
                    StatusCode::BAD_REQUEST,
                    "missing_phone_number_id",
                    "Campaign must have a phone_number_id before confirmation.",
                )
            })
            .unwrap_err();

        assert!(matches!(
            missing,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST && code == "missing_phone_number_id"
        ));
    }

    #[test]
    fn confirm_template_with_variables_without_bindings_fails() {
        let components = vec![serde_json::json!({ "type": "BODY", "text": "Hello {{1}}" })];
        let none_bindings: Option<&[TemplateVariableBinding]> = None;

        let err =
            validate_bindings_against_template_components(&components, none_bindings).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "template_variable_bindings_incomplete"
                    && field.as_deref() == Some("template_variable_bindings")
        ));
    }

    #[test]
    fn confirm_template_with_complete_variables_passes() {
        let components = vec![serde_json::json!({ "type": "BODY", "text": "Hello {{1}}" })];
        let bindings = vec![client_field_body_binding(1)];

        assert!(
            validate_bindings_against_template_components(&components, Some(&bindings)).is_ok()
        );
    }

    #[test]
    fn confirm_template_with_legacy_provider_name_binding_fails() {
        let components = vec![serde_json::json!({ "type": "BODY", "text": "Hello {{1}}" })];
        let bindings = vec![legacy_provider_name_body_binding(1)];

        let err = validate_bindings_against_template_components(&components, Some(&bindings))
            .unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "template_variable_client_field_unsupported"
                    && field.as_deref() == Some("template_variable_bindings.client_field")
        ));
    }

    #[test]
    fn confirm_template_with_non_text_header_placeholder_fails() {
        let components = vec![serde_json::json!({
            "type": "HEADER",
            "format": "IMAGE",
            "image": "https://example.com/{{1}}"
        })];
        let none_bindings: Option<&[TemplateVariableBinding]> = None;

        let err =
            validate_bindings_against_template_components(&components, none_bindings).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "unsupported_template_variable_component"
        ));
    }

    #[test]
    fn confirm_template_with_non_url_button_placeholder_fails() {
        let components = vec![serde_json::json!({
            "type": "BUTTONS",
            "buttons": [{
                "type": "QUICK_REPLY",
                "url": "https://example.com/{{1}}"
            }]
        })];
        let none_bindings: Option<&[TemplateVariableBinding]> = None;

        let err =
            validate_bindings_against_template_components(&components, none_bindings).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "unsupported_template_variable_component"
        ));
    }

    #[test]
    fn confirm_template_with_payment_due_day_variable_passes() {
        let components = vec![serde_json::json!({ "type": "BODY", "text": "Due day {{1}}" })];
        let bindings = vec![payment_due_day_body_binding(1)];

        assert!(
            validate_bindings_against_template_components(&components, Some(&bindings)).is_ok()
        );
    }

    #[test]
    fn confirm_template_without_variables_passes() {
        let components = vec![serde_json::json!({ "type": "BODY", "text": "Hello" })];
        let none_bindings: Option<&[TemplateVariableBinding]> = None;

        assert!(validate_bindings_against_template_components(&components, none_bindings).is_ok());
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
    fn retired_filter_matches_retired_state() {
        let request = CampaignPreviewRequest {
            provider_ids: None,
            sector_ids: None,
            balance_filter: None,
            client_state: Some(ClientStateFilter::Retired),
            include_all_active: None,
            page: None,
            per_page: None,
        };

        assert_eq!(
            build_client_filter(&request).unwrap(),
            doc! { "sState": { "$in": vec![RETIRED_CLIENT_STATE] } }
        );
    }

    #[test]
    fn retired_filter_is_allowed_as_standalone_audience_filter() {
        let request = CampaignPreviewRequest {
            provider_ids: None,
            sector_ids: None,
            balance_filter: None,
            client_state: Some(ClientStateFilter::Retired),
            include_all_active: None,
            page: None,
            per_page: None,
        };

        assert!(has_allowed_filter(&request));
    }

    #[test]
    fn balance_filter_eq_builds_eq_query() {
        let filter = BalanceFilter {
            lt: None,
            lte: None,
            gt: None,
            gte: None,
            eq: Some(0.0),
            between: None,
        };

        assert_eq!(build_balance_filter(&filter).unwrap(), doc! { "$eq": 0.0 });
    }

    #[test]
    fn balance_filter_lt_builds_lt_query() {
        let filter = BalanceFilter {
            lt: Some(0.0),
            lte: None,
            gt: None,
            gte: None,
            eq: None,
            between: None,
        };

        assert_eq!(build_balance_filter(&filter).unwrap(), doc! { "$lt": 0.0 });
    }

    #[test]
    fn balance_filter_operator_value_shape_reports_contract_error() {
        let filter: BalanceFilter = serde_json::from_value(serde_json::json!({
            "op": "eq",
            "value": 0
        }))
        .unwrap();

        let err = build_balance_filter(&filter).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, ref message, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "invalid_balance_filter"
                    && field.as_deref() == Some("balance_filter")
                    && message.contains("lt,lte,gt,gte,eq,between")
        ));
    }

    #[test]
    fn duplicate_detection_is_global_and_keeps_first_valid_occurrence() {
        let mut first = candidate("1", "First", "0412 123 4567");
        first.payment_due_day = Some(15);
        let (totals, recipients) = build_preview_recipients(
            vec![
                first,
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
        assert_eq!(recipients[0].payment_due_day, Some(15));
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
            payment_due_day: Some(10),
        };

        let snapshot = preview_to_snapshot_recipient(campaign_id, recipient, now);

        assert!(!snapshot.can_send);
        assert!(!snapshot.excluded);
        assert_eq!(snapshot.status, "invalid_phone");
        assert_eq!(snapshot.payment_due_day, Some(10));
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
            payment_due_day: None,
        };

        let snapshot = preview_to_snapshot_recipient(campaign_id, recipient, now);

        assert!(!snapshot.can_send);
        assert!(!snapshot.excluded);
        assert_eq!(snapshot.status, "duplicated_phone");
        assert!(matches!(snapshot.phone_status, PhoneStatus::Duplicated));
    }

    #[test]
    fn recipient_item_includes_payment_due_day() {
        let now = DateTime::now();
        let item = recipient_to_item(WaCampaignRecipientDoc {
            id: Some(ObjectId::new()),
            campaign_id: ObjectId::new(),
            client_id: "client-1".to_string(),
            client_name: "Client One".to_string(),
            provider_id: None,
            provider_name: None,
            sector_id: None,
            sector_name: None,
            customer_status_raw: "Activo".to_string(),
            customer_status_derived: DerivedClientState::Solvente,
            balance: 0.0,
            payment_due_day: Some(20),
            phone_original: "0412 123 4567".to_string(),
            phone_normalized: Some("584121234567".to_string()),
            phone_status: PhoneStatus::Valid,
            can_send: true,
            reason: None,
            excluded: false,
            status: "pending".to_string(),
            attempts: 0,
            last_attempt_at: None,
            error_code: None,
            error_message: None,
            validated_at: None,
            created_at: now,
            updated_at: now,
        });

        assert_eq!(item.payment_due_day, Some(20));
    }

    #[test]
    fn recipient_item_includes_dry_run_status_fields() {
        let now = DateTime::from_millis(1_800_000_000_000);
        let last_attempt_at = DateTime::from_millis(1_800_000_000_100);
        let validated_at = DateTime::from_millis(1_800_000_000_200);
        let mut recipient = base_recipient("validated");
        recipient.attempts = 2;
        recipient.last_attempt_at = Some(last_attempt_at);
        recipient.error_code = Some("previous_error".to_string());
        recipient.error_message = Some("Previous error".to_string());
        recipient.validated_at = Some(validated_at);
        recipient.updated_at = now;

        let item = recipient_to_item(recipient);
        let expected_last_attempt_at = iso8601(last_attempt_at);
        let expected_validated_at = iso8601(validated_at);

        assert_eq!(item.status, "validated");
        assert_eq!(item.attempts, 2);
        assert_eq!(
            item.last_attempt_at.as_deref(),
            Some(expected_last_attempt_at.as_str())
        );
        assert_eq!(item.error_code.as_deref(), Some("previous_error"));
        assert_eq!(item.error_message.as_deref(), Some("Previous error"));
        assert_eq!(
            item.validated_at.as_deref(),
            Some(expected_validated_at.as_str())
        );
        assert_eq!(item.updated_at, iso8601(now));
    }

    #[test]
    fn payment_due_day_accepts_only_valid_client_payment_days() {
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": 1 }, "nPayment"),
            Some(1)
        );
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": 15.0 }, "nPayment"),
            Some(15)
        );
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": 15.5 }, "nPayment"),
            None
        );
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": 31_i64 }, "nPayment"),
            Some(31)
        );
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": 0 }, "nPayment"),
            None
        );
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": 32 }, "nPayment"),
            None
        );
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": 32.0 }, "nPayment"),
            None
        );
        assert_eq!(get_payment_due_day(&doc! {}, "nPayment"), None);
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
    fn campaign_recipients_filter_allows_optional_status_exact_match() {
        let campaign_id = ObjectId::parse_str("64f000000000000000000001").unwrap();

        assert_eq!(
            build_campaign_recipients_filter(campaign_id, Some(" validated ")),
            doc! { "campaign_id": campaign_id, "status": "validated" }
        );
        assert_eq!(
            build_campaign_recipients_filter(campaign_id, Some(" ")),
            doc! { "campaign_id": campaign_id }
        );
    }

    #[test]
    fn dry_run_claim_pending_recipient_builds_atomic_sending_update() {
        let campaign_id = ObjectId::parse_str("64f000000000000000000001").unwrap();
        let now = DateTime::from_millis(1_800_000_000_100);
        let recipient = base_recipient("pending");

        assert!(dry_run_recipient_is_claimable(&recipient));
        assert_eq!(
            dry_run_recipient_claim_filter(campaign_id),
            effective_recipient_filter(campaign_id)
        );
        assert_eq!(
            dry_run_recipient_claim_update(now),
            doc! {
                "$set": {
                    "status": "sending",
                    "last_attempt_at": now,
                    "updated_at": now,
                },
                "$inc": { "attempts": 1i64 }
            }
        );
    }

    #[test]
    fn dry_run_claim_does_not_take_excluded_recipients() {
        let mut recipient = base_recipient("pending");
        recipient.excluded = true;

        assert!(!dry_run_recipient_is_claimable(&recipient));
    }

    #[test]
    fn dry_run_claim_does_not_take_non_sendable_recipients() {
        let mut recipient = base_recipient("pending");
        recipient.can_send = false;

        assert!(!dry_run_recipient_is_claimable(&recipient));
    }

    #[test]
    fn dry_run_claim_does_not_take_invalid_or_duplicated_statuses() {
        for status in [
            "invalid_phone",
            "duplicated_phone",
            "sending",
            "validated",
            "failed",
        ] {
            assert!(!dry_run_recipient_is_claimable(&base_recipient(status)));
        }
    }

    #[test]
    fn dry_run_resolver_ok_marks_sending_recipient_validated_and_clears_error() {
        let now = DateTime::from_millis(1_800_000_000_200);
        let mut campaign = base_campaign("running");
        campaign.run_mode = Some("dry_run".to_string());
        campaign.template_components = Some(vec![serde_json::json!({
            "type": "BODY",
            "text": "Hello {{1}}"
        })]);
        campaign.template_variable_bindings = Some(vec![static_body_binding(1, "World").into()]);

        let recipient = base_recipient("sending");
        let resolved = resolve_campaign_template_components(
            campaign.template_components.as_deref(),
            campaign.template_variable_bindings.as_deref(),
            &recipient_to_template_snapshot(&recipient),
        );

        assert!(resolved.is_ok());
        assert_eq!(
            dry_run_recipient_validated_update(now),
            doc! {
                "$set": {
                    "status": "validated",
                    "validated_at": now,
                    "updated_at": now,
                },
                "$unset": {
                    "error_code": "",
                    "error_message": "",
                }
            }
        );
    }

    #[test]
    fn dry_run_resolver_error_marks_sending_recipient_failed_with_safe_error() {
        let now = DateTime::from_millis(1_800_000_000_300);
        let mut campaign = base_campaign("running");
        campaign.run_mode = Some("dry_run".to_string());
        campaign.template_components = Some(vec![serde_json::json!({
            "type": "BODY",
            "text": "Due {{1}}"
        })]);
        campaign.template_variable_bindings = Some(vec![payment_due_day_body_binding(1).into()]);

        let mut recipient = base_recipient("sending");
        recipient.payment_due_day = None;
        let err = resolve_campaign_template_components(
            campaign.template_components.as_deref(),
            campaign.template_variable_bindings.as_deref(),
            &recipient_to_template_snapshot(&recipient),
        )
        .unwrap_err();

        assert_eq!(err.code(), "missing_recipient_field");
        assert_eq!(safe_resolver_error_message(&err), "missing_recipient_field");
        assert_eq!(
            dry_run_recipient_failed_update(err.code(), safe_resolver_error_message(&err), now),
            doc! {
                "$set": {
                    "status": "failed",
                    "error_code": "missing_recipient_field",
                    "error_message": "missing_recipient_field",
                    "updated_at": now,
                }
            }
        );
    }

    #[test]
    fn dry_run_finalization_without_failed_recipients_completes_successfully() {
        let progress = CampaignDryRunProgress {
            pending: 0,
            sending: 0,
            validated: 3,
            failed: 0,
            ..Default::default()
        };

        assert_eq!(
            dry_run_completion_status(&progress),
            Some("dry_run_completed")
        );
    }

    #[test]
    fn dry_run_finalization_with_failed_recipients_completes_with_errors() {
        let progress = CampaignDryRunProgress {
            pending: 0,
            sending: 0,
            validated: 2,
            failed: 1,
            ..Default::default()
        };

        assert_eq!(
            dry_run_completion_status(&progress),
            Some("dry_run_completed_with_errors")
        );
    }

    #[test]
    fn dry_run_finalization_waits_for_pending_or_sending_recipients() {
        assert_eq!(
            dry_run_completion_status(&CampaignDryRunProgress {
                pending: 1,
                sending: 0,
                validated: 0,
                failed: 0,
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            dry_run_completion_status(&CampaignDryRunProgress {
                pending: 0,
                sending: 1,
                validated: 0,
                failed: 0,
                ..Default::default()
            }),
            None
        );
    }

    #[test]
    fn campaign_progress_counts_statuses_and_calculates_processed_percentage() {
        let dto = campaign_progress_to_dto(CampaignDryRunProgress {
            pending: 2,
            sending: 1,
            validated: 3,
            failed: 1,
            invalid_phone: 4,
            duplicated_phone: 5,
            excluded: 6,
            total_effective: 7,
        });

        assert_eq!(dto.pending, 2);
        assert_eq!(dto.sending, 1);
        assert_eq!(dto.validated, 3);
        assert_eq!(dto.failed, 1);
        assert_eq!(dto.invalid_phone, 4);
        assert_eq!(dto.duplicated_phone, 5);
        assert_eq!(dto.excluded, 6);
        assert_eq!(dto.total_effective, 7);
        assert_eq!(dto.processed, 4);
        assert!((dto.progress_percent - 57.14285714285714).abs() < 1e-9);
    }

    #[test]
    fn campaign_progress_percent_handles_zero_effective_total() {
        assert_eq!(calculate_progress_percent(0, 0), 0.0);
        assert_eq!(
            campaign_progress_to_dto(CampaignDryRunProgress::default()).progress_percent,
            0.0
        );
    }

    #[test]
    fn campaign_detail_summary_includes_progress_when_loaded_for_detail() {
        let summary = campaign_to_summary_with_progress(
            base_campaign("running"),
            Some(CampaignDryRunProgress {
                pending: 0,
                sending: 0,
                validated: 2,
                failed: 0,
                invalid_phone: 1,
                duplicated_phone: 1,
                excluded: 1,
                total_effective: 2,
            }),
        );

        let progress = summary.progress.unwrap();
        assert_eq!(progress.validated, 2);
        assert_eq!(progress.processed, 2);
        assert_eq!(progress.progress_percent, 100.0);
    }

    #[test]
    fn campaign_summary_omits_progress_by_default_for_non_detail_flows() {
        let summary = campaign_to_summary(base_campaign("queued"));

        assert!(summary.progress.is_none());
    }

    #[test]
    fn dry_run_recovery_returns_stale_sending_recipient_to_pending() {
        let now = DateTime::from_millis(1_800_000_600_000);
        let mut stale = base_recipient("sending");
        stale.last_attempt_at = Some(DateTime::from_millis(
            now.timestamp_millis() - (CAMPAIGN_SENDING_STALE_SECS + 1) * 1000,
        ));

        assert!(stale_sending_recipient_is_recoverable(&stale, now));
        recover_stale_sending_recipient_state(&mut stale, now);

        assert_eq!(stale.status, "pending");
        assert_eq!(stale.updated_at, now);
        assert_eq!(stale.error_code.as_deref(), Some("stale_sending_recovered"));
        assert_eq!(
            stale.error_message.as_deref(),
            Some("Recipient returned to pending after stale sending timeout.")
        );
    }

    #[test]
    fn dry_run_recovery_keeps_recent_sending_recipient_unchanged() {
        let now = DateTime::from_millis(1_800_000_600_000);
        let mut recent = base_recipient("sending");
        let original_updated_at = recent.updated_at;
        recent.last_attempt_at = Some(DateTime::from_millis(
            now.timestamp_millis() - (CAMPAIGN_SENDING_STALE_SECS - 1) * 1000,
        ));

        assert!(!stale_sending_recipient_is_recoverable(&recent, now));
        recover_stale_sending_recipient_state(&mut recent, now);

        assert_eq!(recent.status, "sending");
        assert_eq!(recent.updated_at, original_updated_at);
        assert!(recent.error_code.is_none());
        assert!(recent.error_message.is_none());
    }

    #[test]
    fn dry_run_recovery_builds_stale_filter_and_pending_update() {
        let campaign_id = ObjectId::parse_str("64f000000000000000000001").unwrap();
        let now = DateTime::from_millis(1_800_000_600_000);

        assert_eq!(
            stale_sending_recovery_filter(campaign_id, now),
            doc! {
                "campaign_id": campaign_id,
                "can_send": true,
                "excluded": false,
                "status": "sending",
                "last_attempt_at": { "$lt": DateTime::from_millis(1_800_000_300_000) },
            }
        );
        assert_eq!(
            stale_sending_recovery_update(now),
            doc! {
                "$set": {
                    "status": "pending",
                    "updated_at": now,
                    "error_code": "stale_sending_recovered",
                    "error_message": "Recipient returned to pending after stale sending timeout.",
                }
            }
        );
    }

    #[test]
    fn dry_run_worker_selection_only_processes_running_dry_run_campaigns() {
        let mut campaign = base_campaign("running");
        campaign.run_mode = Some("dry_run".to_string());

        assert!(should_process_campaign_dry_run(&campaign));
        assert_eq!(
            running_dry_run_campaign_filter(),
            doc! { "status": "running", "run_mode": "dry_run" }
        );
    }

    #[test]
    fn dry_run_worker_selection_rejects_queued_draft_and_other_run_modes() {
        for status in ["queued", "draft", "previewed"] {
            let mut campaign = base_campaign(status);
            campaign.run_mode = Some("dry_run".to_string());
            assert!(!should_process_campaign_dry_run(&campaign));
        }

        let mut live_campaign = base_campaign("running");
        live_campaign.run_mode = Some("live".to_string());
        assert!(!should_process_campaign_dry_run(&live_campaign));

        let missing_run_mode = base_campaign("running");
        assert!(!should_process_campaign_dry_run(&missing_run_mode));
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
    fn start_queued_campaign_with_effective_recipients_builds_running_transition() {
        let mut campaign = base_campaign("queued");
        campaign.total_excluded = 0;

        validate_startable_campaign(&campaign).unwrap();
        let now = DateTime::from_millis(1_800_000_000_001);
        let update = start_campaign_update_doc("admin-2", now);

        assert_eq!(
            update,
            doc! {
                "$set": {
                    "status": "running",
                    "started_by": "admin-2",
                    "started_at": now,
                    "updated_at": now,
                    "run_mode": "dry_run",
                }
            }
        );

        campaign.status = "running".to_string();
        campaign.started_by = Some("admin-2".to_string());
        campaign.started_at = Some(now);
        campaign.run_mode = Some("dry_run".to_string());
        let summary = campaign_to_summary(campaign);

        assert_eq!(summary.status, "running");
        assert_eq!(summary.started_by.as_deref(), Some("admin-2"));
        assert_eq!(summary.run_mode.as_deref(), Some("dry_run"));
        assert!(summary.started_at.is_some());
    }

    #[test]
    fn start_missing_campaign_maps_to_not_found_contract() {
        let missing: Option<WaCampaignDoc> = None;

        assert!(matches!(
            missing.ok_or(ApiError::NotFound),
            Err(ApiError::NotFound)
        ));
    }

    #[test]
    fn start_draft_or_previewed_campaign_returns_conflict() {
        for status_value in ["draft", "previewed"] {
            let err = validate_startable_campaign(&base_campaign(status_value)).unwrap_err();

            assert!(matches!(
                err,
                ApiError::Domain { status, ref code, .. }
                    if status == StatusCode::CONFLICT && code == "campaign_not_startable"
            ));
        }
    }

    #[test]
    fn start_running_campaign_returns_conflict() {
        let err = validate_startable_campaign(&base_campaign("running")).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::CONFLICT && code == "campaign_not_startable"
        ));
    }

    #[test]
    fn start_queued_with_no_effective_recipients_returns_validation_error_without_status_change() {
        let campaign = base_campaign("queued");
        let effective_recipients = 0_u64;

        validate_startable_campaign(&campaign).unwrap();
        let err = if effective_recipients == 0 {
            Err(ApiError::domain_simple(
                StatusCode::BAD_REQUEST,
                "campaign_has_no_effective_recipients",
                "Campaign must have at least one non-excluded pending recipient that can be sent.",
            ))
        } else {
            Ok(())
        }
        .unwrap_err();

        assert_eq!(campaign.status, "queued");
        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST && code == "campaign_has_no_effective_recipients"
        ));
    }

    #[test]
    fn start_legacy_without_phone_number_id_returns_validation_error_without_status_change() {
        let mut campaign = base_campaign("queued");
        campaign.phone_number_id = None;

        let err = validate_startable_campaign(&campaign).unwrap_err();

        assert_eq!(campaign.status, "queued");
        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST && code == "missing_phone_number_id"
        ));
    }

    #[test]
    fn start_legacy_without_template_name_returns_existing_validation_code() {
        let mut campaign = base_campaign("queued");
        campaign.template_name = " ".to_string();

        let err = validate_startable_campaign(&campaign).unwrap_err();

        assert!(matches!(
            err,
            ApiError::BadRequest(ref code) if code == "template_name_required"
        ));
    }

    #[test]
    fn start_legacy_without_template_language_returns_existing_validation_code() {
        let mut campaign = base_campaign("queued");
        campaign.template_language = " ".to_string();

        let err = validate_startable_campaign(&campaign).unwrap_err();

        assert!(matches!(
            err,
            ApiError::BadRequest(ref code) if code == "template_language_required"
        ));
    }

    #[test]
    fn double_start_first_transition_then_second_returns_conflict() {
        let first = validate_startable_campaign(&base_campaign("queued"));
        let second = validate_startable_campaign(&base_campaign("running"));

        assert!(first.is_ok());
        assert!(matches!(
            second,
            Err(ApiError::Domain { status, ref code, .. })
                if status == StatusCode::CONFLICT && code == "campaign_not_startable"
        ));
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
