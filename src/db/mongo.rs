use super::Db;
use crate::{
    auth::claims::VerificationCode,
    domain::customer::{Customer, CustomerView},
};
use chrono::{Duration, Utc};
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use mongodb::{
    Client, Collection, Database,
    bson::{DateTime, Document, doc, oid::ObjectId},
};
use mongodb::{bson, error::Error as MongoError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct MongoDB {
    #[allow(dead_code)]
    client: Arc<Client>,
    db: Database,
}

pub struct PhoneSummary {
    pub primary_name: String, // nombre del primero
    pub phone: String,        // cuántos clientes comparten ese phone
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentDetails {
    #[serde(rename = "_id")] // Guardamos el ID del pago por si acaso
    pub id: ObjectId,
    pub rason: String,
    pub balance_bs: f64,
    pub status: String,
    // Guardamos la fecha completa original (con hora)
    pub full_date: DateTime,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct ResultGroupedByDate {
    #[serde(rename = "_id")] // El $group de MongoDB usa _id como clave
    pub date: String, // Esta será la fecha (ej: "2025-10-27")

    // Un vector de todos los pagos de ESE día
    pub payments: Vec<PaymentDetails>,
}

impl MongoDB {
    pub async fn new(uri: &str, db_name: &str) -> Self {
        let client = Client::with_uri_str(uri)
            .await
            .expect("Error conectando a MongoDB");
        let db = client.database(db_name);
        Self {
            client: Arc::new(client),
            db,
        }
    }

    /// Crea una nueva instancia de MongoDB con pool de conexiones optimizado
    pub async fn new_with_pool(cfg: &crate::config::Config) -> Result<Self, MongoError> {
        use mongodb::options::ClientOptions;
        use std::time::Duration as StdDuration;

        let mut client_options = ClientOptions::parse(&cfg.mongo_uri).await?;

        // ✅ Configuración del pool de conexiones
        client_options.max_pool_size = Some(cfg.mongo_pool_size);
        client_options.min_pool_size = Some(cfg.mongo_min_pool_size);
        client_options.connect_timeout = Some(StdDuration::from_secs(cfg.mongo_connect_timeout));
        client_options.server_selection_timeout = Some(StdDuration::from_secs(5));
        client_options.max_idle_time = Some(StdDuration::from_secs(600)); // 10 minutos

        // ✅ Retry automático
        client_options.retry_writes = Some(true);
        client_options.retry_reads = Some(true);

        // ✅ App name para identificación en logs de MongoDB
        client_options.app_name = Some("api-abdo".to_string());

        let client = Client::with_options(client_options)?;
        let db = client.database(&cfg.mongo_db);

        // ✅ Verificar conexión con ping
        tracing::info!("Verificando conexión a MongoDB...");
        client.database("admin")
            .run_command(doc! { "ping": 1 })
            .await?;

        tracing::info!("✅ MongoDB conectado con pool optimizado (max: {}, min: {})",
            cfg.mongo_pool_size, cfg.mongo_min_pool_size);

        Ok(Self {
            client: Arc::new(client),
            db,
        })
    }

    fn customers(&self) -> Collection<Document> {
        self.db.collection::<Document>("Clients")
    }

    fn verification_codes(&self) -> Collection<VerificationCode> {
        self.db.collection::<VerificationCode>("verification_codes")
    }
}

#[async_trait::async_trait]
impl Db for MongoDB {
    async fn find_customer_by_phone(&self, phone: &str) -> Option<Customer> {
        let filter = doc! { "sPhone": phone };
        let result = self.customers().find_one(filter).await.ok()??;

        Some(Customer {
            id: result.get_object_id("_id").ok()?.to_string(),
            full_name: result.get_str("sName").unwrap_or_default().to_string(),
            phone: result.get_str("sPhone").unwrap_or_default().to_string(),
        })
    }

    async fn find_customer_by_id(&self, id: &str) -> Option<CustomerView> {
        let obj_id = mongodb::bson::oid::ObjectId::parse_str(id).ok()?;
        let filter = doc! { "_id": obj_id };
        let result = self.customers().find_one(filter).await.ok()??;

        Some(CustomerView {
            full_name: result.get_str("sName").unwrap_or_default().to_string(),
            phone: result.get_str("sPhone").unwrap_or_default().to_string(),
        })
    }

    async fn summary_by_phone(&self, phone: &str) -> Option<PhoneSummary> {
        let pipeline = vec![
            doc! { "$match": { "sPhone": phone } },
            // Ordenamos para que el "primero" sea estable
            doc! { "$sort": { "_id": 1 } },
            doc! { "$group": {
                "_id": "$sPhone",
                "firstName": { "$first": "$sName" },
                "phone":     { "$first": "$sPhone" },
            }},
        ];

        let mut cursor = self.customers().aggregate(pipeline).await.ok()?;
        let Some(Ok(doc)) = cursor.next().await else {
            return None;
        };

        Some(PhoneSummary {
            primary_name: doc.get_str("firstName").unwrap_or_default().to_string(),
            phone: doc.get_str("phone").unwrap_or_default().to_string(),
        })
    }

    async fn store_verification_code(&self, phone: &str, code: &u32) -> mongodb::error::Result<()> {
        let now = Utc::now();
        let verification = VerificationCode {
            _id: None,
            phone: phone.to_string(),
            code: *code,
            created_at: now,
            expires_at: now + Duration::minutes(60),
        };

        self.verification_codes().insert_one(verification).await?;
        Ok(())
    }

    async fn find_verification_code(&self, phone: &str, code: &u32) -> Option<VerificationCode> {
        let filter = doc! { "phone": phone, "code": code };

        // .await devuelve Result<Option<VerificationCode>, Error>
        // .ok() lo convierte en Option<Option<VerificationCode>>
        // .flatten() lo aplana a Option<VerificationCode>
        self.verification_codes()
            .find_one(filter)
            .await
            .ok()
            .flatten()
    }

    async fn delete_verification_code(
        &self,
        id: &mongodb::bson::oid::ObjectId,
    ) -> Result<u64, mongodb::error::Error> {
        let filter = doc! { "_id": id };
        let result = self.verification_codes().delete_one(filter).await?;
        // Devolvemos el conteo de documentos borrados
        Ok(result.deleted_count)
    }

    async fn get_user_balance_usd(&self, id: String) -> Result<f64, MongoError> {
        // La colección 'Clients' debe tener _id, sPhone y nBalance
        let collection: mongodb::Collection<Document> = self.db.collection("Clients");

        // 1. Convertir String a ObjectId y manejar el error
        let obj_id = ObjectId::parse_str(&id)
            .map_err(|e| MongoError::custom(format!("Invalid ObjectId format: {}", e)))?;

        // 2. Definir la Pipeline de Agregación
        let pipeline = vec![
            // 1. Obtener el sPhone del usuario autenticado (Fase 1)
            doc! { "$match": { "_id": obj_id } },
            // 2. Extraer el valor del sPhone (Fase 1.5)
            doc! { "$lookup": {
                "from": "Clients", // Volver a consultar la misma colección
                "localField": "sPhone",
                "foreignField": "sPhone",
                "as": "client_group" // Crea un array con todos los clientes con ese sPhone
            }},
            // 3. Desanidar los clientes encontrados
            doc! { "$unwind": "$client_group" },
            // 4. Sumar el balance de todos los documentos en el grupo (Fase 2)
            doc! { "$group": {
                "_id": "$sPhone", // Agrupar por el teléfono
                "total_balance": { "$sum": "$client_group.nBalance" }
            }},
        ];

        // 3. Ejecutar la agregación
        let mut cursor = collection.aggregate(pipeline).await?;

        // 4. Leer el resultado (debe ser un solo documento con el balance total)
        if let Some(result) = cursor.try_next().await? {
            // El resultado es el documento de $group que contiene "total_balance"
            let total_balance = result.get_f64("total_balance").map_err(|_| {
                MongoError::custom("Field 'total_balance' not found or is not a number")
            })?;

            Ok(total_balance)
        } else {
            // No se encontró el documento inicial (el ID del token no existe)
            Err(MongoError::custom(
                "User document not found or no balance associated",
            ))
        }
    }

    async fn get_latest_exchange_rate(&self) -> Result<f64, mongodb::error::Error> {
        // 1. Inicialización de la colección (CORRECTO)
        let db_bcv = self.client.database("BCV");
        let collection: mongodb::Collection<Document> = db_bcv.collection("BCVRates");

        // 2. Cálculo de la medianoche (CORRECTO)
        let now = Utc::now();
        let start_of_day_chrono = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        let start_of_day_millis = start_of_day_chrono.timestamp_millis();
        let start_of_day_bson = DateTime::from_millis(start_of_day_millis);

        // 3. Definición del FILTRO (CORRECTO)
        let filter = doc! {
            "timestamp": { "$gte": start_of_day_bson }
        };

        // 4. Definición de Opciones
        // Creamos las opciones de búsqueda, PERO NO LAS USAMOS EN EL MÉTODO find() directamente.
        let options = mongodb::options::FindOptions::builder()
            .sort(doc! { "timestamp": -1 })
            .limit(1)
            .build();

        // 5. Buscar el documento.
        // ⬇️ CORRECCIÓN: Llamamos a find(filter) y encadenamos .with_options(options) ⬇️
        let mut cursor = collection
            .find(filter) // ⬅️ Solo el filtro (1 argumento)
            .with_options(options) // ⬅️ Aplicamos las opciones aquí
            .await?; // ⬅️ Y finalmente esperamos el resultado (el cursor)

        // 6. Obtener el primer resultado.
        let doc = cursor.try_next().await?;

        match doc {
            Some(d) => {
                // 7. Extracción del campo 'value'
                let rate = d.get_f64("value").map_err(|_| {
                    MongoError::custom("Rate field 'value' not found or invalid type")
                })?;

                Ok(rate)
            }
            None => Err(MongoError::custom(
                "No exchange rate found for today in BCV collection",
            )),
        }
    }

    async fn get_last_payments_by_id(
        &self,
        id: String,
    ) -> Result<Vec<ResultGroupedByDate>, MongoError> {
        // 1. Convertir String a ObjectId y manejar el error
        let obj_id = ObjectId::parse_str(&id)
            .map_err(|e| MongoError::custom(format!("Invalid ObjectId format: {}", e)))?;

        let pipeline = vec![
            // 1-6. Encuentra clientes, busca 10 pagos por cliente, desanida
            doc! { "$match": { "_id": obj_id } },
            doc! { "$lookup": {
                "from": "Clients",
                "localField": "sPhone",
                "foreignField": "sPhone",
                "as": "client_group"
            }},
            doc! { "$unwind": "$client_group" },
            doc! { "$replaceRoot": { "newRoot": "$client_group" } },
            doc! { "$lookup": {
                "from": "Payments",
                "let": { "client_id": "$_id" },
                "pipeline": [
                    doc! { "$match": { "$expr": { "$eq": ["$idClient", "$$client_id"] } }},
                    doc! { "$sort": { "dCreation": -1 } },
                    doc! { "$limit": 10 }
                ],
                "as": "recent_payments"
            }},
            doc! { "$unwind": "$recent_payments" },
            // 7. Establece el pago como raíz
            doc! { "$replaceRoot": { "newRoot": "$recent_payments" } },
            // 8. PRE-PROYECCIÓN: Prepara los campos que necesitamos
            doc! { "$project": {
                "_id": 1,
                "rason": "$sReason",
                "balance_bs": "$nBs",
                "status": "$sState",
                "full_date": "$dCreation", // Mantenemos la fecha/hora completa
                "date_group_key": { // Creamos la clave SÓLO de fecha (YYYY-MM-DD)
                    "$dateToString": {
                        "format": "%Y-%m-%d",
                        "date": "$dCreation",
                        "timezone": "America/Caracas" // <-- Importante: ajusta tu zona horaria
                    }
                }
            }},
            // 9. ORDENAR (Pre-agrupación): Ordena TODOS los pagos
            // Esto asegura que $push (en el paso 10) respete el orden
            doc! { "$sort": { "full_date": -1 } },
            // 10. ¡AGRUPAR!
            doc! { "$group": {
                "_id": "$date_group_key", // Agrupa por la fecha (YYYY-MM-DD)
                "payments": { "$push": {
                    // Construye el objeto 'PaymentDetails'
                    "_id": "$_id",
                    "rason": "$rason",
                    "balance_bs": "$balance_bs",
                    "status": "$status",
                    "full_date": "$full_date"
                } }
            }},
            // 11. ORDENAR (Post-agrupación): Ordena los GRUPOS
            doc! { "$sort": { "_id": -1 } }, // Ordena los grupos por fecha
            // 12. LÍMITE: Limita a los 10 días más recientes
            doc! { "$limit": 10 },
        ];

        let client_collection = self.db.collection::<mongodb::bson::Document>("Clients");

        // Apunta al nuevo struct de resultado
        let mut cursor = client_collection
            .aggregate(pipeline) // <-- Sin genéricos
            .await?;

        let mut results: Vec<ResultGroupedByDate> = Vec::new();

        // .try_next() viene de `use futures::stream::TryStreamExt;`
        while let Some(doc) = cursor.try_next().await? {
            // Deserializamos el Document BSON en nuestro struct
            let item: ResultGroupedByDate = bson::from_document(doc).map_err(|e| {
                MongoError::from(
                    // Manejamos el error de deserialización
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()),
                )
            })?;

            results.push(item);
        }

        Ok(results)
    }

    async fn find_client_by_user_id(&self, user_id: &str) -> Result<Option<crate::profile::structers::Client>, String> {
        let obj_id = ObjectId::parse_str(user_id).map_err(|e| e.to_string())?;
        let filter = doc! { "_id": obj_id };

        match self.customers().find_one(filter).await {
            Ok(Some(doc)) => {
                let client = crate::profile::structers::Client {
                    _id: doc.get_object_id("_id").map(|o| o.to_string()).unwrap_or_default(),
                    s_phone: doc.get_str("sPhone").unwrap_or_default().to_string(),
                };
                Ok(Some(client))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn find_clients_by_phone(&self, s_phone: &str) -> Result<Vec<crate::profile::structers::Client>, String> {
        let filter = doc! { "sPhone": s_phone };
        let mut cursor = self.customers().find(filter).await.map_err(|e| e.to_string())?;
        let mut clients = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let client = crate::profile::structers::Client {
                _id: doc.get_object_id("_id").map(|o| o.to_string()).unwrap_or_default(),
                s_phone: doc.get_str("sPhone").unwrap_or_default().to_string(),
            };
            clients.push(client);
        }

        Ok(clients)
    }

    async fn find_debts_by_client_ids(&self, client_ids: &[ObjectId]) -> Result<Vec<crate::profile::structers::Debt>, String> {
        let filter = doc! { "idClient": { "$in": client_ids } };
        let collection = self.db.collection::<Document>("Debts");
        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut debts = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let debt = crate::profile::structers::Debt {
                _id: doc.get_object_id("_id").map(|o| o.to_string()).unwrap_or_default(),
                n_amount: doc.get_f64("nAmount").unwrap_or(0.0),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
                id_client: doc.get_object_id("idClient").map(|o| o.to_string()).unwrap_or_default(),
                s_reason: doc.get_str("sReason").unwrap_or_default().to_string(),
            };
            debts.push(debt);
        }

        Ok(debts)
    }

    async fn find_part_payments_by_debt_ids(&self, debt_ids: &[ObjectId]) -> Result<Vec<crate::profile::structers::PartPayment>, String> {
        let filter = doc! { "idDebt": { "$in": debt_ids } };
        let collection = self.db.collection::<Document>("PartPayment");
        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut part_payments = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let pp = crate::profile::structers::PartPayment {
                _id: doc.get_object_id("_id").map(|o| o.to_string()).unwrap_or_default(),
                id_debt: doc.get_object_id("idDebt").map(|o| o.to_string()).unwrap_or_default(),
                id_payment: doc.get_object_id("idPayment").map(|o| o.to_string()).unwrap_or_default(),
                n_amount: doc.get_f64("nAmount").unwrap_or(0.0),
            };
            part_payments.push(pp);
        }

        Ok(part_payments)
    }

    async fn find_payments_by_ids(&self, payment_ids: &[ObjectId]) -> Result<Vec<crate::profile::structers::Payment>, String> {
        let filter = doc! { "_id": { "$in": payment_ids } };
        let collection = self.db.collection::<Document>("Payments");
        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut payments = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let payment = crate::profile::structers::Payment {
                _id: doc.get_object_id("_id").map(|o| o.to_string()).unwrap_or_default(),
                n_amount: doc.get_f64("nAmount").unwrap_or(0.0),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
            };
            payments.push(payment);
        }

        Ok(payments)
    }
}
