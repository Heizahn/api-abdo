// Core modules
mod auth;
mod config;
mod crypto;
mod db;
mod domain;
mod error;
mod state;
// Axum modules
mod axum_router;
mod cache;
mod middleware;
mod models;
mod modules;
mod openapi;
mod utils;

// Cron modules
mod cron_bcv;

use config::Config;
use state::AppState;
use std::net::SocketAddr;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Cargar configuración
    let cfg = Config::from_env();

    // 2. Inicializar tracing/logging
    init_tracing(&cfg);
    if !std::path::Path::new("./uploads").exists() {
        tokio::fs::create_dir("./uploads").await?;
    }

    tracing::info!("🚀 Iniciando API ABDO v0.2.0");
    tracing::info!("Environment: {}", cfg.rust_log);

    // 3. Inicializar estado de aplicación (MongoDB + Redis)
    tracing::info!("Inicializando conexiones...");
    let state = AppState::new(cfg.clone()).await.map_err(|e| {
        tracing::error!("Error inicializando estado: {:?}", e);
        format!("{:?}", e)
    })?;

    let state_for_cron = state.clone();
    tokio::spawn(async move {
        cron_bcv::run_bcv_scraper_task(state_for_cron).await;
    });

    // let state_for_zte = state.clone();
    // tokio::spawn(async move {
    //     cron_zte::run_zte_sync_task(state_for_zte).await;
    // });

    let state_for_mikrotik = state.clone();
    tokio::spawn(async move {
        modules::network::mikrotik::cron::run_mikrotik_sync_task(state_for_mikrotik).await;
    });

    let state_for_waba = state.clone();
    tokio::spawn(async move {
        modules::whatsapp::backfill::run_waba_backfill(state_for_waba).await;
    });

    let state_for_last_inbound = state.clone();
    tokio::spawn(async move {
        modules::whatsapp::backfill::run_last_inbound_backfill(state_for_last_inbound).await;
    });

    let state_for_conv_events = state.clone();
    tokio::spawn(async move {
        modules::whatsapp::backfill::run_conversation_events_backfill(state_for_conv_events).await;
    });

    tracing::info!("✅ Conexiones establecidas");
    // 4. Construir router de Axum
    let app = axum_router::build_router(state);

    // 5. Crear listener TCP
    let addr = cfg.address();
    tracing::info!("Servidor escuchando en: http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;

    // 6. Iniciar servidor
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}

/// Inicializa el sistema de tracing/logging
fn init_tracing(cfg: &Config) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.rust_log));

    if cfg.log_format == "json" {
        // Formato JSON para producción
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        // Formato pretty para desarrollo
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().pretty())
            .init();
    }
}
