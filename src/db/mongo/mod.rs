pub mod auth;
pub mod onu;
pub mod profile;
pub mod sales;
pub mod users;
pub mod utils;

use crate::db::Db;
use mongodb::bson::oid::ObjectId;
use mongodb::error::Error as MongoError;
use mongodb::{
    bson::{doc, DateTime, Document},
    Client, Collection, Database,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// Importamos modelos para los helpers de colecciones
use crate::auth::claims::VerificationCode;
use crate::models::payment::PaymentMethod;

// ============================================
// Structs Auxiliares (Públicos para el Trait)
// ============================================

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentDetails {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    pub reason: String,
    pub balance_bs: f64,
    pub status: String,
    pub full_date: DateTime,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResultGroupedByDate {
    #[serde(rename = "_id")]
    pub date: String,
    pub payments: Vec<PaymentDetails>,
}

// ============================================
// Struct Principal MongoDB
// ============================================
#[derive(Clone)]
pub struct MongoDB {
    #[allow(dead_code)]
    pub client: Arc<Client>,
    pub db: Database,
}

impl MongoDB {
    pub async fn new_with_pool(cfg: &crate::config::Config) -> Result<Self, MongoError> {
        use mongodb::options::ClientOptions;
        use std::time::Duration as StdDuration;

        let mut client_options = ClientOptions::parse(&cfg.mongo_uri).await?;

        // ✅ Configuración del pool
        client_options.max_pool_size = Some(cfg.mongo_pool_size);
        client_options.min_pool_size = Some(cfg.mongo_min_pool_size);
        client_options.connect_timeout = Some(StdDuration::from_secs(cfg.mongo_connect_timeout));
        client_options.server_selection_timeout = Some(StdDuration::from_secs(5));
        client_options.max_idle_time = Some(StdDuration::from_secs(600));
        client_options.retry_writes = Some(true);
        client_options.retry_reads = Some(true);
        client_options.app_name = Some("api-abdo".to_string());

        let client = Client::with_options(client_options)?;
        let db = client.database(&cfg.mongo_db);

        tracing::info!("Verificando conexión a MongoDB...");
        client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await?;

        tracing::info!(
            "✅ MongoDB conectado con pool optimizado (max: {}, min: {})",
            cfg.mongo_pool_size,
            cfg.mongo_min_pool_size
        );

        Ok(Self {
            client: Arc::new(client),
            db,
        })
    }

    // ============================================
    // Helpers para Colecciones (Internal Use)
    // ============================================

    pub(crate) fn customers(&self) -> Collection<Document> {
        self.db.collection::<Document>("Clients")
    }

    pub(crate) fn verification_codes(&self) -> Collection<VerificationCode> {
        self.db.collection::<VerificationCode>("verification_codes")
    }

    // Estos helpers quedan listos para cuando implementes la lógica nueva
    #[allow(dead_code)]
    pub(crate) fn payment_methods(&self) -> Collection<PaymentMethod> {
        self.db.collection::<PaymentMethod>("PaymentsMethods")
    }
}

// Implementación vacía del Trait Maestro (los submódulos hacen el trabajo)
impl Db for MongoDB {}
