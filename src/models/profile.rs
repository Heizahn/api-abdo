use crate::db::mongo::ResultGroupedByDate;
use serde::Serialize;

// ============================================
// CLIENT SUMMARY (GET /v1/profile/me/clients)
// ============================================
#[derive(Debug, Serialize)]
pub struct ClientData {
    pub id: String,
    pub name: String,
    pub phone: String,
    pub id_tax: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClientSummary {
    pub client: ClientData,
    pub balance_ves: f64,
    pub last_payments: Vec<ResultGroupedByDate>,
}

// Estructura de respuesta principal
#[derive(Debug, Serialize)]
pub struct MeGroupResponse {
    pub ok: bool,
    pub clients: Vec<ClientSummary>,
}

#[derive(Debug, Serialize)]
pub struct MePhoneResponse {
    pub ok: bool,
    pub phone: String,
}
