use crate::config::Config;
use crate::db::UtilsRepository;
use crate::services::zte_parse_update;
use crate::services::zte_service::procesar_olt_zte;
use crate::state::AppState;
use chrono::Local;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

pub async fn run_zte_sync_task(state: Arc<AppState>) {
    tracing::info!("🕰️ Iniciando servicio de sincronización ZTE (Background Task)");
    let config = Config::from_env();
    let id_simcot = config.id_simcot;

    loop {
        // ==========================================
        // 1. Lógica de Fecha y Nombre de Archivo
        // ==========================================

        // Si quieres el archivo con la fecha de HOY:
        let now = Local::now();
        let filename = format!("onus_zte_{}.txt", now.format("%Y-%m-%d"));

        tracing::info!("🔄 Iniciando ciclo ZTE. Archivo destino: {}", filename);

        // ==========================================
        // 2. Ejecutar Scraping SSH
        // ==========================================
        // Pasamos el filename dinámico a la función
        let result_zte = procesar_olt_zte(filename.clone()).await;

        match result_zte {
            Ok(path) => {
                tracing::info!("📂 Reporte generado. Consultando DB para cruce de datos...");

                // AQUÍ TU LÓGICA DE BASE DE DATOS
                // Leer el archivo generado o procesar lógica de negocio...
                if let Ok(serials) = state.db.get_device_serial_numbers().await {
                    tracing::info!(
                        "🧠 Analizando archivo y cruzando con {} dispositivos...",
                        serials.len()
                    );

                    match zte_parse_update::parse_zte_report(&path, &serials) {
                        Ok(onus_to_update) => {
                            tracing::debug!("Onus a actualizar: {}", onus_to_update.len());
                            if onus_to_update.len() > 0 {
                                for onu in onus_to_update {
                                    tracing::debug!(
                                        "Actualizando onu con el SN: {} - MAC: {}",
                                        onu.sn,
                                        onu.mac
                                    );

                                    match state.db.save_onu_from_zte(onu, &id_simcot).await {
                                        Ok(_) => {
                                            tracing::debug!("✅ Onu actualizado correctamente");
                                        }
                                        Err(e) => {
                                            tracing::error!("❌ Error al actualizar onu: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("❌ Error crítico ejecutando servicio ZTE: {}", e);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("❌ Error crítico ejecutando servicio ZTE: {}", e);
            }
        }

        // ==========================================
        // 3. Dormir hasta el siguiente ciclo
        // ==========================================
        // Duerme 24 horas (ajustar según necesidad real)
        tracing::info!("💤 Durmiendo 24 horas...");
        time::sleep(Duration::from_secs(24 * 60 * 60)).await;
    }
}
