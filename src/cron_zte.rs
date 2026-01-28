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
    tracing::info!(
        "🕰️ Iniciando servicio de sincronización ZTE (Programado para 00:00 UTC / 08:00 PM VET)"
    );
    let config = Config::from_env();
    let id_simcot = config.id_simcot;

    loop {
        // ==========================================
        // 1. Calcular tiempo hasta las 00:00 UTC
        // ==========================================
        let now = Utc::now();

        // Segundos que han pasado desde la medianoche de hoy (00:00:00)
        let seconds_from_midnight = now.num_seconds_from_midnight();

        // Total segundos en un día: 86,400
        // Calculamos cuánto falta para terminar el día
        let seconds_until_next_run = 86_400 - seconds_from_midnight;

        // Convertimos a Horas/Minutos solo para mostrar en el log
        let hours_wait = seconds_until_next_run / 3600;
        let mins_wait = (seconds_until_next_run % 3600) / 60;

        tracing::info!(
            "⏳ Próxima ejecución en {}h {}m (A las 00:00 UTC / 08:00 PM VET). Durmiendo...",
            hours_wait,
            mins_wait
        );

        // Dormimos la cantidad exacta hasta llegar a las 00:00 UTC
        time::sleep(Duration::from_secs(seconds_until_next_run as u64)).await;

        // ==========================================
        // 2. Ejecución (Justo a las 00:00 UTC)
        // ==========================================

        // Volvemos a tomar la fecha actual, que ahora será la del nuevo día UTC
        let execution_time = Utc::now();
        let filename = format!("onus_zte_{}.txt", execution_time.format("%Y-%m-%d"));

        tracing::info!(
            "🔄 Iniciando ciclo ZTE (Hora: {}). Archivo destino: {}",
            execution_time,
            filename
        );

        // --- INICIO DE TU LÓGICA ---
        let result_zte = procesar_olt_zte(filename.clone()).await;

        match result_zte {
            Ok(path) => {
                tracing::info!("📂 Reporte generado. Consultando DB para cruce de datos...");

                if let Ok(serials) = state.db.get_device_serial_numbers().await {
                    match zte_parse_update::parse_zte_report(&path, &serials) {
                        Ok(onus_to_update) => {
                            if !onus_to_update.is_empty() {
                                tracing::info!(
                                    "📦 Detectadas {} ONUs para actualizar.",
                                    onus_to_update.len()
                                );
                                for onu in onus_to_update {
                                    match state.db.save_onu_from_zte(onu, &id_simcot).await {
                                        Ok(_) => {
                                            tracing::debug!("✅ Onu actualizada correctamente")
                                        }
                                        Err(e) => {
                                            tracing::error!("❌ Error al actualizar onu: {}", e)
                                        }
                                    }
                                }
                            } else {
                                tracing::info!("✨ No hay cambios que actualizar.");
                            }
                        }
                        Err(e) => tracing::error!("❌ Error parseando reporte ZTE: {}", e),
                    }
                }
            }
            Err(e) => tracing::error!("❌ Error crítico ejecutando servicio ZTE: {}", e),
        }
        // --- FIN DE TU LÓGICA ---

        // ==========================================
        // 3. Pausa de seguridad
        // ==========================================
        // Esperamos 1 minuto extra para asegurarnos de no volver a ejecutar
        // en el mismo segundo 00:00:00 si el proceso es muy rápido.
        // El bucle volverá arriba y calculará que faltan 23h 59m para la próxima.
        time::sleep(Duration::from_secs(60)).await;
    }
}
