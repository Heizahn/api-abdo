// src/cron_bcv.rs (Nuevo archivo)
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tokio::time; // Para sleep e interval

use crate::db::UtilsRepository;
use crate::state::AppState;
use crate::utils::bcv_scraper::fetch_bcv_rate;

pub async fn run_bcv_scraper_task(state: Arc<AppState>) {
    tracing::info!("🕰️ Iniciando servicio de monitoreo BCV (check cada 30 min)");

    // Intervalo de revisión: cada 30 minutos
    let mut interval = time::interval(Duration::from_secs(30 * 60));

    loop {
        interval.tick().await; // Espera el siguiente tick

        let now = Utc::now();

        // Obtenemos el inicio y fin del día actual en UTC (00:00:00 a 23:59:59)
        // Esto cubre el requisito: A partir de las 8PM VET es 00:00 UTC, es un nuevo día.
        let start_of_day = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        let end_of_day = now.date_naive().and_hms_opt(23, 59, 59).unwrap().and_utc();

        tracing::debug!(
            "🔎 Cron BCV: Verificando tasa para el día UTC: {}",
            start_of_day
        );

        // 1. Verificar si ya existe dato en DB
        match state
            .db
            .exists_rate_for_date(start_of_day, end_of_day)
            .await
        {
            Ok(exists) => {
                if exists {
                    tracing::debug!("✅ Tasa BCV ya existe para hoy. Ignorando scrape.");
                    continue;
                }
            }
            Err(e) => {
                tracing::error!("❌ Error consultando DB en cron BCV: {}", e);
                continue;
            }
        }

        // 2. Si no existe, Scrapear
        tracing::info!("🔄 Tasa no encontrada para hoy. Intentando obtener de bcv.org.ve...");

        match fetch_bcv_rate().await {
            Ok(rate) => {
                // 3. Guardar con lógica de fecha: Dia actual + 5 horas
                // Esto asegura que al convertir a hora VET (UTC-4) sean la 1:00 AM del día correcto
                // y entre en los rangos de búsqueda del sistema.
                let save_date = now + chrono::Duration::hours(5);

                if let Err(e) = state.db.save_exchange_rate(rate, save_date).await {
                    tracing::error!("❌ Error guardando tasa BCV en DB: {}", e);
                } else {
                    tracing::info!(
                        "🚀 Tasa BCV actualizada: {} (Fecha guardada: {})",
                        rate,
                        save_date
                    );

                    // Opcional: Invalidar caché de Redis para que la app tome el nuevo valor de inmediato
                    let _ = state.redis.invalidate_exchange_rate().await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    "⚠️ Falló el scraping del BCV: {}. Reintentando en 30 min.",
                    e
                );
            }
        }
    }
}
