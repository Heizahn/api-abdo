#![allow(dead_code)]

use crate::config::Config;
use crate::db::OnuRepository;
use crate::services::zte_parse_update;
use crate::services::zte_service::procesar_olt_zte;
use crate::state::AppState;
use chrono::{Timelike, Utc}; // 👈 Necesitamos Timelike y Utc
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

pub async fn run_zte_sync_task(state: Arc<AppState>) {
    tracing::info!("🚀 Iniciando servicio ZTE: Ejecución inicial inmediata...");
    
    let config = Config::from_env();
    let id_simcot = config.id_simcot;

    // --- PRIMERA EJECUCIÓN (Al arrancar) ---
    ejecutar_ciclo_zte(&state, &id_simcot).await;

    loop {
        // --- CÁLCULO DE ESPERA ---
        let now = Utc::now();
        let seconds_from_midnight = now.num_seconds_from_midnight();
        let seconds_until_next_run = 86_400 - seconds_from_midnight;

        let hours_wait = seconds_until_next_run / 3600;
        let mins_wait = (seconds_until_next_run % 3600) / 60;

        tracing::info!(
            "⏳ Próxima ejecución programada en {}h {}m (00:00 UTC). Durmiendo...",
            hours_wait,
            mins_wait
        );

        time::sleep(Duration::from_secs(seconds_until_next_run as u64)).await;

        // --- EJECUCIÓN PROGRAMADA ---
        ejecutar_ciclo_zte(&state, &id_simcot).await;

        // Pausa de seguridad para no repetir en el mismo segundo
        time::sleep(Duration::from_secs(60)).await;
    }
}

// Extraemos la lógica a una función interna para no repetir código
async fn ejecutar_ciclo_zte(state: &Arc<AppState>, id_simcot: &str) {
    let execution_time = Utc::now();
    let filename = format!("onus_zte_{}.txt", execution_time.format("%Y-%m-%d"));

    tracing::info!("🔄 Iniciando ciclo ZTE. Archivo: {}", filename);

    let result_zte = procesar_olt_zte(filename.clone()).await;

    match result_zte {
        Ok(path) => {
            tracing::info!("📂 Reporte generado. Consultando DB...");
            if let Ok(serials) = state.db.get_device_serial_numbers().await {
                match zte_parse_update::parse_zte_report(&path, &serials) {
                    Ok(onus_to_update) => {
                        if !onus_to_update.is_empty() {
                            tracing::info!("📦 Actualizando {} ONUs.", onus_to_update.len());
                            for onu in onus_to_update {
                                if let Err(e) = state.db.save_onu_from_zte(onu, id_simcot).await {
                                    tracing::error!("❌ Error en DB: {}", e);
                                }
                            }
                            tracing::info!("✅ Sincronización finalizada.");
                        } else {
                            tracing::info!("✨ No hay cambios detectados.");
                        }
                    }
                    Err(e) => tracing::error!("❌ Error parseando reporte: {}", e),
                }
            }
        }
        Err(e) => tracing::error!("❌ Error crítico en servicio ZTE: {}", e),
    }
}
