use redis::{Client, AsyncCommands, RedisError};
use sha2::{Digest, Sha256};
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

    // ============================================
    // WhatsApp — cache de URL previews
    // ============================================

    /// Lee el cache de preview por URL. Retorna:
    /// - `Some(Some(json))` → hit con preview
    /// - `Some(None)`       → hit negativo (URL ya intentada sin preview; no re-fetchear)
    /// - `None`             → miss (hay que fetchear)
    ///
    /// Se guarda como JSON: `"null"` para miss negativo, el objeto serializado para hit.
    pub async fn get_url_preview(&self, url: &str) -> Option<Option<serde_json::Value>> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        let raw: Option<String> = conn.get(url_preview_key(url)).await.ok().flatten();
        let s = raw?;
        // `null` literal = hit negativo (URL mala, no re-fetchear hasta expirar TTL).
        if s.trim() == "null" {
            return Some(None);
        }
        serde_json::from_str::<serde_json::Value>(&s).ok().map(Some)
    }

    /// Guarda preview (o miss negativo con `None`) por URL con TTL de 24h.
    pub async fn set_url_preview(&self, url: &str, preview: Option<&serde_json::Value>) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let raw = match preview {
            Some(v) => v.to_string(),
            None => "null".to_string(),
        };
        let _: Result<(), _> = conn.set_ex(url_preview_key(url), raw, 86_400).await;
    }

    // ============================================
    // WhatsApp — cache de templates por WABA
    // ============================================

    /// Lee el cache de templates para un WABA id. Retorna el JSON serializado.
    pub async fn get_templates(&self, waba_id: &str) -> Option<serde_json::Value> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        let raw: Option<String> = conn.get(templates_key(waba_id)).await.ok().flatten();
        serde_json::from_str::<serde_json::Value>(&raw?).ok()
    }

    /// Guarda templates para un WABA id con TTL de 300s (5 minutos).
    pub async fn set_templates(&self, waba_id: &str, value: &serde_json::Value) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let raw = value.to_string();
        let _: Result<(), _> = conn.set_ex(templates_key(waba_id), raw, 300).await;
    }

    // ============================================
    // WhatsApp — cache de media (binarios inmutables)
    // ============================================

    /// Lee un media cacheado. Retorna `(bytes, mime, filename)` si hay hit.
    /// Lee 3 campos con HGETALL en una sola round-trip.
    pub async fn get_media_cache(
        &self,
        media_id: &str,
    ) -> Option<(Vec<u8>, String, Option<String>)> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        let key = media_cache_key(media_id);
        let bin: Vec<u8> = redis::cmd("HGET")
            .arg(&key)
            .arg("bin")
            .query_async(&mut conn)
            .await
            .ok()?;
        if bin.is_empty() {
            return None;
        }
        let mime: Option<String> = redis::cmd("HGET")
            .arg(&key)
            .arg("mime")
            .query_async(&mut conn)
            .await
            .ok();
        let filename: Option<String> = redis::cmd("HGET")
            .arg(&key)
            .arg("filename")
            .query_async(&mut conn)
            .await
            .ok();
        Some((
            bin,
            mime.unwrap_or_else(|| "application/octet-stream".to_string()),
            filename,
        ))
    }

    /// Guarda un media en Redis con TTL de 30 días (los `media_id` de Meta son inmutables).
    /// No-op silencioso si Redis falla — es best-effort.
    pub async fn set_media_cache(
        &self,
        media_id: &str,
        bytes: &[u8],
        mime: &str,
        filename: Option<&str>,
    ) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let key = media_cache_key(media_id);
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("HSET").arg(&key).arg("bin").arg(bytes).ignore()
            .cmd("HSET").arg(&key).arg("mime").arg(mime).ignore();
        if let Some(f) = filename {
            pipe.cmd("HSET").arg(&key).arg("filename").arg(f).ignore();
        }
        pipe.cmd("EXPIRE").arg(&key).arg(2_592_000u64).ignore();
        let _: Result<(), _> = pipe.query_async(&mut conn).await;
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

/// Hash de la URL para evitar keys gigantes. URL-sensitive: cualquier diferencia
/// (scheme, case del path, fragment) genera keys distintas.
fn url_preview_key(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        hex.push_str(&format!("{:02x}", b));
    }
    format!("wa:url_preview:{}", hex)
}

fn templates_key(waba_id: &str) -> String {
    format!("wa:templates:{}", waba_id)
}

fn media_cache_key(media_id: &str) -> String {
    format!("wa:media:{}", media_id)
}

fn exchange_rate_key() -> String {
    let today = VenezuelaDateTime::now().date_string_venezuela();
    format!("exchange_rate:bcv:{}", today)
}
