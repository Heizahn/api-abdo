use mongodb::bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

// ============================================
// Database Models
// ============================================
// These are used internally by database queries but not directly constructed
// in handlers (yet). They may be used in future endpoints.

#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    pub s_phone: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Debt {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    pub n_amount: f64,
    pub s_state: String,
    pub id_client: ObjectId,
    pub s_reason: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PartPayment {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    pub id_debt: ObjectId,
    pub id_payment: ObjectId,
    pub n_amount: f64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Payment {
    #[serde(rename = "_id")]
    pub _id: ObjectId,
    pub n_amount: f64,
    pub s_state: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Clone)]
pub struct ActiveDebtResponse {
    #[serde(flatten)]
    pub debt: Debt,
    pub active_debt_amount: f64,
}
