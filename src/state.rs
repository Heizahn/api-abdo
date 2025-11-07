use std::sync::Arc;
use crate::{
    cache::RedisClient,
    config::Config,
    db::MongoDB,
};

/// Estado compartido de la aplicación
/// Se pasa a todos los handlers mediante Axum's State extractor
#[derive(Clone)]
pub struct AppState {
    pub db: MongoDB,
    pub redis: RedisClient,
    pub config: Arc<Config>,
}

impl AppState {
    /// Crea un nuevo estado de aplicación
    /// Inicializa conexiones a MongoDB y Redis
    pub async fn new(config: Config) -> Result<Self, anyhow::Error> {
        tracing::info!("Inicializando estado de aplicación...");

        // Inicializar MongoDB con pool
        tracing::info!("Conectando a MongoDB: {}", config.mongo_uri);
        let db = MongoDB::new_with_pool(&config).await?;
        tracing::info!("✅ MongoDB conectado");

        // Inicializar Redis
        tracing::info!("Conectando a Redis: {}", config.redis_uri);
        let redis = RedisClient::new(&config).await?;
        tracing::info!("✅ Redis conectado");

        Ok(Self {
            db,
            redis,
            config: Arc::new(config),
        })
    }
}
