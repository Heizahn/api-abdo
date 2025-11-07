use redis::{Client, AsyncCommands, RedisError};
use crate::config::Config;
use serde::{Serialize, Deserialize};
use crate::db::mongo::PhoneSummary;

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
        redis::cmd("PING").query_async::<_, ()>(&mut conn).await?;

        tracing::info!("✅ Cliente Redis conectado");

        Ok(Self { client })
    }

    /// Obtiene tasa de cambio del cache
    pub async fn get_exchange_rate(&self) -> Result<Option<f64>, RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        conn.get("exchange_rate:bcv").await
    }

    /// Guarda tasa de cambio en cache con TTL
    pub async fn set_exchange_rate(&self, rate: f64, ttl_secs: u64) -> Result<(), RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        conn.set_ex("exchange_rate:bcv", rate, ttl_secs).await
    }

    /// Obtiene balance de usuario del cache
    pub async fn get_user_balance(&self, user_id: &str) -> Result<Option<f64>, RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let key = format!("balance:user:{}", user_id);
        conn.get(key).await
    }

    /// Guarda balance de usuario en cache con TTL
    pub async fn set_user_balance(&self, user_id: &str, balance: f64, ttl_secs: u64) -> Result<(), RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let key = format!("balance:user:{}", user_id);
        conn.set_ex(key, balance, ttl_secs).await
    }

    /// Obtiene datos de usuario del cache
    pub async fn get_user_summary(&self, user_id: &str) -> Result<Option<PhoneSummary>, RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let key = format!("summary:user:{}", user_id);

        let data: Option<String> = conn.get(&key).await?;

        match data {
            Some(json_str) => {
                match serde_json::from_str::<PhoneSummary>(&json_str) {
                    Ok(summary) => Ok(Some(summary)),
                    Err(e) => {
                        tracing::error!("Error deserializando summary de cache: {:?}", e);
                        Ok(None)
                    }
                }
            }
            None => Ok(None),
        }
    }

    /// Guarda datos de usuario en cache con TTL
    pub async fn set_user_summary(&self, user_id: &str, summary: &PhoneSummary, ttl_secs: u64) -> Result<(), RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let key = format!("summary:user:{}", user_id);

        match serde_json::to_string(summary) {
            Ok(json_str) => {
                conn.set_ex(key, json_str, ttl_secs).await
            }
            Err(e) => {
                tracing::error!("Error serializando summary para cache: {:?}", e);
                Ok(()) // No fallamos, simplemente no cacheamos
            }
        }
    }

    /// Invalida cache de balance de usuario
    pub async fn invalidate_user_balance(&self, user_id: &str) -> Result<(), RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let key = format!("balance:user:{}", user_id);
        let _: () = conn.del(key).await?;
        Ok(())
    }

    /// Invalida cache de summary de usuario
    pub async fn invalidate_user_summary(&self, user_id: &str) -> Result<(), RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let key = format!("summary:user:{}", user_id);
        let _: () = conn.del(key).await?;
        Ok(())
    }
}

/// Implementar Serialize y Deserialize para PhoneSummary
/// (Necesario para cache en Redis)
impl Serialize for PhoneSummary {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("PhoneSummary", 2)?;
        state.serialize_field("primary_name", &self.primary_name)?;
        state.serialize_field("phone", &self.phone)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for PhoneSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct PhoneSummaryHelper {
            primary_name: String,
            phone: String,
        }

        let helper = PhoneSummaryHelper::deserialize(deserializer)?;
        Ok(PhoneSummary {
            primary_name: helper.primary_name,
            phone: helper.phone,
        })
    }
}
