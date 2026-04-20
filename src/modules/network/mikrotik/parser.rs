use crate::models::db::{OnuForUpdateIp, OnuIpUpdate};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;

pub fn parse_mikrotik_leases(
    file_path: &str,
    db_onus: &[OnuForUpdateIp],
) -> Result<Vec<OnuIpUpdate>> {
    let db_map: HashMap<String, &OnuForUpdateIp> = db_onus
        .iter()
        .map(|identity| (identity.mac.trim().to_uppercase(), identity))
        .collect();

    let content = fs::read_to_string(file_path)
        .with_context(|| format!("No se pudo leer el archivo de leases: {}", file_path))?;

    let mut actualizaciones = Vec::new();

    for line in content.lines() {
        if line.starts_with("IP ADDRESS") || line.starts_with("---") || line.trim().is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split('|').map(|s| s.trim()).collect();

        if parts.len() < 2 {
            continue;
        }

        let file_ip = parts[0];
        let file_mac_raw = parts[1];

        if file_ip.is_empty() || file_ip == "---" {
            continue;
        }
        if file_mac_raw.is_empty() {
            continue;
        }

        let file_mac_norm = file_mac_raw.to_uppercase();

        if let Some(db_record) = db_map.get(&file_mac_norm) {
            let ip_changed = match &db_record.ip {
                Some(db_ip) => db_ip != file_ip,
                None => true,
            };

            if ip_changed {
                actualizaciones.push(OnuIpUpdate {
                    id: db_record.id,
                    new_ip: file_ip.to_string(),
                });
            }
        }
    }

    Ok(actualizaciones)
}
