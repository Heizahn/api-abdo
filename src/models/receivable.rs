use serde::Serialize;
use utoipa::ToSchema;

// ============================================
// RECEIVABLES (GET /v1/receivable/me)
// ============================================

#[derive(Serialize, ToSchema)]
pub struct ReceivablesResponse {
    pub ok: bool,
    pub receivables: Vec<ReceivableData>,
}

#[derive(Serialize, ToSchema)]
pub struct ReceivableByIdResponse {
    pub ok: bool,
    pub receivable: ReceivableData,
}

#[derive(Serialize, ToSchema)]
pub struct ReceivableData {
    pub debt_id: String,
    pub id_owner: String, // El cliente dueño de la deuda
    pub reason: String,
    pub state: String,
    pub created_at: String,
    pub total_amount_usd: f64, // Monto original de la deuda
    pub pending_amount_usd: f64,
    pub pending_amount_bs: f64, // Calculado con IVA específico
    pub has_pending_payments: bool,
    pub payments: Option<Vec<PaymentData>>,
}

#[derive(Serialize, ToSchema)]
pub struct PaymentData {
    pub payment_id: String,
    pub amount_usd: f64,
    pub amount_bs: f64,
    pub status: String, // "Activo", "Pendiente", "Rechazado"
    pub reference: Option<String>,
    pub is_report: bool, // true si viene de PaymentReport, false si es un Payment procesado
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RejectedPayment {
    pub payment_id: String,
    pub amount_usd: f64,
    pub amount_bs: f64,
    pub reference: String,
    pub rejected_at: String,
    pub rejection_reason: String,
}

#[derive(Serialize, ToSchema)]
pub struct RejectedPaymentsResponse {
    pub ok: bool,
    pub payments: Vec<RejectedPayment>,
}
