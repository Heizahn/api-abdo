use crate::{cache::RedisClient, config::Config, db::mongo::MongoDB};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{mpsc::UnboundedSender, RwLock};

/// Mapa de conexiones WebSocket activas: user_id → sender de eventos JSON.
pub type WsRegistry = Arc<RwLock<HashMap<String, UnboundedSender<String>>>>;

/// Estado compartido de la aplicación
/// Se pasa a todos los handlers mediante Axum's State extractor


#[derive(Clone)]
pub struct AppState {
    pub db: MongoDB,
    pub redis: RedisClient,
    pub config: Arc<Config>,
    pub reqwest_client: reqwest::Client,
    pub ws_registry: WsRegistry,
}
impl AppState {
    /// Crea un nuevo estado de aplicación
    /// Inicializa conexiones a MongoDB y Redis
    pub async fn new(config: Config) -> Result<Arc<Self>, anyhow::Error> {
        tracing::info!("Inicializando estado de aplicación...");

        // Inicializar MongoDB con pool
        tracing::info!("Conectando a MongoDB: {}", config.mongo_uri);
        let db = MongoDB::new_with_pool(&config).await?;
        tracing::info!("✅ MongoDB conectado");

        // Inicializar Redis
        tracing::info!("Conectando a Redis: {}", config.redis_uri);
        let redis = RedisClient::new(&config).await?;
        tracing::info!("✅ Redis conectado");


        Ok(Arc::new(Self {
            db,
            redis,
            config: Arc::new(config),
            reqwest_client: reqwest::Client::new(),
            ws_registry: Arc::new(RwLock::new(HashMap::new())),
        }))
    }
}
