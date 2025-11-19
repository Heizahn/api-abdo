use mongodb::bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PaymentMethod {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    #[serde(rename = "sBankName")]
    pub bank_name: String,
    #[serde(rename = "sPhone")]
    pub phone: String,
    #[serde(rename = "sIdNumber")]
    pub id_number: String,
    #[serde(rename = "sAccountName")]
    pub account_name: String,
    #[serde(rename = "bActive")]
    pub is_active: bool,
}

#[derive(Serialize)]
pub struct PaymentMethodResponse {
    pub ok: bool,
    pub data: Option<PagoMovilData>,
}

#[derive(Serialize, Deserialize)]
pub struct PagoMovilData {
    pub bank_name: String,
    pub id_number: String,
    pub phone: String,
}

#[derive(Debug, Deserialize)]
pub struct ClientOwner {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    #[serde(rename = "idOwner")]
    pub id_owner: String,
}

#[derive(Debug, Deserialize)]
pub struct UserPaymentInfo {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "idPaymentMethod")]
    pub id_payment_method: Option<ObjectId>,
}