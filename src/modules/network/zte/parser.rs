#![allow(dead_code)]

use crate::models::db::OnuIdentity;
use anyhow::{Context, Result};
use mongodb::bson::oid::ObjectId;
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct OnuDetected {
    pub id: ObjectId,
    pub mac: String,
    pub motherboard: u32,
    pub pon: u32,
    pub id_onu: u32,
    pub id_olt: ObjectId,
}

pub fn parse_zte_report(file_path: &str, db_sn_list: &[OnuIdentity]) -> Result<Vec<OnuDetected>> {
    let db_map: HashMap<&str, &OnuIdentity> = db_sn_list
        .iter()
        .map(|identity| (identity.sn.as_str(), identity))
        .collect();

    let current_olt_str = "697228928ace1d49d5c64192";
    let current_olt_id = ObjectId::from_str(current_olt_str).unwrap_or_default();

    let content = fs::read_to_string(file_path)
        .with_context(|| format!("No se pudo leer el archivo de reporte: {}", file_path))?;

    let mut actualizaciones = Vec::new();
    let re_mac = Regex::new(r"^([0-9A-Fa-f]{2}[:-]){5}([0-9A-Fa-f]{2})$").unwrap();

    for line in content.lines() {
        if line.starts_with("Fecha")
            || line.starts_with("---")
            || line.starts_with("====")
            || line.trim().is_empty()
        {
            continue;
        }

        let parts: Vec<&str> = line.split('|').map(|s| s.trim()).collect();
        if parts.len() < 6 {
            continue;
        }

        let interfaz_raw = parts[2];
        let sn = parts[3];
        let estado = parts[4];
        let mac_file = parts[5];

        if !estado.eq_ignore_ascii_case("ready") {
            continue;
        }
        if !re_mac.is_match(mac_file) {
            continue;
        }

        if let Some(db_record) = db_map.get(sn) {
            let clean_interface = interfaz_raw.replace("gpon_onu-", "");
            let interface_parts: Vec<&str> = clean_interface.split('/').collect();

            if interface_parts.len() >= 3 {
                if let Ok(moth_u32) = interface_parts[1].parse::<u32>() {
                    let pon_id_parts: Vec<&str> = interface_parts[2].split(':').collect();
                    if pon_id_parts.len() == 2 {
                        let pon_u32 = pon_id_parts[0].parse::<u32>().unwrap_or(0);
                        let id_onu_u32 = pon_id_parts[1].parse::<u32>().unwrap_or(0);

                        if moth_u32 > 0 && pon_u32 > 0 && id_onu_u32 > 0 {
                            let file_moth_i32 = moth_u32 as i32;
                            let file_pon_i32 = pon_u32 as i32;
                            let file_id_onu_i32 = id_onu_u32 as i32;

                            let mac_changed = match &db_record.mac {
                                Some(db_mac) => !db_mac.eq_ignore_ascii_case(mac_file),
                                None => true,
                            };

                            let moth_changed = db_record.motherboard != Some(file_moth_i32);
                            let pon_changed = db_record.pon != Some(file_pon_i32);
                            let id_onu_changed = db_record.id_onu != Some(file_id_onu_i32);
                            let olt_changed = db_record.id_olt != Some(current_olt_id);

                            if mac_changed
                                || moth_changed
                                || pon_changed
                                || id_onu_changed
                                || olt_changed
                            {
                                actualizaciones.push(OnuDetected {
                                    id: db_record.id,
                                    mac: mac_file.to_string(),
                                    motherboard: moth_u32,
                                    pon: pon_u32,
                                    id_onu: id_onu_u32,
                                    id_olt: current_olt_id,
                                });
                            }
                        }
                    }
                }
            }
        } else {
            continue;
        }
    }

    Ok(actualizaciones)
}
