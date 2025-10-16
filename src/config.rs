use dotenvy::dotenv;
use std::env;

pub struct Config {
    pub host: String,
    pub port: u16,
    pub mongo_uri: String,
    pub mongo_db: String,
}

impl Config {
    pub fn from_env() -> Self {
        dotenv().ok(); // carga .env automáticamente

        let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("PORT")
            .unwrap_or_else(|_| "3000".to_string())
            .parse()
            .expect("PORT debe ser un número válido");
        let mongo_uri = env::var("MONGO_URI").expect("Falta MONGO_URI en .env");
        let mongo_db = env::var("MONGO_DB").unwrap_or_else(|_| "test".to_string());

        Self {
            host,
            port,
            mongo_uri,
            mongo_db,
        }
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
