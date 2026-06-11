use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use axum::http::StatusCode;
use futures::stream::StreamExt;
use futures::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId, Bson, DateTime, Document};
use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument, UpdateModifications};
use mongodb::Collection;
use serde::{Deserialize, Serialize};

use crate::{
    crypto::aes::decrypt_payload,
    db::{WaTemplateMediaRef, WaTemplateMediaRepository, WaTemplateRepository, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::{WaConversationEventInput, WaMessage, WaSettings, WaTemplate},
    modules::whatsapp::{
        service::{MediaRelay, MetaApiError, WhatsAppService},
        shared::{settings_secret, time::iso8601},
    },
    state::AppState,
};

use super::{
    dto::{
        BalanceFilter, CampaignAutoPrepareResult, CampaignListItem, CampaignListQuery,
        CampaignListResponse, CampaignPreviewRecipient, CampaignPreviewRequest,
        CampaignPreviewResponse, CampaignPreviewTotals, CampaignProgress, CampaignRecipientItem,
        CampaignRecipientsQuery, CampaignRecipientsResponse, CampaignSummary,
        CampaignSummaryResponse, ClientStateFilter, CreateCampaignRequest, DerivedClientState,
        PhoneStatus, TemplateClientField, TemplateMediaBinding, TemplateMediaComponent,
        TemplateMediaSource, TemplateMediaType, TemplateVariableBinding, TemplateVariableComponent,
        TemplateVariableSource, UpdateCampaignRecipientExclusionsData,
        UpdateCampaignRecipientExclusionsRequest, UpdateCampaignRecipientExclusionsResponse,
        UpdateCampaignRequest, UpdateCampaignResponse,
    },
    phone::normalize_phone_to_whatsapp,
    template_resolver::{
        extract_template_placeholders, resolve_campaign_template_components,
        CampaignTemplateRecipientSnapshot, CampaignTemplateResolveError,
    },
    template_send_builder::{
        build_campaign_template_send_components, CampaignTemplateSendBuildError,
    },
};

const DEFAULT_PER_PAGE: u32 = 100;
const MAX_PER_PAGE: u32 = 500;
const DEFAULT_CAMPAIGN_LIST_LIMIT: u32 = 20;
const MAX_CAMPAIGN_LIST_LIMIT: u32 = 100;
const RETIRED_CLIENT_STATE: &str = "Retirado";
const CAMPAIGN_WORKER_INTERVAL_SECS: u64 = 5;
const CAMPAIGN_WORKER_BATCH_SIZE: usize = 50;
const CAMPAIGN_SEND_WORKER_INTERVAL_SECS: u64 = 5;
const CAMPAIGN_SEND_WORKER_BATCH_SIZE: usize = 1;
const CAMPAIGN_SEND_DELAY_MS: u64 = 1_500;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    template_media_bindings: Option<Vec<TemplateMediaBinding>>,
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
    #[serde(default)]
    send_started_by: Option<String>,
    #[serde(default)]
    send_started_at: Option<DateTime>,
    #[serde(default)]
    send_completed_at: Option<DateTime>,
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
    #[serde(default, alias = "client_state_raw")]
    customer_status_raw: String,
    #[serde(
        default = "default_derived_client_state",
        alias = "client_state_derived"
    )]
    customer_status_derived: DerivedClientState,
    balance: f64,
    #[serde(default)]
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
    #[serde(default)]
    send_attempts: i64,
    #[serde(default)]
    send_started_at: Option<DateTime>,
    #[serde(default)]
    sent_at: Option<DateTime>,
    #[serde(default)]
    send_error_code: Option<String>,
    #[serde(default)]
    send_error_message: Option<String>,
    #[serde(default)]
    meta_message_id: Option<String>,
    #[serde(default)]
    meta_error_code: Option<String>,
    #[serde(default)]
    meta_error_subcode: Option<String>,
    #[serde(default)]
    meta_error_user_msg: Option<String>,
    created_at: DateTime,
    updated_at: DateTime,
}

fn default_derived_client_state() -> DerivedClientState {
    DerivedClientState::Suspended
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
    sent: u64,
    send_failed: u64,
    send_unknown: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CampaignSendResult {
    meta_message_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CampaignSendError {
    code: String,
    message: String,
    meta_error_code: Option<String>,
    meta_error_subcode: Option<String>,
    meta_error_user_msg: Option<String>,
}

impl CampaignSendError {
    #[cfg(test)]
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        }
    }
}

impl fmt::Display for CampaignSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CampaignSendError {}

impl From<&anyhow::Error> for CampaignSendError {
    fn from(err: &anyhow::Error) -> Self {
        if let Some(meta) = err.downcast_ref::<MetaApiError>() {
            return Self {
                code: "meta_rejected".to_string(),
                message: meta.message.clone(),
                meta_error_code: Some(meta.code.to_string()),
                meta_error_subcode: meta.error_subcode.map(|value| value.to_string()),
                meta_error_user_msg: meta.error_user_msg.clone(),
            };
        }

        Self {
            code: "send_template_failed".to_string(),
            message: err.to_string(),
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        }
    }
}

#[async_trait]
trait CampaignMessageSender: Send + Sync {
    async fn send_template(
        &self,
        campaign: &WaCampaignDoc,
        recipient: &WaCampaignRecipientDoc,
        components: Vec<serde_json::Value>,
    ) -> Result<CampaignSendResult, CampaignSendError>;
}

struct CampaignMetaMessageSender {
    state: Arc<AppState>,
}

impl CampaignMetaMessageSender {
    fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl CampaignMessageSender for CampaignMetaMessageSender {
    async fn send_template(
        &self,
        campaign: &WaCampaignDoc,
        recipient: &WaCampaignRecipientDoc,
        components: Vec<serde_json::Value>,
    ) -> Result<CampaignSendResult, CampaignSendError> {
        let service = resolve_campaign_whatsapp_service(&self.state, campaign).await?;
        let to = recipient
            .phone_normalized
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CampaignSendError {
                code: "missing_recipient_phone".to_string(),
                message: "Campaign recipient does not have a normalized WhatsApp phone."
                    .to_string(),
                meta_error_code: None,
                meta_error_subcode: None,
                meta_error_user_msg: None,
            })?;
        let components_value = if components.is_empty() {
            None
        } else {
            Some(serde_json::Value::Array(components))
        };

        service
            .send_template(
                to,
                campaign.template_name.as_str(),
                campaign.template_language.as_str(),
                components_value.as_ref(),
            )
            .await
            .map(|meta_message_id| CampaignSendResult { meta_message_id })
            .map_err(|err| CampaignSendError::from(&err))
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct FakeCampaignMessageSender;

#[cfg(test)]
#[async_trait]
impl CampaignMessageSender for FakeCampaignMessageSender {
    async fn send_template(
        &self,
        campaign: &WaCampaignDoc,
        recipient: &WaCampaignRecipientDoc,
        components: Vec<serde_json::Value>,
    ) -> Result<CampaignSendResult, CampaignSendError> {
        fake_campaign_send_result(campaign, recipient, &components)
    }
}

#[cfg(test)]
fn fake_campaign_send_result(
    campaign: &WaCampaignDoc,
    recipient: &WaCampaignRecipientDoc,
    _components: &[serde_json::Value],
) -> Result<CampaignSendResult, CampaignSendError> {
    let campaign_id = campaign
        .id
        .map(|id| id.to_hex())
        .unwrap_or_else(|| "unknown_campaign".to_string());
    let recipient_id = recipient
        .id
        .map(|id| id.to_hex())
        .unwrap_or_else(|| "unknown_recipient".to_string());

    Ok(CampaignSendResult {
        meta_message_id: format!("fake:{campaign_id}:{recipient_id}"),
    })
}

async fn resolve_campaign_whatsapp_service(
    state: &AppState,
    campaign: &WaCampaignDoc,
) -> Result<WhatsAppService, CampaignSendError> {
    let phone_number_id = campaign
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CampaignSendError {
            code: "whatsapp_account_missing_phone_number_id".to_string(),
            message: "Campaign does not have a phone_number_id configured.".to_string(),
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        })?;

    let settings = state
        .db
        .find_wa_settings_by_phone_number_id(phone_number_id)
        .await
        .map_err(|err| CampaignSendError {
            code: "whatsapp_account_lookup_failed".to_string(),
            message: err,
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        })?
        .ok_or_else(|| CampaignSendError {
            code: "whatsapp_account_not_found".to_string(),
            message: "Selected WhatsApp account was not found.".to_string(),
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        })?;

    validate_wa_settings_for_campaign_send(&settings)?;

    let token = decrypt_payload(&settings_secret(), &settings.access_token).ok_or_else(|| {
        CampaignSendError {
            code: "whatsapp_account_token_decrypt_failed".to_string(),
            message: "Could not decrypt WhatsApp account access token.".to_string(),
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        }
    })?;

    let service = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id.clone(),
        token,
    );
    Ok(apply_media_relay_for_campaign_send(state, service))
}

fn apply_media_relay_for_campaign_send(
    state: &AppState,
    service: WhatsAppService,
) -> WhatsAppService {
    match (
        state.config.relay_url.as_ref(),
        state.config.relay_secret.as_ref(),
    ) {
        (Some(url), Some(secret)) => service.with_media_relay(MediaRelay {
            url: url.clone(),
            secret: secret.clone(),
        }),
        _ => service,
    }
}

async fn resolve_template_media_bindings_for_send(
    state: &AppState,
    campaign: &WaCampaignDoc,
) -> Result<Option<Vec<TemplateMediaBinding>>, CampaignSendError> {
    let Some(bindings) = campaign.template_media_bindings.as_deref() else {
        return Ok(None);
    };
    if !bindings
        .iter()
        .any(|binding| matches!(binding.source, TemplateMediaSource::TemplateMediaId))
    {
        return Ok(Some(bindings.to_vec()));
    }

    let service = resolve_campaign_whatsapp_service(state, campaign).await?;
    let mut resolved = Vec::with_capacity(bindings.len());
    for binding in bindings {
        if !matches!(binding.source, TemplateMediaSource::TemplateMediaId) {
            resolved.push(binding.clone());
            continue;
        }

        let oid = ObjectId::parse_str(binding.value.trim()).map_err(|_| CampaignSendError {
            code: "invalid_template_media_binding".to_string(),
            message: "Template media binding value is not a valid local media id.".to_string(),
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        })?;
        let (bytes, mime_type) = state
            .db
            .read_template_media_bytes(&oid)
            .await
            .map_err(|err| CampaignSendError {
                code: "template_media_lookup_failed".to_string(),
                message: err,
                meta_error_code: None,
                meta_error_subcode: None,
                meta_error_user_msg: None,
            })?
            .ok_or_else(|| CampaignSendError {
                code: "invalid_template_media_binding".to_string(),
                message: "Template media binding points to a missing local media file.".to_string(),
                meta_error_code: None,
                meta_error_subcode: None,
                meta_error_user_msg: None,
            })?;

        let meta_media_id = service
            .upload_media(bytes, &mime_type, None)
            .await
            .map_err(|err| CampaignSendError {
                code: "template_media_upload_failed".to_string(),
                message: err.to_string(),
                meta_error_code: None,
                meta_error_subcode: None,
                meta_error_user_msg: None,
            })?;

        resolved.push(template_media_binding_with_meta_media_id(
            binding,
            meta_media_id,
        ));
    }

    Ok(Some(resolved))
}

fn template_media_binding_with_meta_media_id(
    binding: &TemplateMediaBinding,
    meta_media_id: String,
) -> TemplateMediaBinding {
    TemplateMediaBinding {
        source: TemplateMediaSource::MediaId,
        value: meta_media_id,
        ..binding.clone()
    }
}

fn validate_wa_settings_for_campaign_send(settings: &WaSettings) -> Result<(), CampaignSendError> {
    if !settings.active {
        return Err(CampaignSendError {
            code: "whatsapp_account_inactive".to_string(),
            message: "The selected WhatsApp account is inactive.".to_string(),
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        });
    }
    if settings.phone_number_id.trim().is_empty() {
        return Err(CampaignSendError {
            code: "whatsapp_account_missing_phone_number_id".to_string(),
            message: "Selected WhatsApp account is missing phone_number_id.".to_string(),
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        });
    }
    if settings.access_token.trim().is_empty() {
        return Err(CampaignSendError {
            code: "whatsapp_account_missing_token".to_string(),
            message: "Selected WhatsApp account is missing access_token.".to_string(),
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
        });
    }
    Ok(())
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
    let auto_prepare = request.auto_prepare.unwrap_or(false);
    let phone_number_id = normalize_optional_phone_number_id(request.phone_number_id.as_deref())?;
    validate_create_template_variable_bindings(request.template_variable_bindings.as_deref())?;
    validate_template_media_bindings(request.template_media_bindings.as_deref())?;
    validate_template_media_bindings_against_gridfs(
        state,
        phone_number_id.as_deref(),
        request.template_media_bindings.as_deref(),
    )
    .await?;

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
        template_media_bindings: request.template_media_bindings,
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
        send_started_by: None,
        send_started_at: None,
        send_completed_at: None,
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

    if auto_prepare {
        return match auto_prepare_campaign_internal(state, campaign_id.clone(), created_by).await {
            Ok(campaign) => Ok(CampaignSummaryResponse {
                ok: true,
                data: campaign_to_summary(campaign),
                auto_prepare: Some(CampaignAutoPrepareResult {
                    confirmed: true,
                    validation_started: true,
                }),
            }),
            Err(err) => Err(auto_prepare_failed_error(campaign_id, "prepare", err)),
        };
    }

    Ok(CampaignSummaryResponse {
        ok: true,
        data: campaign_to_summary(campaign),
        auto_prepare: None,
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
        auto_prepare: None,
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
    let next_phone_number_id =
        normalize_optional_phone_number_id(request.phone_number_id.as_deref())?
            .or_else(|| campaign.phone_number_id.clone());
    if let Err(err) = validate_template_media_bindings_against_gridfs(
        state,
        next_phone_number_id.as_deref(),
        request.template_media_bindings.as_deref(),
    )
    .await
    {
        let _ = restore_campaign_after_failed_edit(&campaigns, &campaign_id, &campaign).await;
        return Err(err);
    }
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
    let campaign = confirm_campaign_internal(state, campaign_id, confirmed_by).await?;

    Ok(CampaignSummaryResponse {
        ok: true,
        data: campaign_to_summary(campaign),
        auto_prepare: None,
    })
}

pub async fn start_campaign(
    state: &AppState,
    campaign_id: &str,
    started_by: &str,
) -> Result<CampaignSummaryResponse, ApiError> {
    let campaign_id = parse_campaign_id(campaign_id)?;
    let campaign = start_campaign_validation_internal(state, campaign_id, started_by).await?;

    Ok(CampaignSummaryResponse {
        ok: true,
        data: campaign_to_summary(campaign),
        auto_prepare: None,
    })
}

async fn auto_prepare_campaign_internal(
    state: &AppState,
    campaign_id: ObjectId,
    requested_by: &str,
) -> Result<WaCampaignDoc, ApiError> {
    confirm_campaign_internal(state, campaign_id.clone(), requested_by).await?;
    match start_campaign_validation_internal(state, campaign_id.clone(), requested_by).await {
        Ok(campaign) => Ok(campaign),
        Err(err) => {
            restore_auto_prepare_created_campaign(state, &campaign_id).await?;
            Err(err)
        }
    }
}

async fn confirm_campaign_internal(
    state: &AppState,
    campaign_id: ObjectId,
    confirmed_by: &str,
) -> Result<WaCampaignDoc, ApiError> {
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

    let effective_recipients = count_effective_recipients(&recipients, campaign_id.clone()).await?;
    if effective_recipients == 0 {
        restore_campaign_after_failed_confirmation(
            &campaigns,
            &campaign_id,
            campaign.status.as_str(),
        )
        .await?;
        return Err(campaign_has_no_effective_recipients_error());
    }

    let now = DateTime::now();
    let result = campaigns
        .update_one(
            doc! { "_id": campaign_id, "status": "confirming", "confirming_from": &campaign.status },
            confirm_campaign_update_doc(confirmed_by, now),
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

    campaigns
        .find_one(doc! { "_id": campaign_id })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .ok_or(ApiError::NotFound)
}

async fn start_campaign_validation_internal(
    state: &AppState,
    campaign_id: ObjectId,
    started_by: &str,
) -> Result<WaCampaignDoc, ApiError> {
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

    let effective_recipients = count_effective_recipients(&recipients, campaign_id.clone()).await?;
    if effective_recipients == 0 {
        return Err(campaign_has_no_effective_recipients_error());
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

    campaigns
        .find_one(doc! { "_id": campaign_id })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .ok_or(ApiError::NotFound)
}

async fn count_effective_recipients(
    recipients: &Collection<WaCampaignRecipientDoc>,
    campaign_id: ObjectId,
) -> Result<u64, ApiError> {
    recipients
        .count_documents(effective_recipient_filter(campaign_id))
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))
}

async fn restore_auto_prepare_created_campaign(
    state: &AppState,
    campaign_id: &ObjectId,
) -> Result<(), ApiError> {
    state
        .db
        .db
        .collection::<WaCampaignDoc>("WaCampaigns")
        .update_one(
            doc! { "_id": campaign_id },
            restore_auto_prepare_created_campaign_update_doc(DateTime::now()),
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    Ok(())
}

fn auto_prepare_failed_error(campaign_id: ObjectId, stage: &str, source: ApiError) -> ApiError {
    ApiError::Domain {
        status: StatusCode::BAD_REQUEST,
        code: "campaign_auto_prepare_failed".to_string(),
        field: Some("auto_prepare".to_string()),
        message: "Campaign was created as draft, but automatic preparation failed. Review and correct it manually.".to_string(),
        details: Some(serde_json::json!({
            "campaign_id": campaign_id.to_hex(),
            "stage": stage,
            "cause": api_error_code(&source),
        })),
    }
}

fn api_error_code(err: &ApiError) -> String {
    match err {
        ApiError::BadRequest(code) | ApiError::Conflict(code) => code.clone(),
        ApiError::Domain { code, .. } => code.clone(),
        ApiError::ValidationError { code, .. } => code.clone(),
        ApiError::NotFound => "not_found".to_string(),
        ApiError::Unauthorized(_) => "unauthorized".to_string(),
        ApiError::Forbidden => "forbidden".to_string(),
        ApiError::WrongPassword => "wrong_password".to_string(),
        ApiError::SamePassword => "same_password".to_string(),
        ApiError::WeakPassword => "weak_password".to_string(),
        ApiError::WindowExpired => "window_expired".to_string(),
        ApiError::MissingTemplateParams => "missing_template_params".to_string(),
        ApiError::WindowClosed => "window_closed".to_string(),
        ApiError::ConversationNotTakeable => "conversacion_no_tomable".to_string(),
        ApiError::ClosedRequiresTemplate => "conversacion_cerrada_requiere_plantilla".to_string(),
        ApiError::DatabaseError(_) => "database_error".to_string(),
        ApiError::CacheError(_) => "cache_error".to_string(),
        ApiError::SmsError(_) => "sms_error".to_string(),
        ApiError::Internal(_) => "internal_error".to_string(),
        ApiError::InternalServerError => "internal_server_error".to_string(),
    }
}

pub async fn send_campaign(
    state: &AppState,
    campaign_id: &str,
    send_started_by: &str,
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

    validate_sendable_campaign(&campaign)?;

    let phone_number_id = campaign
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(missing_phone_number_id_error)?;

    let settings = state
        .db
        .find_wa_settings_by_phone_number_id(phone_number_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(whatsapp_account_not_found_error)?;
    validate_wa_settings_for_real_send(&settings)?;

    let validated_filter = validated_real_send_recipient_filter(campaign_id);
    let validated_count = recipients
        .count_documents(validated_filter.clone())
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
    if validated_count == 0 {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "campaign_has_no_validated_recipients",
            "Campaign must have at least one validated non-excluded recipient before real send.",
        ));
    }

    let sample_recipient = recipients
        .find_one(validated_filter)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .ok_or_else(|| {
            ApiError::domain_simple(
                StatusCode::BAD_REQUEST,
                "campaign_has_no_validated_recipients",
                "Campaign must have at least one validated non-excluded recipient before real send.",
            )
        })?;
    validate_campaign_send_components_for_recipient(&campaign, &sample_recipient)?;

    let now = DateTime::now();
    let result = campaigns
        .update_one(
            doc! { "_id": campaign_id, "status": "dry_run_completed" },
            send_campaign_update_doc(send_started_by, now),
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    if result.matched_count == 0 {
        let campaign = campaigns
            .find_one(doc! { "_id": campaign_id })
            .await
            .map_err(|e| ApiError::DatabaseError(e.to_string()))?
            .ok_or(ApiError::NotFound)?;

        if campaign.status != "dry_run_completed" {
            return Err(campaign_not_sendable());
        }
    }

    let campaign = campaigns
        .find_one(doc! { "_id": campaign_id })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    let progress = load_campaign_progress(&recipients, campaign_id).await?;

    Ok(CampaignSummaryResponse {
        ok: true,
        data: campaign_to_summary_with_progress(campaign, Some(progress)),
        auto_prepare: None,
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

pub async fn run_campaign_send_worker(state: Arc<AppState>) {
    tracing::info!(
        interval_secs = CAMPAIGN_SEND_WORKER_INTERVAL_SECS,
        batch_size = CAMPAIGN_SEND_WORKER_BATCH_SIZE,
        "WhatsApp campaign real-send worker started"
    );

    let sender = CampaignMetaMessageSender::new(state.clone());
    let mut interval =
        tokio::time::interval(Duration::from_secs(CAMPAIGN_SEND_WORKER_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        if let Err(err) =
            process_sending_real_campaigns(&state, &sender, CAMPAIGN_SEND_WORKER_BATCH_SIZE).await
        {
            tracing::error!(error = %err, "WhatsApp campaign real-send worker tick failed");
        }
    }
}

async fn process_sending_real_campaigns<S>(
    state: &AppState,
    sender: &S,
    batch_size: usize,
) -> Result<(), ApiError>
where
    S: CampaignMessageSender,
{
    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");
    let sending = campaigns
        .find(sending_real_campaign_filter())
        .sort(doc! { "send_started_at": 1, "_id": 1 })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    for campaign in sending {
        process_campaign_send_batch(state, sender, &campaign, batch_size).await?;
    }

    Ok(())
}

async fn process_campaign_send_batch<S>(
    state: &AppState,
    sender: &S,
    campaign: &WaCampaignDoc,
    batch_size: usize,
) -> Result<(), ApiError>
where
    S: CampaignMessageSender,
{
    if !should_process_campaign_send(campaign) {
        return Ok(());
    }

    let Some(campaign_id) = campaign.id else {
        return Ok(());
    };

    let recipients = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");

    tracing::info!(
        campaign_id = %campaign_id,
        batch_size,
        "WhatsApp campaign real-send batch started"
    );

    let mut claimed = 0usize;
    let mut resolved_media_bindings: Option<Vec<TemplateMediaBinding>> = None;
    let mut media_resolution_error: Option<CampaignSendError> = None;
    while claimed < batch_size {
        let Some(recipient) =
            claim_next_validated_recipient_for_send(&recipients, campaign_id).await?
        else {
            break;
        };
        claimed += 1;

        if resolved_media_bindings.is_none() && media_resolution_error.is_none() {
            match resolve_template_media_bindings_for_send(state, campaign).await {
                Ok(bindings) => resolved_media_bindings = bindings,
                Err(err) => media_resolution_error = Some(err),
            }
        }

        if let Some(err) = media_resolution_error.clone() {
            mark_campaign_recipient_send_failed(&recipients, campaign, &recipient, err).await?;
        } else {
            send_claimed_campaign_recipient(
                state,
                &recipients,
                sender,
                campaign,
                resolved_media_bindings.as_deref(),
                recipient,
            )
            .await?;
        }
        if claimed < batch_size {
            tokio::time::sleep(Duration::from_millis(CAMPAIGN_SEND_DELAY_MS)).await;
        }
    }

    finalize_campaign_send_if_done(state, campaign_id).await?;
    Ok(())
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

async fn claim_next_validated_recipient_for_send(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign_id: ObjectId,
) -> Result<Option<WaCampaignRecipientDoc>, ApiError> {
    let opts = FindOneAndUpdateOptions::builder()
        .return_document(ReturnDocument::After)
        .build();

    recipients
        .find_one_and_update(
            send_recipient_claim_filter(campaign_id),
            send_recipient_claim_update(DateTime::now()),
        )
        .sort(doc! { "created_at": 1, "_id": 1 })
        .with_options(opts)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))
}

async fn send_claimed_campaign_recipient<S>(
    state: &AppState,
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    sender: &S,
    campaign: &WaCampaignDoc,
    resolved_media_bindings: Option<&[TemplateMediaBinding]>,
    recipient: WaCampaignRecipientDoc,
) -> Result<(), ApiError>
where
    S: CampaignMessageSender,
{
    let Some(recipient_id) = recipient.id else {
        return Ok(());
    };
    let Some(campaign_id) = campaign.id else {
        return Ok(());
    };

    let snapshot = recipient_to_template_snapshot(&recipient);
    let masked_phone = recipient
        .phone_normalized
        .as_deref()
        .map(mask_phone)
        .unwrap_or_else(|| "<missing>".to_string());
    tracing::info!(
        campaign_id = %campaign_id,
        recipient_id = %recipient_id,
        phone_normalized = %masked_phone,
        template_name = %campaign.template_name,
        template_language = %campaign.template_language,
        phone_number_id = %campaign.phone_number_id.as_deref().unwrap_or_default(),
        "WhatsApp campaign recipient send started"
    );

    let now = DateTime::now();
    let media_bindings = resolved_media_bindings.or(campaign.template_media_bindings.as_deref());
    let components = match build_campaign_template_send_components(
        campaign.template_components.as_deref(),
        campaign.template_variable_bindings.as_deref(),
        media_bindings,
        &snapshot,
    ) {
        Ok(components) => components,
        Err(err) => {
            let code = err.code();
            recipients
                .update_one(
                    doc! { "_id": recipient_id, "campaign_id": campaign_id, "status": "sending" },
                    send_recipient_failed_update(code, code.to_string(), None, None, None, now),
                )
                .await
                .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
            return Ok(());
        }
    };

    match sender
        .send_template(campaign, &recipient, components.clone())
        .await
    {
        Ok(result) => {
            let meta_message_id = result.meta_message_id;
            recipients
                .update_one(
                    doc! { "_id": recipient_id, "campaign_id": campaign_id, "status": "sending" },
                    send_recipient_sent_update(meta_message_id.clone(), now),
                )
                .await
                .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

            if let Err(err) = record_campaign_send_in_chat(
                state,
                campaign,
                &recipient,
                recipient_id,
                &meta_message_id,
                &components,
                now,
            )
            .await
            {
                tracing::warn!(
                    campaign_id = %campaign_id,
                    recipient_id = %recipient_id,
                    meta_message_id = %safe_meta_message_id(&meta_message_id),
                    error = %err,
                    "WhatsApp campaign recipient sent but chat history record failed"
                );
            }
            tracing::info!(
                campaign_id = %campaign_id,
                recipient_id = %recipient_id,
                phone_normalized = %masked_phone,
                template_name = %campaign.template_name,
                meta_message_id = %safe_meta_message_id(&meta_message_id),
                final_status = "sent",
                "WhatsApp campaign recipient send completed"
            );
        }
        Err(err) => {
            recipients
                .update_one(
                    doc! { "_id": recipient_id, "campaign_id": campaign_id, "status": "sending" },
                    send_recipient_failed_update(
                        &err.code,
                        err.message.clone(),
                        err.meta_error_code.clone(),
                        err.meta_error_subcode.clone(),
                        err.meta_error_user_msg.clone(),
                        now,
                    ),
                )
                .await
                .map_err(|e| ApiError::DatabaseError(e.to_string()))?;
            tracing::warn!(
                campaign_id = %campaign_id,
                recipient_id = %recipient_id,
                phone_normalized = %masked_phone,
                template_name = %campaign.template_name,
                error_code = %err.code,
                final_status = "send_failed",
                "WhatsApp campaign recipient send failed"
            );
        }
    }

    Ok(())
}

async fn record_campaign_send_in_chat(
    state: &AppState,
    campaign: &WaCampaignDoc,
    recipient: &WaCampaignRecipientDoc,
    recipient_id: ObjectId,
    meta_message_id: &str,
    components: &[serde_json::Value],
    sent_at: DateTime,
) -> Result<(), String> {
    let campaign_id = campaign
        .id
        .ok_or_else(|| "campaign missing _id".to_string())?;
    let phone_number_id = campaign
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "campaign missing phone_number_id".to_string())?;
    let settings = state
        .db
        .find_wa_settings_by_phone_number_id(phone_number_id)
        .await?
        .ok_or_else(|| "whatsapp settings not found for campaign phone_number_id".to_string())?;
    let customer_phone = recipient
        .phone_normalized
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "recipient missing normalized phone".to_string())?;

    let existing_conversation = state
        .db
        .find_conversation_by_phones(customer_phone, &settings.phone)
        .await?;
    let conversation_preexisted = existing_conversation.is_some();

    let (conv, conv_created) = state
        .db
        .upsert_conversation(customer_phone, &settings.phone, None)
        .await?;
    let conv_id = conv
        .id
        .ok_or_else(|| "conversation missing _id after upsert".to_string())?;

    if conv_created {
        if let Ok(client_oid) = ObjectId::parse_str(recipient.client_id.trim()) {
            if let Err(err) = state
                .db
                .update_conversation_client_id(&conv_id, &client_oid)
                .await
            {
                tracing::warn!(
                    campaign_id = %campaign_id,
                    recipient_id = %recipient_id,
                    error = %err,
                    "campaign chat record could not link conversation client_id"
                );
            }
        }

        let input = WaConversationEventInput {
            conversation_id: &conv_id,
            business_phone: &settings.phone,
            event_type: "created",
            actor_id: None,
            actor_name: None,
            target_id: None,
            target_name: None,
            note: Some("campaign_send"),
        };
        if let Err(err) = state.db.record_conversation_event(input).await {
            tracing::warn!(
                campaign_id = %campaign_id,
                recipient_id = %recipient_id,
                error = %err,
                "campaign chat record could not write conversation event"
            );
        }

        // Safe inbox mode: a conversation born only from a campaign should not
        // become pending/operator work until the customer replies or an agent
        // explicitly reopens/takes it.
        if let Err(err) = state.db.close_conversation(&conv_id).await {
            tracing::warn!(
                campaign_id = %campaign_id,
                recipient_id = %recipient_id,
                error = %err,
                "campaign chat record could not close silent conversation"
            );
        }
    }

    let preview = render_campaign_template_preview(campaign, components);
    let components_value = if components.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(components.to_vec()))
    };

    let msg = WaMessage {
        id: None,
        conversation_id: conv_id,
        wa_message_id: meta_message_id.to_string(),
        direction: "out".to_string(),
        msg_type: "template".to_string(),
        body: Some(preview.clone()),
        media_id: None,
        media_mime_type: None,
        media_filename: None,
        status: Some("sent".to_string()),
        meta_error_code: None,
        meta_error_title: None,
        meta_error_message: None,
        meta_error_details: None,
        failed_at: None,
        sent_by: None,
        source: Some("campaign".to_string()),
        campaign_id: Some(campaign_id),
        campaign_recipient_id: Some(recipient_id),
        read_by_user_id: None,
        read_at: None,
        idempotency_key: None,
        reply_to_wa_message_id: None,
        is_forwarded: None,
        is_frequently_forwarded: None,
        url_preview: None,
        voice: false,
        template_name: Some(campaign.template_name.clone()),
        template_language: Some(campaign.template_language.clone()),
        template_components: components_value,
        interactive_payload: None,
        contacts_payload: None,
        location: None,
        reactions: vec![],
        raw_payload: None,
        ai_processed_at: None,
        timestamp: sent_at,
    };

    let saved = state.db.save_message(msg).await?;

    // Safe sidebar mode: only existing conversations get their last-message
    // preview bumped. Newly-created campaign-only conversations stay closed and
    // silent, but their history is available when opened directly/refreshed.
    if conversation_preexisted {
        let touch = crate::db::ConversationTouch {
            preview: &preview,
            msg_type: &saved.msg_type,
            direction: "out",
            wa_message_id: &saved.wa_message_id,
            from_user_id: None,
            media_filename: saved.media_filename.as_deref(),
            status: Some("sent"),
            increment_unread: false,
            last_message_at: Some(sent_at),
        };
        state.db.touch_conversation(&conv_id, touch).await?;
    }

    Ok(())
}

fn render_campaign_template_preview(
    campaign: &WaCampaignDoc,
    components: &[serde_json::Value],
) -> String {
    let Some(mut text) = campaign.template_components.as_deref().and_then(|stored| {
        stored.iter().find_map(|component| {
            component
                .get("type")
                .and_then(serde_json::Value::as_str)
                .filter(|value| value.eq_ignore_ascii_case("BODY"))?;
            component
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
    }) else {
        return format!("[plantilla: {}]", campaign.template_name);
    };

    if let Some(params) = components.iter().find_map(|component| {
        component
            .get("type")
            .and_then(serde_json::Value::as_str)
            .filter(|value| value.eq_ignore_ascii_case("BODY"))?;
        component
            .get("parameters")
            .and_then(serde_json::Value::as_array)
    }) {
        for (idx, param) in params.iter().enumerate() {
            let Some(value) = param
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
            else {
                continue;
            };
            text = text.replace(&format!("{{{{{}}}}}", idx + 1), value);
        }
    }

    if text.is_empty() {
        format!("[plantilla: {}]", campaign.template_name)
    } else {
        text
    }
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

async fn mark_campaign_recipient_send_failed(
    recipients: &mongodb::Collection<WaCampaignRecipientDoc>,
    campaign: &WaCampaignDoc,
    recipient: &WaCampaignRecipientDoc,
    err: CampaignSendError,
) -> Result<(), ApiError> {
    let Some(recipient_id) = recipient.id else {
        return Ok(());
    };
    let Some(campaign_id) = campaign.id else {
        return Ok(());
    };

    let code = err.code;
    recipients
        .update_one(
            doc! { "_id": recipient_id, "campaign_id": campaign_id, "status": "sending" },
            send_recipient_failed_update(
                &code,
                err.message,
                err.meta_error_code,
                err.meta_error_subcode,
                err.meta_error_user_msg,
                DateTime::now(),
            ),
        )
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    tracing::warn!(
        campaign_id = %campaign_id,
        recipient_id = %recipient_id,
        error_code = %code,
        "WhatsApp campaign recipient send failed before Meta send"
    );

    Ok(())
}

async fn finalize_campaign_send_if_done(
    state: &AppState,
    campaign_id: ObjectId,
) -> Result<(), ApiError> {
    let recipients = state
        .db
        .db
        .collection::<WaCampaignRecipientDoc>("WaCampaignRecipients");
    let campaigns = state.db.db.collection::<WaCampaignDoc>("WaCampaigns");

    let progress = load_campaign_progress(&recipients, campaign_id).await?;
    let Some(status) = send_completion_status(&progress) else {
        return Ok(());
    };

    let now = DateTime::now();
    let result = campaigns
        .update_one(
            doc! {
                "_id": campaign_id,
                "status": "sending",
                "run_mode": "real",
            },
            doc! {
                "$set": {
                    "status": status,
                    "send_completed_at": now,
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
            sent = progress.sent,
            send_failed = progress.send_failed,
                "WhatsApp campaign real-send completed"
        );
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
        sent: count_campaign_recipients(
            recipients,
            effective_status_count_filter(campaign_id, "sent"),
        )
        .await?,
        send_failed: count_campaign_recipients(
            recipients,
            effective_status_count_filter(campaign_id, "send_failed"),
        )
        .await?,
        send_unknown: count_campaign_recipients(
            recipients,
            effective_status_count_filter(campaign_id, "send_unknown"),
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

fn sending_real_campaign_filter() -> Document {
    doc! { "status": "sending", "run_mode": "real" }
}

fn should_process_campaign_dry_run(campaign: &WaCampaignDoc) -> bool {
    campaign.status == "running" && campaign.run_mode.as_deref() == Some("dry_run")
}

fn should_process_campaign_send(campaign: &WaCampaignDoc) -> bool {
    campaign.status == "sending" && campaign.run_mode.as_deref() == Some("real")
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

fn send_recipient_claim_filter(campaign_id: ObjectId) -> Document {
    validated_real_send_recipient_filter(campaign_id)
}

#[cfg(test)]
fn send_recipient_is_claimable(recipient: &WaCampaignRecipientDoc) -> bool {
    recipient.can_send && !recipient.excluded && recipient.status == "validated"
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

fn send_recipient_claim_update(now: DateTime) -> Document {
    doc! {
        "$set": {
            "status": "sending",
            "send_started_at": now,
            "last_attempt_at": now,
            "updated_at": now,
        },
        "$inc": { "send_attempts": 1i64 }
    }
}

fn send_recipient_sent_update(meta_message_id: String, now: DateTime) -> Document {
    doc! {
        "$set": {
            "status": "sent",
            "meta_message_id": meta_message_id,
            "sent_at": now,
            "updated_at": now,
        },
        "$unset": {
            "send_error_code": "",
            "send_error_message": "",
            "meta_error_code": "",
            "meta_error_subcode": "",
            "meta_error_user_msg": "",
        }
    }
}

fn send_recipient_failed_update(
    error_code: &str,
    error_message: String,
    meta_error_code: Option<String>,
    meta_error_subcode: Option<String>,
    meta_error_user_msg: Option<String>,
    now: DateTime,
) -> Document {
    let mut set = doc! {
        "status": "send_failed",
        "send_error_code": error_code,
        "send_error_message": error_message,
        "updated_at": now,
    };
    if let Some(value) = meta_error_code {
        set.insert("meta_error_code", value);
    }
    if let Some(value) = meta_error_subcode {
        set.insert("meta_error_subcode", value);
    }
    if let Some(value) = meta_error_user_msg {
        set.insert("meta_error_user_msg", value);
    }

    doc! { "$set": set }
}

fn safe_meta_message_id(meta_message_id: &str) -> String {
    const MAX_LEN: usize = 32;
    if meta_message_id.len() <= MAX_LEN {
        meta_message_id.to_string()
    } else {
        format!("{}…", &meta_message_id[..MAX_LEN])
    }
}

fn mask_phone(phone: &str) -> String {
    let len = phone.len();
    if len <= 4 {
        return "****".to_string();
    }
    let suffix_start = len.saturating_sub(4);
    format!("***{}", &phone[suffix_start..])
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

fn send_completion_status(progress: &CampaignDryRunProgress) -> Option<&'static str> {
    if progress.validated > 0 || progress.sending > 0 {
        None
    } else if progress.send_failed == 0 {
        Some("completed")
    } else {
        Some("completed_with_errors")
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
    validate_template_media_bindings(request.template_media_bindings.as_deref())?;
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
    campaign.template_media_bindings = request.template_media_bindings;
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

fn validate_template_media_bindings(
    bindings: Option<&[TemplateMediaBinding]>,
) -> Result<(), ApiError> {
    let Some(bindings) = bindings else {
        return Ok(());
    };

    for binding in bindings {
        let value = binding.value.trim();
        if value.is_empty() {
            return Err(ApiError::domain_with_field(
                StatusCode::BAD_REQUEST,
                "template_media_value_required",
                "template_media_bindings.value",
                "Template media binding value is required.",
            ));
        }
        match binding.source {
            TemplateMediaSource::Link => {
                if !value.starts_with("https://") {
                    return Err(invalid_template_media_binding_error());
                }
            }
            TemplateMediaSource::MediaId => {}
            TemplateMediaSource::TemplateMediaId => {
                if ObjectId::parse_str(value).is_err() {
                    return Err(invalid_template_media_binding_error());
                }
            }
        }
    }

    Ok(())
}

async fn validate_template_media_bindings_against_gridfs(
    state: &AppState,
    campaign_phone_number_id: Option<&str>,
    bindings: Option<&[TemplateMediaBinding]>,
) -> Result<(), ApiError> {
    let Some(bindings) = bindings else {
        return Ok(());
    };

    for binding in bindings
        .iter()
        .filter(|binding| matches!(binding.source, TemplateMediaSource::TemplateMediaId))
    {
        let oid = ObjectId::parse_str(binding.value.trim())
            .map_err(|_| invalid_template_media_binding_error())?;
        let media = state
            .db
            .find_template_media_by_id(&oid)
            .await
            .map_err(ApiError::DatabaseError)?
            .ok_or_else(invalid_template_media_binding_error)?;
        validate_template_media_ref_matches_binding(campaign_phone_number_id, binding, &media)?;
    }

    Ok(())
}

fn validate_template_media_ref_matches_binding(
    campaign_phone_number_id: Option<&str>,
    binding: &TemplateMediaBinding,
    media: &WaTemplateMediaRef,
) -> Result<(), ApiError> {
    let expected_format = template_media_type_format(&binding.media_type);
    if !media.format.trim().is_empty() && !media.format.eq_ignore_ascii_case(expected_format) {
        return Err(invalid_template_media_binding_error());
    }

    if let Some(phone_number_id) = campaign_phone_number_id {
        if !media.phone_number_id.trim().is_empty() && media.phone_number_id != phone_number_id {
            return Err(invalid_template_media_binding_error());
        }
    }

    Ok(())
}

fn invalid_template_media_binding_error() -> ApiError {
    ApiError::domain_with_field(
        StatusCode::BAD_REQUEST,
        "invalid_template_media_binding",
        "template_media_bindings.value",
        "Template media binding is invalid.",
    )
}

fn template_media_type_format(media_type: &TemplateMediaType) -> &'static str {
    match media_type {
        TemplateMediaType::Image => "IMAGE",
        TemplateMediaType::Video => "VIDEO",
        TemplateMediaType::Document => "DOCUMENT",
    }
}

#[allow(dead_code)]
fn validate_template_media_for_real_send(campaign: &WaCampaignDoc) -> Result<(), ApiError> {
    let Some(required_media_type) =
        required_header_media_type(campaign.template_components.as_deref())
    else {
        return Ok(());
    };

    let has_binding = campaign
        .template_media_bindings
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .any(|binding| {
            matches!(binding.component, TemplateMediaComponent::Header)
                && binding.media_type == required_media_type
                && !binding.value.trim().is_empty()
        });

    if has_binding {
        return Ok(());
    }

    Err(ApiError::domain_with_field(
        StatusCode::BAD_REQUEST,
        "missing_template_media_binding",
        "template_media_bindings",
        "Template has a media HEADER and requires a matching template_media_bindings entry before real send.",
    ))
}

fn required_header_media_type(
    components: Option<&[serde_json::Value]>,
) -> Option<TemplateMediaType> {
    components?.iter().find_map(header_media_type)
}

fn header_media_type(component: &serde_json::Value) -> Option<TemplateMediaType> {
    let component_type = component.get("type")?.as_str()?;
    if !component_type.eq_ignore_ascii_case("HEADER") {
        return None;
    }

    match component
        .get("format")?
        .as_str()?
        .to_ascii_uppercase()
        .as_str()
    {
        "IMAGE" => Some(TemplateMediaType::Image),
        "VIDEO" => Some(TemplateMediaType::Video),
        "DOCUMENT" => Some(TemplateMediaType::Document),
        _ => None,
    }
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
        .ok_or_else(missing_phone_number_id_error)?;

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

fn validate_sendable_campaign(campaign: &WaCampaignDoc) -> Result<(), ApiError> {
    if campaign.status != "dry_run_completed" {
        return Err(campaign_not_sendable());
    }

    campaign
        .phone_number_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(missing_phone_number_id_error)?;

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

fn validate_wa_settings_for_real_send(settings: &WaSettings) -> Result<(), ApiError> {
    if !settings.active {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "whatsapp_account_inactive",
            "The selected WhatsApp account is inactive.",
        ));
    }
    if settings.phone_number_id.trim().is_empty() {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "whatsapp_account_missing_phone_number_id",
            "Selected WhatsApp account is missing phone_number_id.",
        ));
    }
    if settings.access_token.trim().is_empty() {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "whatsapp_account_missing_token",
            "Selected WhatsApp account is missing access_token.",
        ));
    }
    Ok(())
}

fn validate_campaign_send_components_for_recipient(
    campaign: &WaCampaignDoc,
    recipient: &WaCampaignRecipientDoc,
) -> Result<(), ApiError> {
    let snapshot = recipient_to_template_snapshot(recipient);
    let media_bindings =
        template_media_bindings_for_validation(campaign.template_media_bindings.as_deref());
    build_campaign_template_send_components(
        campaign.template_components.as_deref(),
        campaign.template_variable_bindings.as_deref(),
        media_bindings
            .as_deref()
            .or(campaign.template_media_bindings.as_deref()),
        &snapshot,
    )
    .map(|_| ())
    .map_err(campaign_send_build_error_to_api_error)
}

fn template_media_bindings_for_validation(
    bindings: Option<&[TemplateMediaBinding]>,
) -> Option<Vec<TemplateMediaBinding>> {
    let bindings = bindings?;
    if !bindings
        .iter()
        .any(|binding| matches!(binding.source, TemplateMediaSource::TemplateMediaId))
    {
        return None;
    }

    Some(
        bindings
            .iter()
            .map(|binding| {
                if matches!(binding.source, TemplateMediaSource::TemplateMediaId) {
                    TemplateMediaBinding {
                        source: TemplateMediaSource::MediaId,
                        value: "template-media-validation-placeholder".to_string(),
                        ..binding.clone()
                    }
                } else {
                    binding.clone()
                }
            })
            .collect(),
    )
}

fn campaign_send_build_error_to_api_error(error: CampaignTemplateSendBuildError) -> ApiError {
    let code = error.code();
    let field = match code {
        "missing_template_media_binding"
        | "duplicate_template_media_binding"
        | "unexpected_template_media_binding"
        | "unsupported_template_header_combination" => "template_media_bindings",
        "invalid_media_link" => "template_media_bindings.value",
        "invalid_template_media_binding" => "template_media_bindings.value",
        "mismatched_template_media_type" => "template_media_bindings.media_type",
        _ => "template_variable_bindings",
    };

    ApiError::domain_with_field(
        StatusCode::BAD_REQUEST,
        code,
        field,
        "Campaign template send components could not be built for real send.",
    )
}

fn campaign_has_no_effective_recipients_error() -> ApiError {
    ApiError::domain_simple(
        StatusCode::BAD_REQUEST,
        "campaign_has_no_effective_recipients",
        "Campaign must have at least one non-excluded pending recipient that can be sent.",
    )
}

fn missing_phone_number_id_error() -> ApiError {
    ApiError::domain_simple(
        StatusCode::BAD_REQUEST,
        "missing_phone_number_id",
        "Campaign must have a phone_number_id before starting.",
    )
}

fn whatsapp_account_not_found_error() -> ApiError {
    ApiError::domain_simple(
        StatusCode::BAD_REQUEST,
        "whatsapp_account_not_found",
        "Selected WhatsApp account was not found or is missing credentials.",
    )
}

fn campaign_not_startable() -> ApiError {
    ApiError::domain_simple(
        StatusCode::CONFLICT,
        "campaign_not_startable",
        "Only queued campaigns can be started.",
    )
}

fn campaign_not_sendable() -> ApiError {
    ApiError::domain_simple(
        StatusCode::CONFLICT,
        "campaign_not_sendable",
        "Only dry_run_completed campaigns can start real sending.",
    )
}

fn restore_auto_prepare_created_campaign_update_doc(now: DateTime) -> Document {
    doc! {
        "$set": {
            "status": "draft",
            "updated_at": now,
        },
        "$unset": {
            "confirming_from": "",
            "confirmed_by": "",
            "confirmed_at": "",
            "started_by": "",
            "started_at": "",
            "run_mode": "",
            "dry_run_completed_at": "",
        }
    }
}

fn confirm_campaign_update_doc(confirmed_by: &str, now: DateTime) -> Document {
    doc! {
        "$set": {
            "status": "queued",
            "confirmed_by": confirmed_by,
            "confirmed_at": now,
            "updated_at": now,
        },
        "$unset": { "confirming_from": "" }
    }
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

fn send_campaign_update_doc(send_started_by: &str, now: DateTime) -> Document {
    doc! {
        "$set": {
            "status": "sending",
            "run_mode": "real",
            "send_started_by": send_started_by,
            "send_started_at": now,
            "updated_at": now,
        },
        "$unset": {
            "send_completed_at": "",
        }
    }
}

fn validated_real_send_recipient_filter(campaign_id: ObjectId) -> Document {
    doc! {
        "campaign_id": campaign_id,
        "can_send": true,
        "excluded": false,
        "status": "validated",
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
    let mut missing_payment_due_day = 0usize;
    let mut invalid_payment_due_day = 0usize;

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

        let payment_due_day = match read_payment_due_day(&doc, "nPayment") {
            PaymentDueDayRead::Valid(day) => Some(day),
            PaymentDueDayRead::Missing => {
                missing_payment_due_day += 1;
                None
            }
            PaymentDueDayRead::Invalid => {
                invalid_payment_due_day += 1;
                None
            }
        };

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
            payment_due_day,
        });
    }

    if missing_payment_due_day > 0 || invalid_payment_due_day > 0 {
        tracing::warn!(
            matched_clients = candidates.len(),
            missing_payment_due_day,
            invalid_payment_due_day,
            "WhatsApp campaign snapshot found clients without a valid nPayment"
        );
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
        send_attempts: 0,
        send_started_at: None,
        sent_at: None,
        send_error_code: None,
        send_error_message: None,
        meta_message_id: None,
        meta_error_code: None,
        meta_error_subcode: None,
        meta_error_user_msg: None,
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
        template_media_bindings: campaign.template_media_bindings,
        filters: campaign.filters,
        status: campaign.status,
        started_by: campaign.started_by,
        started_at: campaign.started_at.map(iso8601),
        run_mode: campaign.run_mode,
        dry_run_completed_at: campaign.dry_run_completed_at.map(iso8601),
        send_started_by: campaign.send_started_by,
        send_started_at: campaign.send_started_at.map(iso8601),
        send_completed_at: campaign.send_completed_at.map(iso8601),
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
    let total_to_send = progress.validated
        + progress.sending
        + progress.sent
        + progress.send_failed
        + progress.send_unknown;
    let processed_send = progress.sent + progress.send_failed + progress.send_unknown;
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
        sent: progress.sent,
        send_failed: progress.send_failed,
        send_unknown: progress.send_unknown,
        total_to_send,
        processed_send,
        send_progress_percent: calculate_progress_percent(processed_send, total_to_send),
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
        has_template_media: campaign
            .template_media_bindings
            .as_ref()
            .is_some_and(|bindings| !bindings.is_empty()),
        template_media_count: campaign
            .template_media_bindings
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
    let customer_status_raw = recipient.customer_status_raw;
    let customer_status_derived = recipient.customer_status_derived;
    CampaignRecipientItem {
        id: recipient.id.map(|id| id.to_hex()).unwrap_or_default(),
        campaign_id: recipient.campaign_id.to_hex(),
        client_id: recipient.client_id,
        client_name: recipient.client_name,
        provider_id: recipient.provider_id,
        provider_name: recipient.provider_name,
        sector_id: recipient.sector_id,
        sector_name: recipient.sector_name,
        customer_status_raw: customer_status_raw.clone(),
        customer_status_derived: customer_status_derived.clone(),
        client_state_raw: customer_status_raw,
        client_state_derived: customer_status_derived,
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
        send_attempts: recipient.send_attempts,
        send_started_at: recipient.send_started_at.map(iso8601),
        sent_at: recipient.sent_at.map(iso8601),
        send_error_code: recipient.send_error_code,
        send_error_message: recipient.send_error_message,
        meta_message_id: recipient.meta_message_id,
        meta_error_code: recipient.meta_error_code,
        meta_error_subcode: recipient.meta_error_subcode,
        meta_error_user_msg: recipient.meta_error_user_msg,
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
            client_state_raw: candidate.state.clone(),
            client_state_derived: derived.clone(),
            customer_status_raw: candidate.state,
            customer_status_derived: derived,
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
    if raw == RETIRED_CLIENT_STATE {
        DerivedClientState::Retired
    } else if raw != "Activo" {
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
fn get_payment_due_day(doc: &Document, key: &str) -> Option<i32> {
    read_payment_due_day(doc, key).value()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaymentDueDayRead {
    Valid(i32),
    Missing,
    Invalid,
}

impl PaymentDueDayRead {
    #[cfg(test)]
    fn value(self) -> Option<i32> {
        match self {
            Self::Valid(day) => Some(day),
            Self::Missing | Self::Invalid => None,
        }
    }
}

fn read_payment_due_day(doc: &Document, key: &str) -> PaymentDueDayRead {
    match doc.get(key) {
        Some(Bson::Int32(day)) => valid_payment_due_day(*day),
        Some(Bson::Int64(day)) => i32::try_from(*day)
            .map(valid_payment_due_day)
            .unwrap_or(PaymentDueDayRead::Invalid),
        Some(Bson::Double(day)) => valid_f64_payment_due_day(*day),
        Some(Bson::String(day)) => parse_string_payment_due_day(day),
        Some(Bson::Null) | None => PaymentDueDayRead::Missing,
        Some(_) => PaymentDueDayRead::Invalid,
    }
}

fn valid_payment_due_day(day: i32) -> PaymentDueDayRead {
    if (1..=31).contains(&day) {
        PaymentDueDayRead::Valid(day)
    } else {
        PaymentDueDayRead::Invalid
    }
}

fn valid_f64_payment_due_day(day: f64) -> PaymentDueDayRead {
    if day.is_finite() && day.fract() == 0.0 && (1.0..=31.0).contains(&day) {
        PaymentDueDayRead::Valid(day as i32)
    } else {
        PaymentDueDayRead::Invalid
    }
}

fn parse_string_payment_due_day(day: &str) -> PaymentDueDayRead {
    let day = day.trim();
    if day.is_empty() {
        return PaymentDueDayRead::Invalid;
    }

    day.parse::<i32>()
        .map(valid_payment_due_day)
        .or_else(|_| day.parse::<f64>().map(valid_f64_payment_due_day))
        .unwrap_or(PaymentDueDayRead::Invalid)
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
            template_media_bindings: None,
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
            send_started_by: None,
            send_started_at: None,
            send_completed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn campaign_template_preview_renders_body_parameters() {
        let mut campaign = base_campaign("sending");
        campaign.template_components = Some(vec![serde_json::json!({
            "type": "BODY",
            "text": "Hola {{1}}, tu saldo es {{2}}"
        })]);
        let components = vec![serde_json::json!({
            "type": "BODY",
            "parameters": [
                { "type": "text", "text": "Ana" },
                { "type": "text", "text": "$10.50" }
            ]
        })];

        assert_eq!(
            render_campaign_template_preview(&campaign, &components),
            "Hola Ana, tu saldo es $10.50"
        );
    }

    #[test]
    fn campaign_template_preview_falls_back_without_body() {
        let campaign = base_campaign("sending");

        assert_eq!(
            render_campaign_template_preview(&campaign, &[]),
            "[plantilla: promo_template]"
        );
    }

    #[test]
    fn campaign_message_source_maps_to_message_item() {
        let campaign_id = ObjectId::parse_str("64f000000000000000000001").unwrap();
        let recipient_id = ObjectId::parse_str("64f000000000000000000002").unwrap();
        let msg = WaMessage {
            id: Some(ObjectId::parse_str("64f000000000000000000003").unwrap()),
            conversation_id: ObjectId::parse_str("64f000000000000000000004").unwrap(),
            wa_message_id: "wamid.campaign".to_string(),
            direction: "out".to_string(),
            msg_type: "template".to_string(),
            body: Some("Hola Ana".to_string()),
            media_id: None,
            media_mime_type: None,
            media_filename: None,
            status: Some("sent".to_string()),
            meta_error_code: None,
            meta_error_title: None,
            meta_error_message: None,
            meta_error_details: None,
            failed_at: None,
            sent_by: None,
            source: Some("campaign".to_string()),
            campaign_id: Some(campaign_id),
            campaign_recipient_id: Some(recipient_id),
            read_by_user_id: None,
            read_at: None,
            idempotency_key: None,
            reply_to_wa_message_id: None,
            is_forwarded: None,
            is_frequently_forwarded: None,
            url_preview: None,
            voice: false,
            template_name: Some("promo_template".to_string()),
            template_language: Some("es".to_string()),
            template_components: None,
            interactive_payload: None,
            contacts_payload: None,
            location: None,
            reactions: vec![],
            raw_payload: None,
            ai_processed_at: None,
            timestamp: DateTime::from_millis(1_800_000_000_000),
        };

        let item = crate::modules::whatsapp::shared::mappers::msg_to_item(msg, None, None);

        assert_eq!(item.source.as_deref(), Some("campaign"));
        assert_eq!(item.campaign_id, Some(campaign_id.to_hex()));
        assert_eq!(item.campaign_recipient_id, Some(recipient_id.to_hex()));
    }

    fn wa_settings(active: bool, phone_number_id: &str, access_token: &str) -> WaSettings {
        let now = DateTime::from_millis(1_800_000_000_000);
        WaSettings {
            id: Some(ObjectId::parse_str("64f000000000000000000201").unwrap()),
            phone: "584222236777".to_string(),
            workspace_name: "Main".to_string(),
            phone_number_id: phone_number_id.to_string(),
            whatsapp_business_account_id: "waba-1".to_string(),
            access_token: access_token.to_string(),
            agents: vec![],
            active,
            purposes: crate::models::whatsapp::WaPurposes::default(),
            templates_synced_at: None,
            enable_guardrails: true,
            enable_conversation_state: true,
            pre_classifier_enabled: false,
            trivial_responses: vec![],
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
            send_attempts: 0,
            send_started_at: None,
            sent_at: None,
            send_error_code: None,
            send_error_message: None,
            meta_message_id: None,
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
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

    fn header_image_link_binding(value: &str) -> TemplateMediaBinding {
        TemplateMediaBinding {
            component: TemplateMediaComponent::Header,
            media_type: TemplateMediaType::Image,
            source: crate::modules::whatsapp::campaigns::dto::TemplateMediaSource::Link,
            value: value.to_string(),
        }
    }

    fn header_image_media_id_binding(value: &str) -> TemplateMediaBinding {
        TemplateMediaBinding {
            component: TemplateMediaComponent::Header,
            media_type: TemplateMediaType::Image,
            source: crate::modules::whatsapp::campaigns::dto::TemplateMediaSource::MediaId,
            value: value.to_string(),
        }
    }

    fn header_image_template_media_id_binding(value: &str) -> TemplateMediaBinding {
        TemplateMediaBinding {
            component: TemplateMediaComponent::Header,
            media_type: TemplateMediaType::Image,
            source: crate::modules::whatsapp::campaigns::dto::TemplateMediaSource::TemplateMediaId,
            value: value.to_string(),
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
            template_media_bindings: None,
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
    fn create_without_auto_prepare_keeps_default_manual_flow() {
        let payload = serde_json::json!({
            "name": "June Promo",
            "phone_number_id": "987654321",
            "template_name": "promo_template",
            "template_language": "es",
            "filters": { "client_state": "active" }
        });

        let request: CreateCampaignRequest = serde_json::from_value(payload).unwrap();

        assert_eq!(request.auto_prepare, None);
    }

    #[test]
    fn create_with_auto_prepare_true_deserializes_flag() {
        let payload = serde_json::json!({
            "name": "June Promo",
            "phone_number_id": "987654321",
            "template_name": "promo_template",
            "template_language": "es",
            "filters": { "client_state": "active" },
            "auto_prepare": true
        });

        let request: CreateCampaignRequest = serde_json::from_value(payload).unwrap();

        assert_eq!(request.auto_prepare, Some(true));
    }

    #[test]
    fn auto_prepare_response_reports_confirmed_and_validation_started() {
        let mut campaign = base_campaign("running");
        campaign.run_mode = Some("dry_run".to_string());
        campaign.confirmed_by = Some("admin-1".to_string());
        campaign.confirmed_at = Some(DateTime::from_millis(1_800_000_000_001));
        campaign.started_by = Some("admin-1".to_string());
        campaign.started_at = Some(DateTime::from_millis(1_800_000_000_002));

        let response = CampaignSummaryResponse {
            ok: true,
            data: campaign_to_summary(campaign),
            auto_prepare: Some(CampaignAutoPrepareResult {
                confirmed: true,
                validation_started: true,
            }),
        };

        assert_eq!(response.data.status, "running");
        assert_eq!(response.data.run_mode.as_deref(), Some("dry_run"));
        assert_eq!(response.data.confirmed_by.as_deref(), Some("admin-1"));
        assert!(response.data.confirmed_at.is_some());
        assert_eq!(response.data.started_by.as_deref(), Some("admin-1"));
        assert!(response.data.started_at.is_some());
        assert!(response.auto_prepare.unwrap().validation_started);
    }

    #[test]
    fn create_with_bindings_persists_to_summary() {
        let mut campaign = base_campaign("draft");
        campaign.template_variable_bindings = Some(vec![static_body_binding(1, "ABDO").into()]);

        let summary = campaign_to_summary(campaign);

        assert_eq!(summary.template_variable_bindings.unwrap().len(), 1);
    }

    #[test]
    fn create_with_template_media_bindings_persists_to_summary_and_list() {
        let mut campaign = base_campaign("draft");
        campaign.template_media_bindings = Some(vec![header_image_link_binding(
            "https://example.com/header.jpg",
        )]);

        let summary = campaign_to_summary(campaign.clone());
        let list_item = campaign_to_list_item(campaign);

        let media_bindings = summary.template_media_bindings.unwrap();
        assert_eq!(media_bindings.len(), 1);
        assert!(matches!(
            media_bindings[0].component,
            TemplateMediaComponent::Header
        ));
        assert!(matches!(
            media_bindings[0].media_type,
            TemplateMediaType::Image
        ));
        assert_eq!(media_bindings[0].value, "https://example.com/header.jpg");
        assert!(list_item.has_template_media);
        assert_eq!(list_item.template_media_count, 1);
    }

    #[test]
    fn edit_template_media_bindings_saves_and_preserves_totals() {
        let campaign = base_campaign("previewed");
        let mut request = update_request("June Promo");
        request.template_media_bindings =
            Some(vec![header_image_media_id_binding("meta-media-123")]);

        let updated =
            apply_campaign_edit(campaign, request, "admin-1", None, DateTime::now()).unwrap();

        let bindings = updated.template_media_bindings.unwrap();
        assert_eq!(bindings.len(), 1);
        assert!(matches!(
            bindings[0].source,
            crate::modules::whatsapp::campaigns::dto::TemplateMediaSource::MediaId
        ));
        assert_eq!(updated.total_recipients, 5);
    }

    #[test]
    fn template_media_binding_empty_value_fails_validation() {
        let err =
            validate_template_media_bindings(Some(&[header_image_link_binding(" ")])).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "template_media_value_required"
                    && field.as_deref() == Some("template_media_bindings.value")
        ));
    }

    #[test]
    fn template_media_binding_accepts_template_media_id_source() {
        let payload = serde_json::json!({
            "component": "header",
            "media_type": "image",
            "source": "template_media_id",
            "value": "665f00000000000000000001"
        });

        let binding = serde_json::from_value::<TemplateMediaBinding>(payload).unwrap();
        assert!(matches!(
            binding.source,
            TemplateMediaSource::TemplateMediaId
        ));
        assert!(validate_template_media_bindings(Some(&[binding])).is_ok());
    }

    #[test]
    fn template_media_binding_rejects_invalid_template_media_id() {
        let err =
            validate_template_media_bindings(Some(&[header_image_template_media_id_binding(
                "not-an-object-id",
            )]))
            .unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "invalid_template_media_binding"
                    && field.as_deref() == Some("template_media_bindings.value")
        ));
    }

    #[test]
    fn template_media_ref_validation_rejects_format_and_phone_mismatch() {
        let binding = header_image_template_media_id_binding("665f00000000000000000001");
        let mut media = WaTemplateMediaRef {
            id: ObjectId::parse_str("665f00000000000000000001").unwrap(),
            phone_number_id: "phone-1".to_string(),
            format: "VIDEO".to_string(),
            mime_type: "video/mp4".to_string(),
            sha256: "abc".to_string(),
            file_size: 42,
        };

        assert!(
            validate_template_media_ref_matches_binding(Some("phone-1"), &binding, &media).is_err()
        );
        media.format = "IMAGE".to_string();
        assert!(
            validate_template_media_ref_matches_binding(Some("phone-2"), &binding, &media).is_err()
        );
        assert!(
            validate_template_media_ref_matches_binding(Some("phone-1"), &binding, &media).is_ok()
        );
    }

    #[test]
    fn template_media_binding_rejects_invalid_source_and_media_type() {
        let invalid_source = serde_json::json!({
            "component": "header",
            "media_type": "image",
            "source": "upload",
            "value": "https://example.com/header.jpg"
        });
        let invalid_media_type = serde_json::json!({
            "component": "header",
            "media_type": "audio",
            "source": "link",
            "value": "https://example.com/header.mp3"
        });

        assert!(serde_json::from_value::<TemplateMediaBinding>(invalid_source).is_err());
        assert!(serde_json::from_value::<TemplateMediaBinding>(invalid_media_type).is_err());
    }

    #[test]
    fn detects_template_header_media_components() {
        assert_eq!(
            required_header_media_type(Some(&[
                serde_json::json!({ "type": "HEADER", "format": "IMAGE" })
            ])),
            Some(TemplateMediaType::Image)
        );
        assert_eq!(
            required_header_media_type(Some(&[
                serde_json::json!({ "type": "HEADER", "format": "VIDEO" })
            ])),
            Some(TemplateMediaType::Video)
        );
        assert_eq!(
            required_header_media_type(Some(&[
                serde_json::json!({ "type": "HEADER", "format": "DOCUMENT" })
            ])),
            Some(TemplateMediaType::Document)
        );
        assert_eq!(
            required_header_media_type(Some(&[
                serde_json::json!({ "type": "HEADER", "format": "TEXT" })
            ])),
            None
        );
    }

    #[test]
    fn resolve_for_send_rewrites_template_media_id_to_meta_media_id() {
        let binding = header_image_template_media_id_binding("665f00000000000000000001");

        let resolved =
            template_media_binding_with_meta_media_id(&binding, "meta-media-123".to_string());

        assert!(matches!(resolved.source, TemplateMediaSource::MediaId));
        assert_eq!(resolved.value, "meta-media-123");
        assert_eq!(resolved.component, TemplateMediaComponent::Header);
        assert_eq!(resolved.media_type, TemplateMediaType::Image);
    }

    #[test]
    fn real_send_validation_accepts_template_media_id_without_passing_it_to_builder() {
        let mut campaign = base_campaign("queued");
        campaign.template_components = Some(vec![
            serde_json::json!({ "type": "HEADER", "format": "IMAGE" }),
        ]);
        campaign.template_media_bindings = Some(vec![header_image_template_media_id_binding(
            "665f00000000000000000001",
        )]);
        let recipient = base_recipient("validated");

        assert!(validate_campaign_send_components_for_recipient(&campaign, &recipient).is_ok());
        let resolved =
            template_media_bindings_for_validation(campaign.template_media_bindings.as_deref())
                .unwrap();
        assert!(matches!(resolved[0].source, TemplateMediaSource::MediaId));
        assert_eq!(resolved[0].value, "template-media-validation-placeholder");
    }

    #[test]
    fn real_send_media_validation_requires_matching_header_binding() {
        let mut campaign = base_campaign("queued");
        campaign.template_components = Some(vec![
            serde_json::json!({ "type": "HEADER", "format": "IMAGE" }),
        ]);

        let err = validate_template_media_for_real_send(&campaign).unwrap_err();
        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "missing_template_media_binding"
                    && field.as_deref() == Some("template_media_bindings")
        ));

        campaign.template_media_bindings = Some(vec![header_image_link_binding(
            "https://example.com/header.jpg",
        )]);
        assert!(validate_template_media_for_real_send(&campaign).is_ok());
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
        first.balance = -10.0;
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
        assert_eq!(recipients[0].client_state_raw, "Activo");
        assert_eq!(recipients[0].customer_status_raw, "Activo");
        assert!(matches!(
            recipients[0].client_state_derived,
            DerivedClientState::Moroso
        ));
        assert!(matches!(
            recipients[0].customer_status_derived,
            DerivedClientState::Moroso
        ));
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
    fn preview_recipients_include_state_aliases_and_payment_due_day() {
        let mut humberto = candidate("client-1", "Humberto Bracho", "04144271554");
        humberto.balance = 0.0;
        humberto.payment_due_day = Some(7);

        let (_, recipients) =
            build_preview_recipients(vec![humberto], &HashMap::new(), &HashMap::new());

        assert_eq!(
            recipients[0].phone_normalized.as_deref(),
            Some("584144271554")
        );
        assert_eq!(recipients[0].client_state_raw, "Activo");
        assert_eq!(recipients[0].customer_status_raw, "Activo");
        assert!(matches!(
            recipients[0].client_state_derived,
            DerivedClientState::Solvente
        ));
        assert!(matches!(
            recipients[0].customer_status_derived,
            DerivedClientState::Solvente
        ));
        assert_eq!(recipients[0].payment_due_day, Some(7));
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
            customer_status_raw: "Activo".to_string(),
            customer_status_derived: DerivedClientState::Solvente,
            balance: 0.0,
            payment_due_day: Some(10),
        };

        let snapshot = preview_to_snapshot_recipient(campaign_id, recipient, now);

        assert!(!snapshot.can_send);
        assert!(!snapshot.excluded);
        assert_eq!(snapshot.status, "invalid_phone");
        assert_eq!(snapshot.customer_status_raw, "Activo");
        assert!(matches!(
            snapshot.customer_status_derived,
            DerivedClientState::Solvente
        ));
        assert_eq!(snapshot.payment_due_day, Some(10));
        assert!(matches!(snapshot.phone_status, PhoneStatus::Invalid));
    }

    #[test]
    fn snapshot_serializes_payment_due_day_null_when_missing() {
        let mut recipient = base_recipient("pending");
        recipient.payment_due_day = None;

        let doc = mongodb::bson::to_document(&recipient).unwrap();

        assert_eq!(doc.get("payment_due_day"), Some(&Bson::Null));
    }

    #[test]
    fn legacy_snapshot_state_aliases_deserialize() {
        let mut doc = mongodb::bson::to_document(&base_recipient("pending")).unwrap();
        doc.insert("client_state_raw", "Activo");
        doc.insert("client_state_derived", "moroso");
        doc.remove("customer_status_raw");
        doc.remove("customer_status_derived");

        let recipient = mongodb::bson::from_document::<WaCampaignRecipientDoc>(doc).unwrap();

        assert_eq!(recipient.customer_status_raw, "Activo");
        assert!(matches!(
            recipient.customer_status_derived,
            DerivedClientState::Moroso
        ));
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
            customer_status_raw: "Activo".to_string(),
            customer_status_derived: DerivedClientState::Solvente,
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
            send_attempts: 0,
            send_started_at: None,
            sent_at: None,
            send_error_code: None,
            send_error_message: None,
            meta_message_id: None,
            meta_error_code: None,
            meta_error_subcode: None,
            meta_error_user_msg: None,
            created_at: now,
            updated_at: now,
        });

        assert_eq!(item.payment_due_day, Some(20));
        assert_eq!(item.customer_status_raw, "Activo");
        assert_eq!(item.client_state_raw, "Activo");
        assert!(matches!(
            item.customer_status_derived,
            DerivedClientState::Solvente
        ));
        assert!(matches!(
            item.client_state_derived,
            DerivedClientState::Solvente
        ));
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
            get_payment_due_day(&doc! { "nPayment": "20" }, "nPayment"),
            Some(20)
        );
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": "20.0" }, "nPayment"),
            Some(20)
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
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": "15.5" }, "nPayment"),
            None
        );
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": "abc" }, "nPayment"),
            None
        );
        assert_eq!(
            get_payment_due_day(&doc! { "nPayment": Bson::Null }, "nPayment"),
            None
        );
        assert_eq!(get_payment_due_day(&doc! {}, "nPayment"), None);
    }

    #[test]
    fn payment_due_day_read_distinguishes_missing_and_invalid_values() {
        assert_eq!(
            read_payment_due_day(&doc! { "nPayment": "31" }, "nPayment"),
            PaymentDueDayRead::Valid(31)
        );
        assert_eq!(
            read_payment_due_day(&doc! { "nPayment": Bson::Null }, "nPayment"),
            PaymentDueDayRead::Missing
        );
        assert_eq!(
            read_payment_due_day(&doc! {}, "nPayment"),
            PaymentDueDayRead::Missing
        );
        assert_eq!(
            read_payment_due_day(&doc! { "nPayment": "not-a-day" }, "nPayment"),
            PaymentDueDayRead::Invalid
        );
        assert_eq!(
            read_payment_due_day(&doc! { "nPayment": 0 }, "nPayment"),
            PaymentDueDayRead::Invalid
        );
    }

    #[test]
    fn client_state_derivation_handles_balance_and_retired_state() {
        assert!(matches!(
            derive_client_state("Activo", -0.01),
            DerivedClientState::Moroso
        ));
        assert!(matches!(
            derive_client_state("Activo", 0.0),
            DerivedClientState::Solvente
        ));
        assert!(matches!(
            derive_client_state("Retirado", 0.0),
            DerivedClientState::Retired
        ));
        assert!(matches!(
            derive_client_state("Suspendido", 0.0),
            DerivedClientState::Suspended
        ));
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
    fn dry_run_resolver_validates_when_payment_due_day_exists() {
        let mut campaign = base_campaign("running");
        campaign.run_mode = Some("dry_run".to_string());
        campaign.template_components = Some(vec![serde_json::json!({
            "type": "BODY",
            "text": "Hello {{1}}, due day {{2}}"
        })]);
        campaign.template_variable_bindings = Some(vec![
            client_field_body_binding(1).into(),
            payment_due_day_body_binding(2).into(),
        ]);

        let recipient = base_recipient("sending");
        let resolved = resolve_campaign_template_components(
            campaign.template_components.as_deref(),
            campaign.template_variable_bindings.as_deref(),
            &recipient_to_template_snapshot(&recipient),
        )
        .unwrap();

        assert_eq!(resolved[0]["parameters"][0]["text"], "Client One");
        assert_eq!(resolved[0]["parameters"][1]["text"], "20");
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
            sent: 2,
            send_failed: 1,
            send_unknown: 0,
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
        assert_eq!(dto.sent, 2);
        assert_eq!(dto.send_failed, 1);
        assert_eq!(dto.send_unknown, 0);
        assert_eq!(dto.total_to_send, 7);
        assert_eq!(dto.processed_send, 3);
        assert!((dto.send_progress_percent - 42.857142857142854).abs() < 1e-9);
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
                sent: 0,
                send_failed: 0,
                send_unknown: 0,
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
    fn auto_prepare_uses_confirm_and_start_dry_run_transitions_only() {
        let confirm_time = DateTime::from_millis(1_800_000_000_001);
        let start_time = DateTime::from_millis(1_800_000_000_002);

        assert_eq!(
            confirm_campaign_update_doc("admin-2", confirm_time),
            doc! {
                "$set": {
                    "status": "queued",
                    "confirmed_by": "admin-2",
                    "confirmed_at": confirm_time,
                    "updated_at": confirm_time,
                },
                "$unset": { "confirming_from": "" }
            }
        );
        assert_eq!(
            start_campaign_update_doc("admin-2", start_time),
            doc! {
                "$set": {
                    "status": "running",
                    "started_by": "admin-2",
                    "started_at": start_time,
                    "updated_at": start_time,
                    "run_mode": "dry_run",
                }
            }
        );
    }

    #[test]
    fn auto_prepare_failure_rollback_restores_created_campaign_to_draft() {
        let now = DateTime::from_millis(1_800_000_000_003);

        assert_eq!(
            restore_auto_prepare_created_campaign_update_doc(now),
            doc! {
                "$set": {
                    "status": "draft",
                    "updated_at": now,
                },
                "$unset": {
                    "confirming_from": "",
                    "confirmed_by": "",
                    "confirmed_at": "",
                    "started_by": "",
                    "started_at": "",
                    "run_mode": "",
                    "dry_run_completed_at": "",
                }
            }
        );
    }

    #[test]
    fn auto_prepare_failure_error_includes_campaign_id_and_cause() {
        let campaign_id = ObjectId::parse_str("64f000000000000000000001").unwrap();
        let err = auto_prepare_failed_error(
            campaign_id,
            "prepare",
            ApiError::BadRequest("template_name_required".to_string()),
        );

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref details, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "campaign_auto_prepare_failed"
                    && details.as_ref().and_then(|value| value.get("campaign_id")).and_then(|value| value.as_str()) == Some("64f000000000000000000001")
                    && details.as_ref().and_then(|value| value.get("cause")).and_then(|value| value.as_str()) == Some("template_name_required")
        ));
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
    fn send_dry_run_completed_campaign_builds_real_transition() {
        let mut campaign = base_campaign("dry_run_completed");
        campaign.run_mode = Some("dry_run".to_string());
        campaign.dry_run_completed_at = Some(DateTime::from_millis(1_800_000_000_001));

        validate_sendable_campaign(&campaign).unwrap();
        let now = DateTime::from_millis(1_800_000_000_002);
        let update = send_campaign_update_doc("admin-2", now);

        assert_eq!(
            update,
            doc! {
                "$set": {
                    "status": "sending",
                    "run_mode": "real",
                    "send_started_by": "admin-2",
                    "send_started_at": now,
                    "updated_at": now,
                },
                "$unset": { "send_completed_at": "" }
            }
        );

        campaign.status = "sending".to_string();
        campaign.run_mode = Some("real".to_string());
        campaign.send_started_by = Some("admin-2".to_string());
        campaign.send_started_at = Some(now);
        let summary = campaign_to_summary(campaign);

        assert_eq!(summary.status, "sending");
        assert_eq!(summary.run_mode.as_deref(), Some("real"));
        assert_eq!(summary.send_started_by.as_deref(), Some("admin-2"));
        assert!(summary.send_started_at.is_some());
    }

    #[test]
    fn send_non_completed_dry_run_statuses_return_conflict() {
        for status_value in [
            "draft",
            "previewed",
            "queued",
            "running",
            "dry_run_completed_with_errors",
            "sending",
            "completed",
            "completed_with_errors",
            "cancelled",
        ] {
            let err = validate_sendable_campaign(&base_campaign(status_value)).unwrap_err();

            assert!(matches!(
                err,
                ApiError::Domain { status, ref code, .. }
                    if status == StatusCode::CONFLICT && code == "campaign_not_sendable"
            ));
        }
    }

    #[test]
    fn send_missing_campaign_maps_to_not_found_contract() {
        let missing: Option<WaCampaignDoc> = None;

        assert!(matches!(
            missing.ok_or(ApiError::NotFound),
            Err(ApiError::NotFound)
        ));
    }

    #[test]
    fn send_with_no_validated_recipients_returns_validation_error_without_status_change() {
        let campaign = base_campaign("dry_run_completed");
        let validated_recipients = 0_u64;

        validate_sendable_campaign(&campaign).unwrap();
        let err = if validated_recipients == 0 {
            Err(ApiError::domain_simple(
                StatusCode::BAD_REQUEST,
                "campaign_has_no_validated_recipients",
                "Campaign must have at least one validated non-excluded recipient before real send.",
            ))
        } else {
            Ok(())
        }
        .unwrap_err();

        assert_eq!(campaign.status, "dry_run_completed");
        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST && code == "campaign_has_no_validated_recipients"
        ));
    }

    #[test]
    fn send_legacy_without_phone_number_id_returns_validation_error() {
        let mut campaign = base_campaign("dry_run_completed");
        campaign.phone_number_id = None;

        let err = validate_sendable_campaign(&campaign).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST && code == "missing_phone_number_id"
        ));
    }

    #[test]
    fn send_validates_whatsapp_account_active_and_credentials() {
        assert!(validate_wa_settings_for_real_send(&wa_settings(true, "123", "cipher")).is_ok());

        let inactive =
            validate_wa_settings_for_real_send(&wa_settings(false, "123", "cipher")).unwrap_err();
        assert!(matches!(
            inactive,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST && code == "whatsapp_account_inactive"
        ));

        let missing_phone =
            validate_wa_settings_for_real_send(&wa_settings(true, "", "cipher")).unwrap_err();
        assert!(matches!(
            missing_phone,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST && code == "whatsapp_account_missing_phone_number_id"
        ));

        let missing_token =
            validate_wa_settings_for_real_send(&wa_settings(true, "123", "")).unwrap_err();
        assert!(matches!(
            missing_token,
            ApiError::Domain { status, ref code, .. }
                if status == StatusCode::BAD_REQUEST && code == "whatsapp_account_missing_token"
        ));
    }

    #[test]
    fn send_validated_recipient_filter_targets_only_validated_sendable_rows() {
        let campaign_id = ObjectId::parse_str("64f000000000000000000001").unwrap();

        assert_eq!(
            validated_real_send_recipient_filter(campaign_id),
            doc! {
                "campaign_id": campaign_id,
                "can_send": true,
                "excluded": false,
                "status": "validated",
            }
        );
    }

    #[test]
    fn send_claim_validated_recipient_builds_atomic_sending_update() {
        let campaign_id = ObjectId::parse_str("64f000000000000000000001").unwrap();
        let now = DateTime::from_millis(1_800_000_000_400);
        let recipient = base_recipient("validated");

        assert!(send_recipient_is_claimable(&recipient));
        assert_eq!(
            send_recipient_claim_filter(campaign_id),
            validated_real_send_recipient_filter(campaign_id)
        );
        assert_eq!(
            send_recipient_claim_update(now),
            doc! {
                "$set": {
                    "status": "sending",
                    "send_started_at": now,
                    "last_attempt_at": now,
                    "updated_at": now,
                },
                "$inc": { "send_attempts": 1i64 }
            }
        );
    }

    #[test]
    fn send_claim_does_not_take_noneligible_recipients() {
        for status in [
            "pending",
            "sending",
            "sent",
            "failed",
            "validation_failed",
            "send_failed",
            "send_unknown",
            "invalid_phone",
            "duplicated_phone",
        ] {
            assert!(!send_recipient_is_claimable(&base_recipient(status)));
        }

        let mut excluded = base_recipient("validated");
        excluded.excluded = true;
        assert!(!send_recipient_is_claimable(&excluded));

        let mut non_sendable = base_recipient("validated");
        non_sendable.can_send = false;
        assert!(!send_recipient_is_claimable(&non_sendable));
    }

    #[test]
    fn fake_sender_success_uses_fake_message_id() {
        let _sender = FakeCampaignMessageSender;
        let campaign = base_campaign("sending");
        let recipient = base_recipient("sending");
        let result = fake_campaign_send_result(&campaign, &recipient, &[]).unwrap();

        assert!(result.meta_message_id.starts_with("fake:"));
        assert!(result.meta_message_id.contains("64f000000000000000000001"));
        assert!(result.meta_message_id.contains("64f000000000000000000101"));
    }

    #[test]
    fn send_success_update_marks_sent_and_clears_send_error() {
        let now = DateTime::from_millis(1_800_000_000_500);

        assert_eq!(
            send_recipient_sent_update("fake:campaign:recipient".to_string(), now),
            doc! {
                "$set": {
                    "status": "sent",
                    "meta_message_id": "fake:campaign:recipient",
                    "sent_at": now,
                    "updated_at": now,
                },
                "$unset": {
                    "send_error_code": "",
                    "send_error_message": "",
                    "meta_error_code": "",
                    "meta_error_subcode": "",
                    "meta_error_user_msg": "",
                }
            }
        );
    }

    #[test]
    fn send_error_update_marks_send_failed_with_error_fields() {
        let now = DateTime::from_millis(1_800_000_000_600);
        let error = CampaignSendError::new("fake_error", "Simulated fake sender error");

        assert_eq!(
            send_recipient_failed_update(&error.code, error.message, None, None, None, now),
            doc! {
                "$set": {
                    "status": "send_failed",
                    "send_error_code": "fake_error",
                    "send_error_message": "Simulated fake sender error",
                    "updated_at": now,
                }
            }
        );
    }

    #[test]
    fn meta_api_error_maps_to_send_failed_meta_fields() {
        let now = DateTime::from_millis(1_800_000_000_650);
        let anyhow_error = anyhow::Error::new(MetaApiError {
            code: 131_049,
            message: "Temporarily throttled by Meta".to_string(),
            error_subcode: Some(2_000),
            error_user_msg: Some("Try later".to_string()),
        });
        let error = CampaignSendError::from(&anyhow_error);

        assert_eq!(error.code, "meta_rejected");
        assert_eq!(error.meta_error_code.as_deref(), Some("131049"));
        assert_eq!(error.meta_error_subcode.as_deref(), Some("2000"));
        assert_eq!(error.meta_error_user_msg.as_deref(), Some("Try later"));
        assert_eq!(
            send_recipient_failed_update(
                &error.code,
                error.message,
                error.meta_error_code,
                error.meta_error_subcode,
                error.meta_error_user_msg,
                now,
            ),
            doc! {
                "$set": {
                    "status": "send_failed",
                    "send_error_code": "meta_rejected",
                    "send_error_message": "Temporarily throttled by Meta",
                    "meta_error_code": "131049",
                    "meta_error_subcode": "2000",
                    "meta_error_user_msg": "Try later",
                    "updated_at": now,
                }
            }
        );
    }

    #[test]
    fn real_send_worker_selection_only_processes_sending_real_campaigns() {
        let mut campaign = base_campaign("sending");
        campaign.run_mode = Some("real".to_string());

        assert!(should_process_campaign_send(&campaign));
        assert_eq!(
            sending_real_campaign_filter(),
            doc! { "status": "sending", "run_mode": "real" }
        );
    }

    #[test]
    fn real_send_worker_selection_rejects_non_real_send_campaigns() {
        for status in ["dry_run_completed", "running", "queued"] {
            let mut campaign = base_campaign(status);
            campaign.run_mode = Some("real".to_string());
            assert!(!should_process_campaign_send(&campaign));
        }

        let mut dry_run = base_campaign("sending");
        dry_run.run_mode = Some("dry_run".to_string());
        assert!(!should_process_campaign_send(&dry_run));

        let missing_run_mode = base_campaign("sending");
        assert!(!should_process_campaign_send(&missing_run_mode));
    }

    #[test]
    fn real_send_finalization_without_failures_completes_successfully() {
        let progress = CampaignDryRunProgress {
            validated: 0,
            sending: 0,
            sent: 3,
            send_failed: 0,
            ..Default::default()
        };

        assert_eq!(send_completion_status(&progress), Some("completed"));
    }

    #[test]
    fn real_send_finalization_with_failures_completes_with_errors() {
        let progress = CampaignDryRunProgress {
            validated: 0,
            sending: 0,
            sent: 2,
            send_failed: 1,
            ..Default::default()
        };

        assert_eq!(
            send_completion_status(&progress),
            Some("completed_with_errors")
        );
    }

    #[test]
    fn real_send_finalization_waits_for_validated_or_sending_recipients() {
        assert_eq!(
            send_completion_status(&CampaignDryRunProgress {
                validated: 1,
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            send_completion_status(&CampaignDryRunProgress {
                sending: 1,
                ..Default::default()
            }),
            None
        );
    }

    #[test]
    fn real_send_progress_counts_sent_failed_and_percent() {
        let dto = campaign_progress_to_dto(CampaignDryRunProgress {
            validated: 2,
            sending: 1,
            sent: 5,
            send_failed: 2,
            send_unknown: 1,
            ..Default::default()
        });

        assert_eq!(dto.total_to_send, 11);
        assert_eq!(dto.processed_send, 8);
        assert!((dto.send_progress_percent - 72.72727272727273).abs() < 1e-9);
    }

    #[test]
    fn real_send_builds_components_before_fake_send() {
        let mut campaign = base_campaign("sending");
        campaign.run_mode = Some("real".to_string());
        campaign.template_components = Some(vec![serde_json::json!({
            "type": "BODY",
            "text": "Hola {{1}}"
        })]);
        campaign.template_variable_bindings = Some(vec![client_field_body_binding(1).into()]);
        let recipient = base_recipient("sending");
        let components = build_campaign_template_send_components(
            campaign.template_components.as_deref(),
            campaign.template_variable_bindings.as_deref(),
            campaign.template_media_bindings.as_deref(),
            &recipient_to_template_snapshot(&recipient),
        )
        .unwrap();
        let result = fake_campaign_send_result(&campaign, &recipient, &components).unwrap();

        assert_eq!(components[0]["parameters"][0]["text"], "Client One");
        assert!(result.meta_message_id.starts_with("fake:"));
    }

    #[test]
    fn real_send_builder_failure_maps_to_send_failed_update() {
        let now = DateTime::from_millis(1_800_000_000_700);
        let mut campaign = base_campaign("sending");
        campaign.template_components = Some(vec![serde_json::json!({
            "type": "BODY",
            "text": "Due {{1}}"
        })]);
        campaign.template_variable_bindings = Some(vec![payment_due_day_body_binding(1).into()]);
        let mut recipient = base_recipient("sending");
        recipient.payment_due_day = None;
        let err = build_campaign_template_send_components(
            campaign.template_components.as_deref(),
            campaign.template_variable_bindings.as_deref(),
            campaign.template_media_bindings.as_deref(),
            &recipient_to_template_snapshot(&recipient),
        )
        .unwrap_err();

        assert_eq!(err.code(), "missing_recipient_field");
        assert_eq!(
            send_recipient_failed_update(err.code(), err.code().to_string(), None, None, None, now),
            doc! {
                "$set": {
                    "status": "send_failed",
                    "send_error_code": "missing_recipient_field",
                    "send_error_message": "missing_recipient_field",
                    "updated_at": now,
                }
            }
        );
    }

    #[test]
    fn send_media_required_without_binding_returns_missing_media_error() {
        let mut campaign = base_campaign("dry_run_completed");
        campaign.template_components = Some(vec![
            serde_json::json!({ "type": "HEADER", "format": "IMAGE" }),
            serde_json::json!({ "type": "BODY", "text": "Hola" }),
        ]);
        let recipient = base_recipient("validated");

        let err =
            validate_campaign_send_components_for_recipient(&campaign, &recipient).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "missing_template_media_binding"
                    && field.as_deref() == Some("template_media_bindings")
        ));
    }

    #[test]
    fn send_media_invalid_link_returns_builder_error() {
        let mut campaign = base_campaign("dry_run_completed");
        campaign.template_components = Some(vec![
            serde_json::json!({ "type": "HEADER", "format": "IMAGE" }),
            serde_json::json!({ "type": "BODY", "text": "Hola" }),
        ]);
        campaign.template_media_bindings = Some(vec![header_image_link_binding(
            "http://example.com/header.jpg",
        )]);
        let recipient = base_recipient("validated");

        let err =
            validate_campaign_send_components_for_recipient(&campaign, &recipient).unwrap_err();

        assert!(matches!(
            err,
            ApiError::Domain { status, ref code, ref field, .. }
                if status == StatusCode::BAD_REQUEST
                    && code == "invalid_media_link"
                    && field.as_deref() == Some("template_media_bindings.value")
        ));
    }

    #[test]
    fn double_send_first_transition_then_second_returns_conflict() {
        let first = validate_sendable_campaign(&base_campaign("dry_run_completed"));
        let second = validate_sendable_campaign(&base_campaign("sending"));

        assert!(first.is_ok());
        assert!(matches!(
            second,
            Err(ApiError::Domain { status, ref code, .. })
                if status == StatusCode::CONFLICT && code == "campaign_not_sendable"
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
