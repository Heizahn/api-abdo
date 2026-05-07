use crate::config::Config;
use crate::utils::timezone::VenezuelaDateTime;
use redis::{AsyncCommands, Client, RedisError};
use sha2::{Digest, Sha256};

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
            let _: () = conn
                .decr(agent_load_key(agent_id), 1i64)
                .await
                .unwrap_or(());
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

    /// Intenta adquirir un lock de prefetch para `media_id`. Devuelve `true`
    /// si el caller debe hacer la descarga; `false` si ya hay otra tarea bajándolo.
    /// TTL 60s como red de seguridad (si el prefetch muere, el lock se libera solo).
    pub async fn try_lock_media_prefetch(&self, media_id: &str) -> bool {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return true, // Si Redis falla, permitimos la descarga.
        };
        let key = format!("wa:media:lock:{}", media_id);
        let result: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(60u64)
            .query_async(&mut conn)
            .await
            .unwrap_or(None);
        result.is_some()
    }

    /// Libera el lock de prefetch — idempotente, ignora errores.
    pub async fn release_media_prefetch_lock(&self, media_id: &str) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let key = format!("wa:media:lock:{}", media_id);
        let _: Result<(), _> = conn.del(key).await;
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
            .cmd("HSET")
            .arg(&key)
            .arg("bin")
            .arg(bytes)
            .ignore()
            .cmd("HSET")
            .arg(&key)
            .arg("mime")
            .arg(mime)
            .ignore();
        if let Some(f) = filename {
            pipe.cmd("HSET").arg(&key).arg("filename").arg(f).ignore();
        }
        pipe.cmd("EXPIRE").arg(&key).arg(2_592_000u64).ignore();
        let _: Result<(), _> = pipe.query_async(&mut conn).await;
    }

    // ============================================
    // AI Agent — cache de listado de modelos por workspace+api_key
    // ============================================

    /// Lee el cache del listado de modelos. Devuelve el JSON serializado tal
    /// cual fue guardado (el handler lo deserializa al DTO).
    ///
    /// El key incluye el hash de la api_key para que rotar la key invalide
    /// implícitamente el cache (key vieja queda huérfana hasta que expire).
    #[allow(dead_code)]
    pub async fn get_ai_models_cache(&self, workspace_id: &str, api_key: &str) -> Option<String> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        let key = ai_models_cache_key(workspace_id, api_key);
        conn.get(key).await.ok().flatten()
    }

    /// Cachea el listado serializado a JSON. TTL en segundos.
    #[allow(dead_code)]
    pub async fn set_ai_models_cache(
        &self,
        workspace_id: &str,
        api_key: &str,
        payload: &str,
        ttl_secs: u64,
    ) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let key = ai_models_cache_key(workspace_id, api_key);
        let _: Result<(), _> = conn.set_ex(key, payload, ttl_secs).await;
    }

    /// Borra TODAS las entradas de cache de modelos para el workspace
    /// (independientemente del hash de api_key). Se usa al rotar la api_key
    /// del workspace en el PATCH /settings.
    ///
    /// Implementación: SCAN + DEL — Redis no permite borrar por prefijo
    /// directamente. Es best-effort; si SCAN falla, no rompemos el flow del
    /// PATCH (la cache vieja queda hasta que expire por TTL).
    pub async fn invalidate_ai_models_cache(&self, workspace_id: &str) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let pattern = format!("ai_agent:models:{}:*", workspace_id);
        // SCAN con MATCH — itera todas las keys y junta las que matchean.
        let mut cursor: u64 = 0;
        let mut to_delete: Vec<String> = Vec::new();
        loop {
            let res: Result<(u64, Vec<String>), _> = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(100)
                .query_async(&mut conn)
                .await;
            match res {
                Ok((next, keys)) => {
                    to_delete.extend(keys);
                    if next == 0 {
                        break;
                    }
                    cursor = next;
                }
                Err(_) => return,
            }
        }
        for k in to_delete {
            let _: Result<(), _> = conn.del(k).await;
        }
    }

    // ============================================
    // AI Agent — cache de planes y zonas de cobertura
    // ============================================
    //
    // Las tools `list_plans` y `check_coverage` leen estos blobs en cada turno.
    // Cache TTL 5 min — los admins editan poco; cualquier write desde el CRUD
    // invalida la key y se repuebla en el siguiente tool call.

    const AI_PLANS_KEY: &str = "ai_agent:plans:list_active";
    // TODO: eliminar AI_COVERAGE_KEY y sus métodos asociados después de un ciclo de release.
    #[allow(dead_code)]
    const AI_COVERAGE_KEY: &str = "ai_agent:coverage:list_active";
    /// Cache key para el esquema jerárquico (Phase coverage-zones-restructure).
    /// Bumpeada a v2 para evitar conflictos durante rolling deploy.
    const AI_COVERAGE_KEY_V2: &str = "ai_agent:coverage:list_active:v2";

    pub async fn get_ai_plans_cache(&self) -> Option<String> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        conn.get(Self::AI_PLANS_KEY).await.ok().flatten()
    }

    pub async fn set_ai_plans_cache(&self, payload: &str, ttl_secs: u64) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let _: Result<(), _> = conn.set_ex(Self::AI_PLANS_KEY, payload, ttl_secs).await;
    }

    pub async fn invalidate_ai_plans_cache(&self) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let _: Result<(), _> = conn.del(Self::AI_PLANS_KEY).await;
    }

    // TODO: eliminar los tres métodos siguientes después de un ciclo de release completo.
    // Se mantienen para que un rollback a la binario anterior pueda limpiar su propia key.
    #[allow(dead_code)]
    pub async fn get_ai_coverage_cache(&self) -> Option<String> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        conn.get(Self::AI_COVERAGE_KEY).await.ok().flatten()
    }

    #[allow(dead_code)]
    pub async fn set_ai_coverage_cache(&self, payload: &str, ttl_secs: u64) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let _: Result<(), _> = conn.set_ex(Self::AI_COVERAGE_KEY, payload, ttl_secs).await;
    }

    #[allow(dead_code)]
    pub async fn invalidate_ai_coverage_cache(&self) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let _: Result<(), _> = conn.del(Self::AI_COVERAGE_KEY).await;
    }

    // ─── Coverage cache v2 (esquema jerárquico) ──────────────────────────────
    // TODO: Eliminar los métodos sin `_v2` después de un ciclo de release completo.

    pub async fn get_ai_coverage_cache_v2(&self) -> Option<String> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        conn.get(Self::AI_COVERAGE_KEY_V2).await.ok().flatten()
    }

    pub async fn set_ai_coverage_cache_v2(&self, payload: &str, ttl_secs: u64) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let _: Result<(), _> = conn
            .set_ex(Self::AI_COVERAGE_KEY_V2, payload, ttl_secs)
            .await;
    }

    pub async fn invalidate_ai_coverage_cache_v2(&self) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let _: Result<(), _> = conn.del(Self::AI_COVERAGE_KEY_V2).await;
    }

    // ============================================
    // AI Agent — cache de métodos de pago por owner
    // ============================================
    //
    // TTL 60s — más corto que planes porque editar el método debe reflejarse
    // rápido para el equipo admin. Sin invalidación explícita en MVP (TTL-only).

    const AI_PAYMENT_METHODS_KEY_PREFIX: &str = "ai_agent:payment_methods:";

    pub async fn get_ai_payment_methods_cache(&self, owner_id: &str) -> Option<String> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        let key = format!("{}{}", Self::AI_PAYMENT_METHODS_KEY_PREFIX, owner_id);
        conn.get(&key).await.ok().flatten()
    }

    pub async fn set_ai_payment_methods_cache(&self, owner_id: &str, payload: &str, ttl_secs: u64) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let key = format!("{}{}", Self::AI_PAYMENT_METHODS_KEY_PREFIX, owner_id);
        let _: Result<(), _> = conn.set_ex(&key, payload, ttl_secs).await;
    }

    // ============================================
    // AI Agent — catálogo de bancos (list_banks)
    // ============================================
    //
    // Clave global única — el catálogo de bancos BCV es nacional, no varía por
    // proveedor ni workspace. TTL 24h — los bancos del catálogo cambian rarísimo.
    // Sin invalidación explícita (TTL-only, idéntico al patrón de payment_methods).

    const AI_LIST_BANKS_KEY: &str = "ai_agent:list_banks";
    const AI_LIST_BANKS_CACHE_TTL_SECS: u64 = 86_400; // 24h

    pub async fn get_ai_list_banks_cache(&self) -> Option<String> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        conn.get(Self::AI_LIST_BANKS_KEY).await.ok().flatten()
    }

    pub async fn set_ai_list_banks_cache(&self, payload: &str, ttl_secs: u64) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let _: Result<(), _> = conn
            .set_ex(Self::AI_LIST_BANKS_KEY, payload, ttl_secs)
            .await;
    }

    // ============================================
    // AI Agent — configuración global (AiConfig)
    // ============================================
    //
    // Almacena el cleartext de la OpenRouter API key (solo la key, no el modelo)
    // para acceso O(1) en cada dispatch. TTL 300s — PATCH /config invalida
    // explícitamente esta key.

    const AI_CONFIG_CACHE_KEY: &str = "ai_agent:config";

    /// Devuelve la cleartext API key global desde cache. `None` en cache miss o
    /// si Redis está caído.
    pub async fn get_ai_config_cache(&self) -> Option<String> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        conn.get(Self::AI_CONFIG_CACHE_KEY).await.ok().flatten()
    }

    /// Persiste la cleartext API key en cache con el TTL dado (segundos).
    pub async fn set_ai_config_cache(&self, api_key: &str, ttl_secs: u64) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let _: Result<(), _> = conn
            .set_ex(Self::AI_CONFIG_CACHE_KEY, api_key, ttl_secs)
            .await;
    }

    /// Elimina la key de cache. Llamada inmediatamente después de cada
    /// escritura exitosa en PATCH /config.
    pub async fn invalidate_ai_config_cache(&self) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let _: Result<(), _> = conn.del(Self::AI_CONFIG_CACHE_KEY).await;
    }

    // ============================================
    // AI Agent — debounce de inbounds + lock anti-concurrencia
    // ============================================

    /// Marca el timestamp del último inbound recibido para `conv_id`. El
    /// dispatch lo usa para implementar debounce: tras dormir N segundos,
    /// compara el timestamp guardado con el suyo. Si coincide → es el último,
    /// procesa. Si cambió → llegó otro mensaje después, abort (otro spawn
    /// procesará la ráfaga completa). TTL 5 min.
    pub async fn set_ai_debounce_ts(&self, conv_id: &str, ts_ms: i64) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let key = format!("ai_agent:debounce:{}", conv_id);
        let _: Result<(), _> = conn.set_ex(key, ts_ms, 300u64).await;
    }

    /// Lee el timestamp de la última actividad inbound. `None` si Redis está
    /// caído o la key expiró.
    pub async fn get_ai_debounce_ts(&self, conv_id: &str) -> Option<i64> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        let key = format!("ai_agent:debounce:{}", conv_id);
        conn.get(key).await.ok().flatten()
    }

    /// Intenta adquirir el lock de dispatch IA para `conv_id`. Red de
    /// seguridad además del debounce — evita que dos spawns con timestamps
    /// muy cercanos terminen ejecutando el runner en paralelo. TTL 60s.
    pub async fn try_lock_ai_dispatch(&self, conv_id: &str) -> bool {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return true,
        };
        let key = format!("ai_agent:dispatch_lock:{}", conv_id);
        let result: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(60u64)
            .query_async(&mut conn)
            .await
            .unwrap_or(None);
        result.is_some()
    }

    /// Libera el lock de dispatch IA. Idempotente; ignora errores.
    pub async fn release_ai_dispatch_lock(&self, conv_id: &str) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let key = format!("ai_agent:dispatch_lock:{}", conv_id);
        let _: Result<(), _> = conn.del(key).await;
    }

    // ============================================
    // AI Agent — counters de límites + escalación
    // ============================================
    //
    // Diseño:
    // - Counters per-conv (TTL 7 días — auto-cleanup si la conv queda inactiva).
    //   Reset explícito en close/reopen y al auto-escalar.
    // - Counters per-agent diarios. La key incluye `YYYY-MM-DD` en TZ Caracas
    //   para que cada día tenga su key fresca; TTL 36h cubre el rollover sin
    //   borrar mientras el día está corriendo.

    fn ai_today_str() -> String {
        VenezuelaDateTime::now().date_string_venezuela()
    }

    /// Segundos hasta el final del día actual en Caracas (ttl seguro para
    /// counters diarios — al rollover la siguiente key se crea fresca y la
    /// vieja expira sola).
    fn ai_seconds_to_eod_caracas() -> u64 {
        use chrono::{Duration, NaiveTime, TimeZone};
        let now_vz = VenezuelaDateTime::now().in_venezuela();
        let tomorrow = now_vz.date_naive() + Duration::days(1);
        let midnight_naive = tomorrow.and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        let tz = chrono_tz::America::Caracas;
        let midnight = match tz.from_local_datetime(&midnight_naive) {
            chrono::LocalResult::Single(dt) => dt,
            chrono::LocalResult::Ambiguous(dt, _) => dt,
            chrono::LocalResult::None => return 86_400,
        };
        let secs = (midnight.timestamp() - now_vz.timestamp()).max(60) as u64;
        // Cap defensivo: si por algún motivo algo sale mal, no más de 36h.
        secs.min(36 * 3600)
    }

    // ── Per-conv: turns total ───────────────────────────────────────────────

    pub async fn incr_ai_turns_conv(&self, conv_id: &str) -> i64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let key = format!("ai_agent:turns_conv:{}", conv_id);
        let new_val: i64 = conn.incr(&key, 1).await.unwrap_or(0);
        let _: Result<(), _> = conn.expire(&key, 7 * 24 * 3600).await;
        new_val
    }

    pub async fn get_ai_turns_conv(&self, conv_id: &str) -> i64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let key = format!("ai_agent:turns_conv:{}", conv_id);
        conn.get(&key).await.unwrap_or(Some(0)).unwrap_or(0)
    }

    // ── Per-conv: identification attempts (lookup_customer fallidos) ────────

    pub async fn incr_ai_id_attempts(&self, conv_id: &str) -> i64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let key = format!("ai_agent:id_attempts:{}", conv_id);
        let new_val: i64 = conn.incr(&key, 1).await.unwrap_or(0);
        let _: Result<(), _> = conn.expire(&key, 7 * 24 * 3600).await;
        new_val
    }

    // ── Per-conv: turnos sin resolución (sin tool de cierre) ────────────────

    pub async fn incr_ai_no_resolution(&self, conv_id: &str) -> i64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let key = format!("ai_agent:no_resolution:{}", conv_id);
        let new_val: i64 = conn.incr(&key, 1).await.unwrap_or(0);
        let _: Result<(), _> = conn.expire(&key, 7 * 24 * 3600).await;
        new_val
    }

    /// Reset targeted del counter de no-resolución para una conversación.
    /// Sólo borra la key `ai_agent:no_resolution:{conv_id}` — NO toca
    /// `turns_conv` ni `id_attempts` (esos los limpia `clear_ai_conv_counters`
    /// en eventos terminales como auto_escalate o close/reopen).
    ///
    /// Idempotente: DEL sobre key inexistente es no-op silencioso. Failure
    /// handling: best-effort, igual que `incr_ai_no_resolution` (si Redis
    /// está caído, el counter "se pierde" pero la conv sigue funcionando).
    pub async fn reset_ai_no_resolution(&self, conv_id: &str) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let key = format!("ai_agent:no_resolution:{}", conv_id);
        let _: Result<(), _> = conn.del(&key).await;
    }

    /// Limpia todos los counters per-conv (turns, id_attempts, no_resolution).
    /// Se llama desde close/reopen y al auto-escalar.
    pub async fn clear_ai_conv_counters(&self, conv_id: &str) {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let keys = vec![
            format!("ai_agent:turns_conv:{}", conv_id),
            format!("ai_agent:id_attempts:{}", conv_id),
            format!("ai_agent:no_resolution:{}", conv_id),
        ];
        for k in keys {
            let _: Result<(), _> = conn.del(k).await;
        }
    }

    // ── Per-agent diario: turnos ────────────────────────────────────────────

    pub async fn incr_ai_turns_agent_daily(&self, agent_id: &str) -> i64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let day = Self::ai_today_str();
        let key = format!("ai_agent:turns_daily:{}:{}", agent_id, day);
        let new_val: i64 = conn.incr(&key, 1).await.unwrap_or(0);
        let ttl = Self::ai_seconds_to_eod_caracas() + 3600;
        let _: Result<(), _> = conn.expire(&key, ttl as i64).await;
        new_val
    }

    pub async fn get_ai_turns_agent_daily(&self, agent_id: &str) -> i64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let day = Self::ai_today_str();
        let key = format!("ai_agent:turns_daily:{}:{}", agent_id, day);
        conn.get(&key).await.unwrap_or(Some(0)).unwrap_or(0)
    }

    // ── Per-agent diario: tokens ────────────────────────────────────────────

    pub async fn add_ai_tokens_agent_daily(&self, agent_id: &str, tokens: u64) -> i64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let day = Self::ai_today_str();
        let key = format!("ai_agent:tokens_daily:{}:{}", agent_id, day);
        let new_val: i64 = conn.incr(&key, tokens as i64).await.unwrap_or(0);
        let ttl = Self::ai_seconds_to_eod_caracas() + 3600;
        let _: Result<(), _> = conn.expire(&key, ttl as i64).await;
        new_val
    }

    pub async fn get_ai_tokens_agent_daily(&self, agent_id: &str) -> i64 {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let day = Self::ai_today_str();
        let key = format!("ai_agent:tokens_daily:{}:{}", agent_id, day);
        conn.get(&key).await.unwrap_or(Some(0)).unwrap_or(0)
    }

    /// Atómicamente setea el flag "alerta de costo ya emitida hoy" para evitar
    /// inundar logs/WS. Devuelve `true` si era la primera vez (caller emite
    /// alerta), `false` si ya estaba seteado.
    pub async fn try_mark_cost_alert_today(&self, agent_id: &str) -> bool {
        let mut conn = match self.client.get_multiplexed_async_connection().await {
            Ok(c) => c,
            Err(_) => return false,
        };
        let day = Self::ai_today_str();
        let key = format!("ai_agent:cost_alert:{}:{}", agent_id, day);
        let ttl = Self::ai_seconds_to_eod_caracas() + 3600;
        let result: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(ttl)
            .query_async(&mut conn)
            .await
            .unwrap_or(None);
        result.is_some()
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

fn media_cache_key(media_id: &str) -> String {
    format!("wa:media:{}", media_id)
}

/// Hash corto (8 bytes hex) de la api_key. No es para verificar la key —
/// sólo para que dos workspaces con la misma key y dos workspaces con keys
/// distintas usen entradas de cache separadas.
#[allow(dead_code)]
fn ai_models_cache_key(workspace_id: &str, api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    let mut hex = String::with_capacity(16);
    for b in digest.iter().take(8) {
        hex.push_str(&format!("{:02x}", b));
    }
    format!("ai_agent:models:{}:{}", workspace_id, hex)
}

fn exchange_rate_key() -> String {
    let today = VenezuelaDateTime::now().date_string_venezuela();
    format!("exchange_rate:bcv:{}", today)
}
