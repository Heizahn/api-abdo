use mongodb::bson::{oid::ObjectId, DateTime};
use serde::Serialize;

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

/// Estructura interna para procesar deudas con sus pagos
#[derive(Debug)]
pub struct DebtWithPayments {
    pub debt_id: ObjectId,
    pub id_owner: ObjectId,
    pub reason: String,
    pub state: String,
    pub created_at: String,
    pub original_amount_usd: f64,
    pub part_payments: Vec<PartPaymentWithStatus>,
}

#[derive(Debug)]
pub struct PartPaymentWithStatus {
    pub payment_id: ObjectId,
    pub amount_usd: f64,
    pub amount_bs: f64,
    pub status: String,
}
