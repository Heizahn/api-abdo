use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
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

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientStateFilter {
    Active,
    Suspended,
    Moroso,
    Solvente,
    Any,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BalanceRange {
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Deserialize, ToSchema)]
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

#[derive(Debug, Serialize, ToSchema, Default)]
pub struct CampaignPreviewTotals {
    pub matched: usize,
    pub can_send: usize,
    pub invalid_phone: usize,
    pub duplicated_phone: usize,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhoneStatus {
    Valid,
    Invalid,
    Duplicated,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DerivedClientState {
    Moroso,
    Solvente,
    Suspended,
}

#[derive(Debug, Serialize, ToSchema)]
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
    pub balance: f64,
}
