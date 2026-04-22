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
            reqwest_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(20))
                // Matar sockets idle antes de que el NAT/proxy los expire.
                // Evita el caso típico: reqwest reutiliza un socket stale y
                // el envío queda colgado hasta timeout mientras curl (socket
                // nuevo) funciona normal.
                .pool_idle_timeout(std::time::Duration::from_secs(30))
                .tcp_keepalive(std::time::Duration::from_secs(15))
                // Forzar IPv4: en Debian el resolver devuelve AAAA primero y
                // reqwest no tiene Happy Eyeballs, así que se cuelga 10s en
                // cada intento a IPv6 (graph.facebook.com). Binding a 0.0.0.0
                // obliga al OS a elegir una dirección IPv4 local, lo que
                // descarta la ruta IPv6.
                .local_address(std::net::IpAddr::from([0u8, 0, 0, 0]))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            ws_registry: Arc::new(RwLock::new(HashMap::new())),
        }))
    }
}
