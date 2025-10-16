mod auth;
mod config;
mod crypto;
mod db;
mod domain;
mod http;
mod router;

use config::Config;
use db::mongo::MongoDB;
use http::server::HttpServer;
use router::AppRouter;

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();

    // Conectamos a la base con URI + nombre de DB desde .env
    let db = MongoDB::new(&cfg.mongo_uri, &cfg.mongo_db).await;

    println!("✅ Conectado a MongoDB: {}", &cfg.mongo_db);

    let server = HttpServer::new(cfg.address(), AppRouter, db);
    println!("🚀 Servidor en http://{}", cfg.address());
    server.run();
}
