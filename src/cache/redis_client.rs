use redis::{Client, AsyncCommands, RedisError};
use crate::config::Config;
use crate::utils::timezone::VenezuelaDateTime;

#[derive(Clone)]
pub struct RedisClient {
    client: Client,
}

impl RedisClient {
    /// Crea un nuevo cliente Redis
    pub async fn new(cfg: &Config) -> Result<Self, RedisError> {
        tracing::info!("Inicializando cliente Redis...");

        let client = Client::open(cfg.redis_uri.as_str())?;

        // Verificar conexión con ping
        let mut conn = client.get_multiplexed_async_connection().await?;
        let _: () = redis::cmd("PING").query_async(&mut conn).await?;

        tracing::info!("✅ Cliente Redis conectado");

        Ok(Self { client })
    }

    /// Obtiene tasa de cambio del cache
    pub async fn get_exchange_rate(&self) -> Result<Option<f64>, RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        conn.get(exchange_rate_key()).await
    }

    /// Guarda tasa de cambio en cache con TTL
    pub async fn set_exchange_rate(&self, rate: f64, ttl_secs: u64) -> Result<(), RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        conn.set_ex(exchange_rate_key(), rate, ttl_secs).await
    }

    /// Invalida cache de tasa de cambio
    #[allow(dead_code)]
    pub async fn invalidate_exchange_rate(&self) -> Result<(), RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let _: () = conn.del(exchange_rate_key()).await?;
        Ok(())
    }

    /// Invalida cache de summary de usuario
    #[allow(dead_code)]
    pub async fn invalidate_user_summary(&self, user_id: &str) -> Result<(), RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let key = format!("summary:user:{}", user_id);
        let _: () = conn.del(key).await?;
        Ok(())
    }

    // ============================================
    // WhatsApp — carga de agentes y locks
    // ============================================

    /// Retorna la carga actual (nº de conversaciones activas) de un agente.
    pub async fn get_agent_load(&self, agent_id: &str) -> u64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let val: u64 = conn.get(agent_load_key(agent_id)).await.unwrap_or(0);
        val
    }

    /// Incrementa la carga del agente y retorna el nuevo valor.
    pub async fn incr_agent_load(&self, agent_id: &str) -> u64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.incr(agent_load_key(agent_id), 1u64).await.unwrap_or(0)
    }

    /// Decrementa la carga del agente (mínimo 0).
    pub async fn decr_agent_load(&self, agent_id: &str) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(_c) => _c,
            Err(_) => return,
        };
        let current: i64 = conn.get(agent_load_key(agent_id)).await.unwrap_or(0);
        if current > 0 {
            let _: () = conn.decr(agent_load_key(agent_id), 1i64).await.unwrap_or(());
        }
    }

    /// Intenta adquirir un lock de asignación para una conversación.
    /// Retorna true si el lock fue adquirido (esta instancia debe proceder).
    /// TTL de 15 segundos para evitar locks eternos.
    pub async fn try_lock_conversation(&self, conv_id: &str) -> bool {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return false,
        };
        let key = format!("wa:lock:conv:{}", conv_id);
        let result: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(15u64)
            .query_async(&mut conn)
            .await
            .unwrap_or(None);
        result.is_some()
    }
}

/// Genera la clave Redis para la tasa de cambio BCV, con scope de fecha venezolana.
/// Formato: `exchange_rate:bcv:{YYYY-MM-DD}` donde la fecha es en hora de Venezuela.
/// Esto garantiza que después de la medianoche VZT la clave cambia y se provoca un
/// cache miss, forzando una nueva consulta a la BD.
fn agent_load_key(agent_id: &str) -> String {
    format!("wa:load:{}", agent_id)
}

fn exchange_rate_key() -> String {
    let today = VenezuelaDateTime::now().date_string_venezuela();
    format!("exchange_rate:bcv:{}", today)
}
