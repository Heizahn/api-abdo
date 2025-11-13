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
mod handlers;
mod middleware;
mod models;
mod utils;

use chrono::Utc;
use config::Config;
use state::AppState;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Cargar configuración
    let cfg = Config::from_env();

    // 2. Inicializar tracing/logging
    init_tracing(&cfg);

    tracing::info!("🚀 Iniciando API ABDO v0.2.0");
    tracing::info!("Environment: {}", cfg.rust_log);

    // 3. Inicializar estado de aplicación (MongoDB + Redis)
    tracing::info!("Inicializando conexiones...");
    let state = AppState::new(cfg.clone()).await.map_err(|e| {
        tracing::error!("Error inicializando estado: {:?}", e);
        format!("{:?}", e)
    })?;

    tracing::info!("✅ Conexiones establecidas");

    // 4. Construir router de Axum
    let app = axum_router::build_router(state);

    // 5. Crear listener TCP
    let addr = cfg.address();
    tracing::info!("Servidor escuchando en: http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;

    println!("🚀 API ABDO v0.2.0 iniciada");
    println!("📊 Endpoints disponibles:");
    println!("   POST   /v1/auth/verify_number");
    println!("   POST   /v1/auth/login");
    println!("   POST   /v1/auth/refresh");
    println!("   GET    /v1/profile/me");
    println!("   GET    /v1/profile/me/balance");
    println!("   GET    /v1/profile/me/last_payments");
    println!("   GET    /v1/receivable/me");
    println!();
    println!("✨ Servidor listo para recibir peticiones");

    // 6. Iniciar servidor
    axum::serve(listener, app).await?;

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
