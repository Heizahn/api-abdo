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

#[derive(Debug, Serialize, Deserialize)]

pub struct OnuResponse {
    pub ok: bool,
    pub data: Vec<Onu>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OnuCreate {
    #[serde(rename = "sSn")]
    pub sn: String,
    #[serde(rename = "idCreator")]
    pub id_creator: String,
    #[serde(rename = "dCreation")]
    pub d_creation: String,
}
