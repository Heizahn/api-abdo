use serde::Serialize;
use crate::db::mongo::{ResultGroupedByDate};

// ============================================
// ME (GET /v1/profile/me)
// ============================================

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub ok: bool,
    pub customer: CustomerData,
}

#[derive(Debug, Serialize)]
pub struct CustomerData {
    pub name: String,
    pub phone: String,
}

// ============================================
// BALANCE (GET /v1/profile/me/balance)
// ============================================

#[derive(Debug, Serialize)]
pub struct BalanceResponse {
    pub ok: bool,
    pub balance_ves: f64,
}

// ============================================
// LAST PAYMENTS (GET /v1/profile/me/last_payments)
// ============================================

#[derive(Debug, Serialize)]
pub struct LastPaymentsResponse {
    pub ok: bool,
    pub data: Vec<ResultGroupedByDate>,
}
