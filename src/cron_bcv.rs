use std::sync::Arc;
use std::time::Duration;
use chrono::{Utc, Timelike}; // Importante: Timelike para modificar horas/minutos
use tokio::time;

use crate::state::AppState;
use crate::db::UtilsRepository;
use crate::utils::bcv_scraper::fetch_bcv_rate;

pub async fn run_bcv_scraper_task(state: Arc<AppState>) {
    tracing::info!("🕰️ Iniciando servicio de monitoreo BCV (check cada 30 min)");

    // Intervalo de revisión: cada 30 minutos
    let mut interval = time::interval(Duration::from_secs(30 * 60));

    loop {
        interval.tick().await; 
        
        let now = Utc::now();
        
        // Inicio y fin del día actual UTC para buscar si ya existe registro
        let start_of_day = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        let end_of_day = now.date_naive().and_hms_opt(23, 59, 59).unwrap().and_utc();

        tracing::debug!("🔎 Cron BCV: Verificando tasa para el día UTC: {}", start_of_day);

        // 1. Verificar si ya existe dato en DB para HOY
        match state.db.exists_rate_for_date(start_of_day, end_of_day).await {
            Ok(exists) => {
                if exists {
                    tracing::debug!("✅ Tasa BCV ya existe para hoy. Ignorando scrape.");
                    continue; 
                }
            },
            Err(e) => {
                tracing::error!("❌ Error consultando DB en cron BCV: {}", e);
                continue;
            }
        }

        // 2. Si no existe, Scrapear
        tracing::info!("🔄 Tasa no encontrada para hoy. Intentando obtener de bcv.org.ve...");
        
        match fetch_bcv_rate().await {
            Ok(rate) => {
                // CAMBIO AQUI:
                // En vez de sumar tiempo, construimos una fecha con hora fija: 05:30:00 UTC
                // Esto asegura consistencia total.
                let save_date = now
                    .with_hour(5).unwrap()
                    .with_minute(30).unwrap()
                    .with_second(0).unwrap()
                    .with_nanosecond(0).unwrap();

                if let Err(e) = state.db.save_exchange_rate(rate, save_date).await {
                    tracing::error!("❌ Error guardando tasa BCV en DB: {}", e);
                } else {
                    tracing::info!("🚀 Tasa BCV actualizada: {} (Fecha fijada: {})", rate, save_date);
                    
                    // Invalidar caché para que la API tome el nuevo valor inmediatamente
                    let _ = state.redis.invalidate_exchange_rate().await;
                }
            },
            Err(e) => {
                tracing::warn!("⚠️ Falló el scraping del BCV: {}. Reintentando en 30 min.", e);
            }
        }
    }
}