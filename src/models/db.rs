use mongodb::bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize)]
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
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Clone)]
pub struct ActiveDebtResponse {
    #[serde(flatten)]
    pub debt: Debt,
    pub active_debt_amount: f64,
}

#[derive(Debug, Serialize, Clone)]
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

#[derive(Debug, Serialize, Clone)]
pub struct LatestVersionResponse {
    pub ok: bool,
    pub data: LatestVersion,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LatestVersion {
    pub latest_version_code: i32,
    pub update_url: String,
}

// ============================================
// ONU get DB
// ============================================
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

#[derive(Debug, Serialize, Clone)]
pub struct BcvResponse {
    pub bcv: f64,
}
