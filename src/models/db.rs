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
