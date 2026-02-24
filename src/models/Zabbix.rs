use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct ZabbixTrafficResponse {
    pub client_sn: String,
    pub olt_name: String,
    pub total_download_gb: f64,
    pub total_upload_gb: f64,
    pub history: Vec<MonthlyTraffic>,
}

#[derive(Serialize)]
pub struct MonthlyTraffic {
    pub year: i32,
    pub month: u32,
    pub download_gb: f64,
    pub upload_gb: f64,
}