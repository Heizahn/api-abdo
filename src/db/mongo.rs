use super::Db;
use crate::{
    auth::claims::VerificationCode,
    domain::customer::{Customer, CustomerView},
};
use chrono::{Duration, Utc};
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use mongodb::error::Error as MongoError;
use mongodb::{
    Client, Collection, Database,
    bson::{DateTime, Document, doc, oid::ObjectId},
};
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
}
