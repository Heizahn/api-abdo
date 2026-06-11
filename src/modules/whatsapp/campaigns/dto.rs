use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq)]
pub struct CampaignPreviewRequest {
    #[serde(default)]
    pub provider_ids: Option<Vec<String>>,
    #[serde(default)]
    pub sector_ids: Option<Vec<String>>,
    #[serde(default)]
    pub balance_filter: Option<BalanceFilter>,
    #[serde(default)]
    pub client_state: Option<ClientStateFilter>,
    #[serde(default)]
    pub include_all_active: Option<bool>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ClientStateFilter {
    Active,
    Suspended,
    Retired,
    Moroso,
    Solvente,
    Any,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq)]
pub struct BalanceRange {
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq)]
pub struct BalanceFilter {
    #[serde(default)]
    pub lt: Option<f64>,
    #[serde(default)]
    pub lte: Option<f64>,
    #[serde(default)]
    pub gt: Option<f64>,
    #[serde(default)]
    pub gte: Option<f64>,
    #[serde(default)]
    pub eq: Option<f64>,
    #[serde(default)]
    pub between: Option<BalanceRange>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignPreviewResponse {
    pub ok: bool,
    pub totals: CampaignPreviewTotals,
    pub recipients: Vec<CampaignPreviewRecipient>,
    pub page: u32,
    pub per_page: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema, Default)]
pub struct CampaignPreviewTotals {
    pub matched: usize,
    pub can_send: usize,
    pub invalid_phone: usize,
    pub duplicated_phone: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhoneStatus {
    Valid,
    Invalid,
    Duplicated,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DerivedClientState {
    Moroso,
    Solvente,
    Suspended,
    Retired,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CampaignPreviewRecipient {
    pub client_id: String,
    pub name: String,
    pub phone_original: String,
    pub phone_normalized: Option<String>,
    pub phone_status: PhoneStatus,
    pub can_send: bool,
    pub reason: Option<String>,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub provider_tag: Option<String>,
    pub sector_id: Option<String>,
    pub sector_name: Option<String>,
    pub client_state_raw: String,
    pub client_state_derived: DerivedClientState,
    pub customer_status_raw: String,
    pub customer_status_derived: DerivedClientState,
    pub balance: f64,
    pub payment_due_day: Option<i32>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateCampaignRequest {
    pub name: String,
    #[serde(default)]
    pub phone_number_id: Option<String>,
    pub template_name: String,
    pub template_language: String,
    #[serde(default)]
    #[schema(value_type = Option<Vec<Object>>)]
    pub template_components: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub template_variable_bindings: Option<Vec<TemplateVariableBinding>>,
    #[serde(default)]
    pub template_media_bindings: Option<Vec<TemplateMediaBinding>>,
    pub filters: CampaignPreviewRequest,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateCampaignRequest {
    pub name: String,
    #[serde(default)]
    pub phone_number_id: Option<String>,
    pub template_name: String,
    pub template_language: String,
    #[serde(default)]
    #[schema(value_type = Option<Vec<Object>>)]
    pub template_components: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub template_variable_bindings: Option<Vec<TemplateVariableBinding>>,
    #[serde(default)]
    pub template_media_bindings: Option<Vec<TemplateMediaBinding>>,
    pub filters: CampaignPreviewRequest,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UpdateCampaignResponse {
    pub ok: bool,
    pub data: CampaignSummary,
    pub snapshot_regenerated: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TemplateVariableComponent {
    Body,
    Header,
    Button,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TemplateVariableSource {
    Static,
    ClientField,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TemplateClientField {
    ClientName,
    Balance,
    PaymentDueDay,
    SectorName,
    CustomerStatusDerived,
    PhoneNormalized,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
pub struct TemplateVariableBinding {
    pub component: TemplateVariableComponent,
    pub index: i32,
    pub placeholder: String,
    pub source: TemplateVariableSource,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub client_field: Option<TemplateClientField>,
    #[serde(default)]
    pub button_index: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TemplateMediaComponent {
    Header,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TemplateMediaType {
    Image,
    Video,
    Document,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TemplateMediaSource {
    Link,
    MediaId,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
pub struct TemplateMediaBinding {
    pub component: TemplateMediaComponent,
    pub media_type: TemplateMediaType,
    pub source: TemplateMediaSource,
    pub value: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CampaignRecipientsQuery {
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CampaignListQuery {
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub created_from: Option<String>,
    #[serde(default)]
    pub created_to: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignListResponse {
    pub ok: bool,
    pub page: u32,
    pub limit: u32,
    pub total: u64,
    pub total_pages: u64,
    pub campaigns: Vec<CampaignListItem>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignListItem {
    pub id: String,
    pub name: String,
    pub phone_number_id: Option<String>,
    pub template_name: String,
    pub template_language: String,
    pub has_template_variables: bool,
    pub template_variables_count: usize,
    pub has_template_media: bool,
    pub template_media_count: usize,
    pub status: String,
    pub run_mode: Option<String>,
    pub dry_run_completed_at: Option<String>,
    pub total_recipients: u64,
    pub total_can_send: u64,
    pub total_invalid_phone: u64,
    pub total_duplicated_phone: u64,
    pub total_excluded: u64,
    pub total_effective_can_send: u64,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateCampaignRecipientExclusionsRequest {
    pub recipient_ids: Vec<String>,
    pub excluded: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UpdateCampaignRecipientExclusionsResponse {
    pub ok: bool,
    pub data: UpdateCampaignRecipientExclusionsData,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UpdateCampaignRecipientExclusionsData {
    pub campaign_id: String,
    pub requested: u64,
    pub updated: u64,
    pub total_excluded: u64,
    pub total_can_send: u64,
    pub total_effective_can_send: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignSummaryResponse {
    pub ok: bool,
    pub data: CampaignSummary,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignSummary {
    pub id: String,
    pub name: String,
    pub phone_number_id: Option<String>,
    pub template_name: String,
    pub template_language: String,
    #[schema(value_type = Option<Vec<Object>>)]
    pub template_components: Option<Vec<serde_json::Value>>,
    pub template_variable_bindings: Option<Vec<TemplateVariableBinding>>,
    pub template_media_bindings: Option<Vec<TemplateMediaBinding>>,
    pub filters: CampaignPreviewRequest,
    pub status: String,
    pub started_by: Option<String>,
    pub started_at: Option<String>,
    pub run_mode: Option<String>,
    pub dry_run_completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<CampaignProgress>,
    pub total_recipients: u64,
    pub total_can_send: u64,
    pub total_invalid_phone: u64,
    pub total_duplicated_phone: u64,
    pub total_excluded: u64,
    pub total_effective_can_send: u64,
    pub created_by: String,
    pub confirmed_by: Option<String>,
    pub confirmed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, ToSchema, PartialEq)]
pub struct CampaignProgress {
    pub pending: u64,
    pub sending: u64,
    pub validated: u64,
    pub failed: u64,
    pub invalid_phone: u64,
    pub duplicated_phone: u64,
    pub excluded: u64,
    pub total_effective: u64,
    pub processed: u64,
    pub progress_percent: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignRecipientsResponse {
    pub ok: bool,
    pub data: Vec<CampaignRecipientItem>,
    pub page: u32,
    pub per_page: u32,
    pub total: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignRecipientItem {
    pub id: String,
    pub campaign_id: String,
    pub client_id: String,
    pub client_name: String,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub sector_id: Option<String>,
    pub sector_name: Option<String>,
    pub customer_status_raw: String,
    pub customer_status_derived: DerivedClientState,
    pub client_state_raw: String,
    pub client_state_derived: DerivedClientState,
    pub balance: f64,
    pub payment_due_day: Option<i32>,
    pub phone_original: String,
    pub phone_normalized: Option<String>,
    pub phone_status: PhoneStatus,
    pub can_send: bool,
    pub reason: Option<String>,
    pub excluded: bool,
    pub status: String,
    pub attempts: i64,
    pub last_attempt_at: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub validated_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
