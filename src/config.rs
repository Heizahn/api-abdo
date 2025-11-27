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
            redis_uri: env::var("REDIS_URI").unwrap_or_else(|_| "redis://localhost:6379".to_string()),
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
        }
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
