use crate::models::db::OnuIdentity;
use anyhow::{Context, Result};
use mongodb::bson::oid::ObjectId; // Necesario para comparar ID OLT
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct OnuDetected {
    pub id: ObjectId,
    pub sn: String,
    pub mac: String,
    pub motherboard: u32,
    pub pon: u32,
    pub id_onu: u32,
    pub id_olt: ObjectId, // Lo mantengo String para tu struct de salida
}

pub fn parse_zte_report(file_path: &str, db_sn_list: &[OnuIdentity]) -> Result<Vec<OnuDetected>> {
    // 1. ESTRATEGIA: Usar HashMap para acceso rápido al registro completo
    // Clave: SN (&str), Valor: El registro completo (&OnuIdentity)
    let db_map: HashMap<&str, &OnuIdentity> = db_sn_list
        .iter()
        .map(|identity| (identity.sn.as_str(), identity))
        .collect();

    // ID de la OLT actual (Hardcodeado según tu ejemplo)
    let current_olt_str = "697228928ace1d49d5c64192";
    // Lo convertimos a ObjectId una sola vez para comparar rápido con la DB
    let current_olt_id = ObjectId::from_str(current_olt_str).unwrap_or_default();

    // 2. Leemos el archivo
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("No se pudo leer el archivo de reporte: {}", file_path))?;

    let mut actualizaciones = Vec::new();
    let re_mac = Regex::new(r"^([0-9A-Fa-f]{2}[:-]){5}([0-9A-Fa-f]{2})$").unwrap();

    // 3. Iteramos línea por línea
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
        let mac_file = parts[5]; // Mac encontrada en el archivo

        // 4. Filtros básicos
        if !estado.eq_ignore_ascii_case("ready") {
            continue;
        }
        if !re_mac.is_match(mac_file) {
            continue;
        }

        // 5. BÚSQUEDA Y COMPARACIÓN (EL CORAZÓN DE LA LÓGICA)

        // Buscamos si el SN existe en nuestra DB
        if let Some(db_record) = db_map.get(sn) {
            // Parseamos los datos de la interfaz (igual que antes)
            let clean_interface = interfaz_raw.replace("gpon_onu-", "");
            let interface_parts: Vec<&str> = clean_interface.split('/').collect();

            if interface_parts.len() >= 3 {
                if let Ok(moth_u32) = interface_parts[1].parse::<u32>() {
                    let pon_id_parts: Vec<&str> = interface_parts[2].split(':').collect();
                    if pon_id_parts.len() == 2 {
                        let pon_u32 = pon_id_parts[0].parse::<u32>().unwrap_or(0);
                        let id_onu_u32 = pon_id_parts[1].parse::<u32>().unwrap_or(0);

                        if moth_u32 > 0 && pon_u32 > 0 && id_onu_u32 > 0 {
                            // ====================================================
                            // LÓGICA DE DETECCIÓN DE CAMBIOS (DIFF)
                            // ====================================================

                            // Convertimos los u32 a i32 porque la DB usa Option<i32>
                            let file_moth_i32 = moth_u32 as i32;
                            let file_pon_i32 = pon_u32 as i32;
                            let file_id_onu_i32 = id_onu_u32 as i32;

                            // Comparamos campo por campo.
                            // Si en la DB es None, O es diferente al archivo -> UPDATE REQUERIDO

                            let mac_changed = match &db_record.mac {
                                Some(db_mac) => !db_mac.eq_ignore_ascii_case(mac_file),
                                None => true, // Si no tenía MAC, hay que actualizar
                            };

                            let moth_changed = db_record.motherboard != Some(file_moth_i32);
                            let pon_changed = db_record.pon != Some(file_pon_i32);
                            let id_onu_changed = db_record.id_onu != Some(file_id_onu_i32);

                            // Comparamos si la OLT asignada en DB es la misma que estamos escaneando
                            let olt_changed = db_record.id_olt != Some(current_olt_id);

                            // SI ALGO CAMBIÓ, AGREGAMOS A LA LISTA
                            if mac_changed
                                || moth_changed
                                || pon_changed
                                || id_onu_changed
                                || olt_changed
                            {
                                // tracing::info!("Detectado cambio en SN {}: MacDiff: {}, PonDiff: {}", sn, mac_changed, pon_changed);

                                actualizaciones.push(OnuDetected {
                                    id: db_record.id,
                                    sn: sn.to_string(),
                                    mac: mac_file.to_string(),
                                    motherboard: moth_u32,
                                    pon: pon_u32,
                                    id_onu: id_onu_u32,
                                    id_olt: current_olt_id,
                                });
                            }
                            // ELSE: Si todo es idéntico, NO hacemos nada (Ignoramos)
                        }
                    }
                }
            }
        } else {
            // El SN está en el archivo pero NO en la DB.
            // Según tu lógica anterior, esto se ignora.
            // Si quisieras insertar nuevos, aquí iría el 'else'.
            continue;
        }
    }

    Ok(actualizaciones)
}
