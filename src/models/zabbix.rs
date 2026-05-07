use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct ZabbixTrafficResponse {
    pub client_zabbix_code: String,
    pub olt_name: String,
    pub total_download_gb: f64,
    pub total_upload_gb: f64,
    pub history: Vec<MonthlyTraffic>,
}

#[derive(Serialize, ToSchema)]
pub struct MonthlyTraffic {
    pub year: i32,
    pub month: u32,
    pub download_gb: f64,
    pub upload_gb: f64,
}

#[allow(dead_code)]
#[derive(Deserialize)]
pub struct ClientOltData {
    client_zabbix_code: String, // O "sn", como lo tengas en tu DB
    olt_zabbix_name: String,    // O "olt_s_zabbix_name"
}

#[derive(Deserialize)]
pub struct ZabbixLookupResult {
    #[serde(rename = "nPon")]
    pub n_pon: i32,
    #[serde(rename = "nIdOnu")]
    pub n_id_onu: i32,
    #[serde(rename = "sNameZabbix")]
    pub s_name_zabbix: String,
}
