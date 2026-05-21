use mongodb::bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================
// Database Models
// ============================================
// These are used internally by database queries but not directly constructed
// in handlers (yet). They may be used in future endpoints.

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Client {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    #[serde(rename = "sPhone")]
    pub s_phone: String,
    #[serde(rename = "idTax", skip_serializing_if = "Option::is_none")]
    pub id_tax: Option<ObjectId>,
    #[serde(rename = "nBalance", default)]
    pub n_balance: f64,
    #[serde(rename = "sState", default)]
    pub s_state: String,
}

#[derive(Debug, Clone)]
pub struct ActiveClientBalance {
    pub id: ObjectId,
    pub n_balance: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LatestPayment {
    pub id: String,
    pub created_at: String,
    pub reason: String,
    pub state: String,
    pub amount: f64,
    pub amount_bs: f64,
    pub client_name: String,
    pub creator_name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SolvencyCounts {
    pub solventes: u32,
    pub morosos: u32,
    pub suspendidos: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Debt {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    #[serde(rename = "nAmount")]
    pub n_amount: f64,
    #[serde(rename = "sState")]
    pub s_state: String,
    #[serde(rename = "idClient")]
    pub id_client: ObjectId,
    #[serde(rename = "sReason")]
    pub s_reason: String,
    #[serde(rename = "dCreation")]
    pub d_creation: DateTime,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PartPayment {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    #[serde(rename = "idDebt")]
    pub id_debt: ObjectId,
    #[serde(rename = "idPayment")]
    pub id_payment: ObjectId,
    #[serde(rename = "nAmount")]
    pub n_amount: f64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Payment {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    #[serde(rename = "nAmount")]
    pub n_amount: f64,
    #[serde(rename = "sState")]
    pub s_state: String,
    #[serde(rename = "nBs")]
    pub n_bs: f64,
    /// Razón descriptiva del pago (calculada por `calculate_payment_reason`).
    #[serde(rename = "sReason", default)]
    pub s_reason: Option<String>,
    /// Cliente al que pertenece el pago.
    #[serde(rename = "idClient", default)]
    pub id_client: Option<ObjectId>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Clone)]
pub struct ActiveDebtResponse {
    #[serde(flatten)]
    pub debt: Debt,
    pub active_debt_amount: f64,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct PingResponse {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Tax {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    #[serde(rename = "sTarget")]
    pub target: String,
    #[serde(rename = "IVA")]
    pub iva: f64,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct LatestVersionResponse {
    pub ok: bool,
    pub data: LatestVersion,
}

#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct LatestVersion {
    pub latest_version_code: i32,
    pub update_url: String,
}

// ============================================
// ONU get DB
// ============================================
#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OnuIdentity {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    #[serde(rename = "sSn")]
    pub sn: String,
    #[serde(rename = "sMac")]
    pub mac: Option<String>,
    #[serde(rename = "nMotherboard")]
    pub motherboard: Option<i32>,
    #[serde(rename = "nPon")]
    pub pon: Option<i32>,
    #[serde(rename = "nIdOnu")]
    pub id_onu: Option<i32>,
    #[serde(rename = "idOlt")]
    pub id_olt: Option<ObjectId>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OnuForUpdateIp {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    #[serde(rename = "sMac")]
    pub mac: String,
    #[serde(rename = "sIp")]
    pub ip: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OnuIpUpdate {
    pub id: ObjectId,
    pub new_ip: String,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct BcvResponse {
    pub bcv: f64,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct ClientOnu {
    pub id: String,
    pub sn: Option<String>,
    pub mac: Option<String>,
    pub ip: Option<String>,
    pub motherboard: Option<i32>,
    pub pon: Option<i32>,
    pub id_onu: Option<i32>,
    pub olt_id: Option<String>,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct ClientDetail {
    pub id: String,
    pub name: String,
    pub dni: Option<String>,
    pub phone: String,
    pub email: Option<String>,
    pub status: String,
    pub balance: f64,
    pub ip: Option<String>,
    pub ip_pppoe: Option<String>,
    pub sn: Option<String>,
    pub mac: Option<String>,
    pub client_type: Option<String>,
    pub payment: Option<f64>,
    pub address: Option<String>,
    pub gps: Option<String>,
    pub commentary: Option<String>,
    pub subscription_id: Option<String>,
    pub sector_id: Option<String>,
    pub owner_id: Option<String>,
    pub tax_id: Option<String>,
    pub is_suspendable: Option<bool>,
    pub check: Option<bool>,
    pub created_at: Option<String>,
    pub suspended_at: Option<String>,
    pub updated_at: Option<String>,
    pub installed_at: Option<String>,
    pub plan_name: Option<String>,
    pub plan_price: Option<f64>,
    pub plan_mbps: Option<f64>,
    pub sector_name: Option<String>,
    pub provider_tag: Option<i32>,
    pub creator: Option<String>,
    pub editor: Option<String>,
    pub installer: Option<String>,
    pub suspender: Option<String>,
    pub onu: Option<ClientOnu>,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct ClientListItem {
    pub id: String,
    pub name: String,
    pub dni: Option<String>,
    pub status: String,
    pub balance: f64,
    pub sector_name: Option<String>,
    pub plan_name: Option<String>,
    pub plan_price: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ClientStatusHistoryItem {
    pub id: String,
    pub client_id: String,
    pub state: String,
    pub previous_state: String,
    pub actor_name: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct CustomerInfoItem {
    pub id: String,
    pub razon_social: String,
    pub dni: Option<String>,
    pub direccion: Option<String>,
    pub email: Option<String>,
    pub telefono: Option<String>,
}

// ============================================
// PaymentReport view models (realtime-pending-badges)
// ============================================

/// Row item returned by `list_payment_reports` (GET /v1/auth-user/payments-reports).
/// Includes all PaymentReport doc fields + denormalized client and editor names.
#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct PaymentReportListItem {
    pub id: String,
    pub id_client: Option<String>,
    pub id_payment_method: Option<String>,
    pub id_debt: Option<String>,
    pub reference: String,
    pub payment_date: String,
    pub amount_bs: f64,
    pub bank_origin: String,
    pub phone_number: String,
    pub image_url: String,
    pub amount_usd: f64,
    pub exchange_rate: f64,
    pub state: String,
    pub rejection_reason: Option<String>,
    pub id_creator: Option<String>,
    pub id_editor: Option<String>,
    pub id_payment: Option<String>,
    pub id_issuing_bank: Option<String>,
    pub created_at: String,
    // Denormalized
    pub client_name: Option<String>,
    pub editor_name: Option<String>,
}

/// Full PaymentReport doc returned by `find_report_by_id` (used by approve/reject handlers).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PaymentReportFull {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    #[serde(rename = "idClient")]
    pub id_client: Option<ObjectId>,
    #[serde(rename = "idPaymentMethod")]
    pub id_payment_method: Option<ObjectId>,
    #[serde(rename = "idDebt")]
    pub id_debt: Option<ObjectId>,
    #[serde(rename = "sReference")]
    pub reference: String,
    #[serde(rename = "dPaymentDate")]
    pub payment_date: String,
    #[serde(rename = "nBs")]
    pub amount_bs: f64,
    #[serde(rename = "sBank")]
    pub bank_origin: String,
    #[serde(rename = "sPhone")]
    pub phone_number: String,
    #[serde(rename = "sImageUrl")]
    pub image_url: String,
    #[serde(rename = "nAmountUSD")]
    pub amount_usd: f64,
    #[serde(rename = "nExchangeRate")]
    pub exchange_rate: f64,
    #[serde(rename = "sState")]
    pub state: String,
    #[serde(rename = "sRejectionReason", default)]
    pub rejection_reason: Option<String>,
    #[serde(rename = "idCreator", default)]
    pub id_creator: Option<String>,
    #[serde(rename = "idEditor", default)]
    pub id_editor: Option<String>,
    #[serde(rename = "idPayment", default)]
    pub id_payment: Option<ObjectId>,
    #[serde(rename = "idIssuingBank", default)]
    pub id_issuing_bank: Option<ObjectId>,
    #[serde(rename = "dCreation")]
    pub created_at: String,
}

/// Projection used by `find_payments_for_match_by_client`:
/// only { _id, sReference, idPaymentReport, idPaymentMethod }.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PaymentForMatch {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    #[serde(rename = "sReference")]
    pub s_reference: String,
    #[serde(rename = "idPaymentReport", default)]
    pub id_payment_report: Option<ObjectId>,
    #[serde(rename = "idPaymentMethod", default)]
    pub id_payment_method: Option<ObjectId>,
}

/// PartPayment joined with the linked Payment's sState.
/// Returned by `find_part_payments_by_debt`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PartPaymentWithPaymentState {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    #[serde(rename = "idDebt")]
    pub id_debt: ObjectId,
    #[serde(rename = "idPayment")]
    pub id_payment: ObjectId,
    #[serde(rename = "nAmount")]
    pub n_amount: f64,
    /// `sState` from the linked Payment doc.
    pub payment_state: String,
}

/// Returned by `PaymentsService::create_payment`.
#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct PaymentCreateResult {
    pub payment_id: String,
}
