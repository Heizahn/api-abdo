use mongodb::bson::{oid::ObjectId};
use serde::{Deserialize, Serialize};

// ============================================
// RECEIVABLES (GET /v1/receivable/me)
// ============================================

#[derive(Debug, Serialize)]
pub struct ReceivablesResponse {
    pub ok: bool,
    pub receivables: Vec<ReceivableData>,
}

#[derive(Debug, Serialize)]
pub struct ReceivableData {
    pub debt_id: String,
    pub id_owner: String,
    pub reason: String,
    pub state: String,
    pub created_at: String,
    pub pending_amount_bs: f64,
    pub has_pending_payments: bool,
    pub payments: Vec<PaymentData>,
}

#[derive(Debug, Serialize)]
pub struct PaymentData {
    pub payment_id: String,
    pub amount_bs: f64,
    pub status: String,
}

// ============================================
// INTERNAL DATA STRUCTURES
// ============================================

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentMethod {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    #[serde(rename = "nTag")]
    pub n_tag: i32,
    #[serde(rename = "sBankName")]
    pub bank_name: String,
    #[serde(rename = "sPhone")]
    pub phone: String,
    #[serde(rename = "sIdNumber")]
    pub id_number: String,
    #[serde(rename = "sAccountName")]
    pub account_name: String,
    pub is_active: bool,
}

// #[derive(Debug, Deserialize)]
// pub struct ClientOwner {
//     #[serde(rename = "_id")]
//     pub id: ObjectId,
//     #[serde(rename = "idOwner")]
//     pub id_owner: String,
// }
//
// #[derive(Debug, Deserialize)]
// pub struct UserTag {
//     #[serde(rename = "_id")]
//     pub id: String,
//     #[serde(rename = "nTag")]
//     pub n_tag: i32,
// }