use crate::models::db::{OnuForUpdateIp, OnuIpUpdate};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;

pub fn parse_mikrotik_leases(
    file_path: &str,
    db_onus: &[OnuForUpdateIp],
) -> Result<Vec<OnuIpUpdate>> {
    // 1. INDEXAR LA DB POR MAC (HashMap para búsqueda O(1))
    // Clave: MAC Normalizada (Mayúsculas, trim), Valor: El registro completo
    // Esto es crucial para poder encontrar la ONU rápido usando la MAC del archivo
    let db_map: HashMap<String, &OnuForUpdateIp> = db_onus
        .iter()
        .map(|identity| (identity.mac.trim().to_uppercase(), identity))
        .collect();

    // 2. LEER EL ARCHIVO
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("No se pudo leer el archivo de leases: {}", file_path))?;

    let mut actualizaciones = Vec::new();

    // 3. RECORRER EL ARCHIVO LÍNEA POR LÍNEA
    for line in content.lines() {
        // Ignorar cabeceras y líneas vacías
        if line.starts_with("IP ADDRESS") || line.starts_with("---") || line.trim().is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split('|').map(|s| s.trim()).collect();

        // Estructura esperada: 0: IP, 1: MAC, 2: HOSTNAME
        if parts.len() < 2 {
            continue;
        }

        let file_ip = parts[0];
        let file_mac_raw = parts[1];

        // Validaciones básicas de integridad
        if file_ip.is_empty() || file_ip == "---" {
            continue;
        }
        if file_mac_raw.is_empty() {
            continue;
        }

        // Normalizamos la MAC del archivo para asegurar coincidencia (Upper Case)
        let file_mac_norm = file_mac_raw.to_uppercase();

        // 4. BUSCAR SI LA MAC EXISTE EN NUESTRA DB
        if let Some(db_record) = db_map.get(&file_mac_norm) {
            // ====================================================
            // LÓGICA DE DETECCIÓN DE CAMBIOS (DIFF IP)
            // ====================================================

            let ip_changed = match &db_record.ip {
                Some(db_ip) => db_ip != file_ip, // Si tiene IP, ¿es diferente?
                None => true,                    // Si no tiene IP (None), hay que actualizar
            };

            if ip_changed {
                // tracing::info!("Cambio de IP detectado para MAC {}: {} -> {}", file_mac_norm, db_record.ip.as_deref().unwrap_or("None"), file_ip);

                actualizaciones.push(OnuIpUpdate {
                    id: db_record.id,
                    new_ip: file_ip.to_string(),
                });
            }
        }
        // Si la MAC no está en el mapa, la ignoramos (no es una ONU registrada en nuestra DB)
    }

    Ok(actualizaciones)
}
