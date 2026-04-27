use dotenvy::dotenv;
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    // Servidor
    pub host: String,
    pub port: u16,

    // MongoDB
    pub mongo_uri: String,
    pub mongo_db: String,
    pub mongo_pool_size: u32,
    pub mongo_min_pool_size: u32,
    pub mongo_connect_timeout: u64,

    // Redis
    pub redis_uri: String,
    #[allow(dead_code)]
    pub redis_pool_size: u32,
    pub redis_exchange_rate_ttl: u64,

    // Rate Limiting
    // pub rate_limit_burst: u32,
    pub rate_limit_auth_per_minute: u64,

    // Logging
    pub rust_log: String,
    pub log_format: String,

    //System
    pub id_simcot: String,

    //ZTE
    #[allow(dead_code)]
    pub olt_zte_pass: String,

    //MikroTik
    pub port_mk: String,
    pub pass_mk: String,

    //Zabbix
    pub zabbix_url: String,
    pub zabbix_token: String,

    // WhatsApp Media Relay (Cloudflare Worker)
    // Si ambas están seteadas, las descargas de media van vía el worker
    // en lugar de conectar directo a `lookaside.fbsbx.com`. Existe porque
    // desde la VM la ruta a esa CDN está bloqueada por el ISP.
    pub wa_media_relay_url: Option<String>,
    pub wa_media_relay_secret: Option<String>,

    // Meta App ID — usado por la Resumable Upload API para subir media de
    // headers de templates. URL: `/{whatsapp_app_id}/uploads`. Si no está
    // seteada, el endpoint de upload responde 503 con código `app_id_not_configured`
    // y el resto del módulo WhatsApp sigue funcionando.
    pub whatsapp_app_id: Option<String>,

    // AI Agent Relay (Cloudflare Worker) — mismo patrón que `wa_media_relay`.
    // Si ambas están seteadas, las llamadas a Gemini
    // (`generativelanguage.googleapis.com`) pasan por el worker en lugar
    // de conectar directo. Pueden apuntar al mismo worker que WA media o
    // a uno separado — el worker `tools/cf-worker-media-relay` ya soporta
    // ambos hosts en la whitelist.
    pub ai_relay_url: Option<String>,
    pub ai_relay_secret: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        dotenv().ok(); // carga .env automáticamente

        Self {
            // Servidor
            host: env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: env::var("PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .expect("PORT debe ser un número válido"),

            // MongoDB
            mongo_uri: env::var("MONGO_URI").expect("Falta MONGO_URI en .env"),
            mongo_db: env::var("MONGO_DB").unwrap_or_else(|_| "test".to_string()),
            mongo_pool_size: env::var("MONGO_POOL_SIZE")
                .unwrap_or_else(|_| "100".to_string())
                .parse()
                .unwrap_or(100),
            mongo_min_pool_size: env::var("MONGO_MIN_POOL_SIZE")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .unwrap_or(10),
            mongo_connect_timeout: env::var("MONGO_CONNECT_TIMEOUT")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .unwrap_or(5),

            // Redis
            redis_uri: env::var("REDIS_URI")
                .unwrap_or_else(|_| "redis://localhost:6379".to_string()),
            redis_pool_size: env::var("REDIS_POOL_SIZE")
                .unwrap_or_else(|_| "50".to_string())
                .parse()
                .unwrap_or(50),
            redis_exchange_rate_ttl: env::var("REDIS_EXCHANGE_RATE_TTL")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),
            // rate_limit_burst: env::var("RATE_LIMIT_BURST")
            //     .unwrap_or_else(|_| "20".to_string())
            //     .parse()
            //     .unwrap_or(20),
            rate_limit_auth_per_minute: env::var("RATE_LIMIT_AUTH_PER_MINUTE")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .unwrap_or(5),

            // Logging
            rust_log: env::var("RUST_LOG").unwrap_or_else(|_| "info,api_abdo=debug".to_string()),
            log_format: env::var("LOG_FORMAT").unwrap_or_else(|_| "json".to_string()),

            //System
            id_simcot: env::var("ID_SIMCOT").unwrap_or_else(|_| "".to_string()),

            //ZTE
            olt_zte_pass: env::var("OLT_ZTE_PASS").expect("Falta OLT_ZTE_PASS en .env"),

            //MikroTik
            port_mk: env::var("PORT_MK").unwrap_or_else(|_| "22".to_string()),
            pass_mk: env::var("PASS_MK").expect("Falta PASS_MK en .env"),

            //Zabbix
            zabbix_url: env::var("ZABBIX_URL").expect("Falta ZABBIX_URL en .env"),
            zabbix_token: env::var("ZABBIX_TOKEN").expect("Falta ZABBIX_TOKEN en .env"),

            // WhatsApp Media Relay — opcional. Si no están seteadas, se cae
            // al flow directo a Meta (útil en dev o si la red mejora).
            wa_media_relay_url: env::var("WA_MEDIA_RELAY_URL").ok().filter(|s| !s.is_empty()),
            wa_media_relay_secret: env::var("WA_MEDIA_RELAY_SECRET").ok().filter(|s| !s.is_empty()),

            // Meta App ID — opcional. Si falta, el endpoint de upload de
            // header media responde 503; el resto sigue funcionando.
            whatsapp_app_id: env::var("WHATSAPP_APP_ID").ok().filter(|s| !s.is_empty()),

            // AI Agent Relay — opcional. Si no están seteadas, las llamadas
            // a Gemini van directo (probablemente fallen desde la VM por
            // bloqueo del ISP, igual que WA).
            ai_relay_url: env::var("AI_RELAY_URL").ok().filter(|s| !s.is_empty()),
            ai_relay_secret: env::var("AI_RELAY_SECRET").ok().filter(|s| !s.is_empty()),
        }
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
