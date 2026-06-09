// Core modules
mod auth;
mod cache;
mod config;
mod crypto;
mod data;
mod db;
mod domain;
mod error;
mod state;
// Axum modules
mod axum_router;
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

    tracing::info!("🚀 Iniciando API ABDO v0.3.63");
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

    // Seed lazy de planes y zonas de cobertura para el AI Agent. Solo
    // inserta si las colecciones están vacías.
    let state_for_ai_seed = state.clone();
    tokio::spawn(async move {
        modules::ai_agent::seed::run(state_for_ai_seed).await;
    });

    // Recovery: re-dispatch inbound messages left unanswered after a crash.
    let state_for_ai_recovery = state.clone();
    tokio::spawn(async move {
        modules::ai_agent::recovery::run_ai_recovery(state_for_ai_recovery).await;
    });

    // Calentar el índice de divisiones políticas de Venezuela (LazyLock).
    // El costo es ~6KB RAM pagado una sola vez al arrancar.
    let _ = data::ve_political_divisions::DIVISIONS.len();

    tracing::info!("✅ Conexiones establecidas");
    // 4. Construir router de Axum
    let app = axum_router::build_router(state);

    // 5. Crear listener TCP
    let addr = cfg.address();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "Servidor escuchando en: http://{}",
        addr
    );

    let listener = tokio::net::TcpListener::bind(&addr).await?;

    // 6. Iniciar servidor
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

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
