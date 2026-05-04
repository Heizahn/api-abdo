use chrono::{DateTime, Utc};
use mongodb::bson::oid::ObjectId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================
// Check Reference
// ============================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct CheckReferenceRequest {
    #[serde(rename = "idClient")]
    pub id_client: String,
    #[serde(rename = "sReference")]
    pub s_reference: String,
}

/// Resultado de la búsqueda de referencia en DB (retornado por el repositorio)
pub struct ReferenceMatchInfo {
    pub source: String,          // "payments" | "payment_reports"
    pub is_same_client: bool,
    pub s_name: Option<String>,  // nombre del cliente si es diferente
    pub s_reference: String,     // la referencia que coincidió en DB
    pub n_amount: f64,
    pub n_bs: f64,
    pub s_state: String,
}

#[derive(Serialize, ToSchema)]
pub struct CheckReferenceResponse {
    pub ok: bool,
    pub message: String,
    pub data: CheckReferenceData,
}

#[derive(Serialize, ToSchema)]
pub struct CheckReferenceData {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<ReferenceDetails>,
}

#[derive(Serialize, ToSchema)]
pub struct ReferenceDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "sName")]
    pub s_name: Option<String>,
    #[serde(rename = "sReference")]
    pub s_reference: String,
    #[serde(rename = "nAmount")]
    pub n_amount: f64,
    #[serde(rename = "nBs")]
    pub n_bs: f64,
    #[serde(rename = "sState")]
    pub s_state: String,
}

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

#[derive(Serialize, ToSchema)]
pub struct PaymentMethodResponse {
    pub ok: bool,
    pub data: Option<PagoMovilData>,
}

#[derive(Serialize, ToSchema)]
pub struct PagoMovilData {
    pub id: String,
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

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Bank {
    #[serde(rename(deserialize = "_id", serialize = "id"))]
    #[serde(serialize_with = "serialize_oid_as_string")]
    #[schema(value_type = String, example = "65a7f8d9c3e2a1b4d6f8e0c5")]
    pub id: ObjectId,
    pub bank_code: String,
    pub bank_name: String,
}

#[derive(Debug, Serialize, ToSchema)]
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

    #[serde(rename = "sRejectionReason", default)]
    pub rejection_reason: Option<String>,

    #[serde(rename = "idCreator", skip_serializing_if = "Option::is_none", default)]
    pub id_creator: Option<String>,

    #[serde(rename = "dCreation")]
    pub created_at: DateTime<Utc>,
}
