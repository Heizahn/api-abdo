
use serde::{ Serialize};

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
