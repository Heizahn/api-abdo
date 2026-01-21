use chrono::{Days, Timelike, Utc}; // Agregamos Datelike y Days
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

use crate::db::UtilsRepository;
use crate::state::AppState;
use crate::utils::bcv_scraper::fetch_bcv_rate;

pub async fn run_bcv_scraper_task(state: Arc<AppState>) {
    tracing::info!("🕰️ Iniciando servicio de monitoreo BCV (Dinámico)");

    loop {
        let now = Utc::now();

        // Rangos del día actual UTC
        let start_of_day = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        let end_of_day = now.date_naive().and_hms_opt(23, 59, 59).unwrap().and_utc();

        tracing::debug!(
            "🔎 Cron BCV: Verificando tasa para el día UTC: {}",
            start_of_day
        );

        // 1. Verificar si ya existe dato en DB para HOY
        // Nota: Asumimos que exists_rate_for_date devuelve bool
        let already_exists = match state
            .db
            .exists_rate_for_date(start_of_day, end_of_day)
            .await
        {
            Ok(exists) => exists,
            Err(e) => {
                tracing::error!("❌ Error consultando DB: {}. Reintentando en 5 min.", e);
                time::sleep(Duration::from_secs(300)).await;
                continue;
            }
        };

        if already_exists {
            // === LÓGICA DE APAGADO HASTA MAÑANA ===
            tracing::info!("✅ Tasa BCV ya existe para hoy.");

            // Calculamos "Mañana a las 00:05 UTC" (damos 5 min de margen para evitar condiciones de carrera en cambio de día)
            let tomorrow = now.date_naive().checked_add_days(Days::new(1)).unwrap();
            let target_wakeup = tomorrow.and_hms_opt(0, 5, 0).unwrap().and_utc();

            // Calculamos la duración segura (std::time::Duration no acepta negativos)
            let duration_until_tomorrow = (target_wakeup - now)
                .to_std()
                .unwrap_or(Duration::from_secs(60));

            tracing::info!(
                "💤 Durmiendo scraper por {:?} hasta {}",
                duration_until_tomorrow,
                target_wakeup
            );

            // Aquí el hilo se detiene completamente hasta el día siguiente
            time::sleep(duration_until_tomorrow).await;

            // Al despertar, inicia el loop de nuevo automáticamente
            continue;
        }

        // 2. Si llegamos aquí, NO existe el dato. Scrapeamos.
        tracing::info!("🔄 Tasa no encontrada. Intentando obtener de bcv.org.ve...");

        match fetch_bcv_rate().await {
            Ok(rate) => {
                let save_date = now
                    .with_hour(5)
                    .unwrap()
                    .with_minute(30)
                    .unwrap()
                    .with_second(0)
                    .unwrap()
                    .with_nanosecond(0)
                    .unwrap();

                if let Err(e) = state.db.save_exchange_rate(rate, save_date).await {
                    tracing::error!("❌ Error guardando tasa BCV: {}", e);
                    // Si falla guardar, esperamos 30 min y reintentamos
                    time::sleep(Duration::from_secs(30 * 60)).await;
                } else {
                    tracing::info!("🚀 Tasa BCV actualizada: {}", rate);
                    let _ = state.redis.invalidate_exchange_rate().await;

                    // OPCIONAL: Como acabamos de guardar exitosamente, podríamos dormir hasta mañana
                    // inmediatamente aquí para ahorrar una consulta a DB en la siguiente vuelta.
                    // Simplemente dejamos que el loop continue, la próxima vuelta caerá en el `if already_exists` y dormirá.
                }
            }
            Err(e) => {
                tracing::warn!("⚠️ Falló scraping BCV: {}. Reintentando en 30 min.", e);
                // Si falla el scraping, esperamos 30 minutos (ciclo corto)
                time::sleep(Duration::from_secs(30 * 60)).await;
            }
        }
    }
}
