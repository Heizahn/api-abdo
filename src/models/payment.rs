use chrono::{DateTime, Utc};
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

fn serialize_oid_as_string<S>(oid: &ObjectId, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&oid.to_hex())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Bank {
    #[serde(rename(deserialize = "_id", serialize = "id"))]
    #[serde(serialize_with = "serialize_oid_as_string")]
    pub id: ObjectId,
    pub bank_code: String,
    pub bank_name: String,
}

#[derive(Debug, Serialize)]
pub struct BankListResponse {
    pub ok: bool,
    pub data: Vec<Bank>,
}

#[derive(Debug, Deserialize)]
pub struct ClientOwner {
    #[serde(rename = "idOwner")]
    pub id_owner: String,
}

#[derive(Debug, Deserialize)]
pub struct UserPaymentInfo {
    #[serde(rename = "idPaymentMethod")]
    pub id_payment_method: Option<ObjectId>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentReport {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,

    #[serde(rename = "idClient")]
    pub id_client: Option<ObjectId>,

    #[serde(rename = "idPaymentMethod")]
    pub id_payment_method: Option<ObjectId>,

    #[serde(rename = "idDebt")]
    pub id_debt: Option<ObjectId>,

    // Datos ingresados por el usuario
    #[serde(rename = "sReference")]
    pub reference: String,

    #[serde(rename = "dPaymentDate")]
    pub payment_date: DateTime<Utc>, // Fecha indicada por el usuario

    #[serde(rename = "nBs")]
    pub amount_bs: f64,

    #[serde(rename = "sBank")]
    pub bank_origin: String,

    #[serde(rename = "sPhone")]
    pub phone_number: String,

    #[serde(rename = "sImageUrl")]
    pub image_url: String, // Ruta relativa ej: "/uploads/foto.jpg"

    // Datos calculados por el sistema
    #[serde(rename = "nAmountUSD")]
    pub amount_usd: f64,

    #[serde(rename = "nExchangeRate")]
    pub exchange_rate: f64,

    #[serde(rename = "sState")]
    pub state: String, // "Pendiente", "Aprobado", "Rechazado"

    #[serde(rename = "dCreation")]
    pub created_at: DateTime<Utc>,
}
