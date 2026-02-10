use serde::{Deserialize, Serialize};

// ============================================
// ONU get DB
// ============================================
#[derive(Debug, Serialize, Deserialize)]
pub struct Onu {
    pub cliente: String,
    pub sn: String,
    pub mac: Option<String>,
    pub ip: Option<String>,
    pub olt_name: Option<String>,
    pub motherboard: Option<i32>,
    pub pon: Option<i32>,
    pub id_onu: Option<i32>,
}
