use super::MongoDB;
use crate::db::UtilsRepository;
use crate::error::ApiError;
use crate::models::db::LatestVersion;
use crate::models::zabbix::ZabbixLookupResult;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use mongodb::{
    bson::{doc, oid::ObjectId, Document},
    error::Error as MongoError,
    Collection,
};

#[async_trait]
impl UtilsRepository for MongoDB {
    async fn find_latest_version(&self) -> Result<Option<LatestVersion>, String> {
        let collection = self.db.collection::<LatestVersion>("VersionCode");
        collection
            .find_one(doc! {})
            .await
            .map_err(|e| e.to_string())
    }

    async fn exists_rate_for_date(
        &self,
        date_start: DateTime<Utc>,
        date_end: DateTime<Utc>,
    ) -> Result<bool, String> {
        let db_bcv = self.client.database("BCV");
        let collection: Collection<Document> = db_bcv.collection("BCVRates");

        // Convertir a BSON DateTime
        let start_bson = mongodb::bson::DateTime::from_millis(date_start.timestamp_millis());
        let end_bson = mongodb::bson::DateTime::from_millis(date_end.timestamp_millis());

        // Buscar si existe un documento en ese rango de tiempo
        let filter = doc! {
            "timestamp": {
                "$gte": start_bson,
                "$lte": end_bson
            }
        };

        let count = collection
            .count_documents(filter)
            .await
            .map_err(|e| e.to_string())?;
        Ok(count > 0)
    }

    async fn save_exchange_rate(&self, rate: f64, date: DateTime<Utc>) -> Result<(), MongoError> {
        let db_bcv = self.client.database("BCV");
        let collection: Collection<Document> = db_bcv.collection("BCVRates");

        let doc = doc! {
            "value": rate,
            "timestamp": mongodb::bson::DateTime::from_millis(date.timestamp_millis()),
        };

        collection.insert_one(doc).await?;
        tracing::info!("💾 Tasa BCV guardada exitosamente: {}", rate);
        Ok(())
    }

    async fn find_client_olt_position(
        &self,
        client_id: &str,
    ) -> Result<(String, String), ApiError> {
        // 1. Casteamos el ID (¡Con el ? al final!)
        let obj_id = ObjectId::parse_str(client_id).map_err(|_| {
            ApiError::DatabaseError("El ID proporcionado no tiene el formato adecuado".to_string())
        })?;

        // Usamos Document genérico porque aggregate devuelve Documents
        let collection = self.db.collection::<Document>("Clients");

        // 2. Construimos el Pipeline de Agregación
        let pipeline = vec![
            // Paso 1: Buscar al cliente por su ID
            doc! { "$match": { "_id": obj_id } },
            // Paso 2: JOIN con la colección Onus usando idOnu
            doc! {
                "$lookup": {
                    "from": "Onus",        // OJO: Cambia si tu colección se llama distinto (ej: "onus")
                    "localField": "idOnu", // Campo en Clients
                    "foreignField": "_id", // Campo en Onus
                    "as": "onu_data"
                }
            },
            doc! { "$unwind": "$onu_data" }, // Desempaqueta el array del JOIN
            // Paso 3: JOIN con la colección Olts usando idOlt de la ONU
            doc! {
                "$lookup": {
                    "from": "Olts",               // OJO: Cambia si tu colección se llama distinto
                    "localField": "onu_data.idOlt",
                    "foreignField": "_id",
                    "as": "olt_data"
                }
            },
            doc! { "$unwind": "$olt_data" }, // Desempaqueta el array del JOIN
            // Paso 4: Validar que sNameZabbix exista y no esté vacío (si no, Mongo lo descarta)
            doc! {
                "$match": {
                    "olt_data.sNameZabbix": { "$exists": true, "$ne": null, "$ne": "" }
                }
            },
            // Paso 5: Proyección (Extraer solo lo que necesitamos para ahorrar RAM)
            doc! {
                "$project": {
                    "_id": 0,
                    "nPon": "$onu_data.nPon",
                    "nIdOnu": "$onu_data.nIdOnu",
                    "sNameZabbix": "$olt_data.sNameZabbix"
                }
            },
        ];

        // 3. Ejecutar la agregación
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| ApiError::DatabaseError(format!("Error en consulta a la BD: {}", e)))?;

        // 4. Leer el primer resultado del cursor
        if let Some(doc) = cursor
            .try_next()
            .await
            .map_err(|_| ApiError::DatabaseError("Error al leer datos".to_string()))?
        {
            // Deserializamos el documento BSON a nuestra estructura de Rust
            let res: ZabbixLookupResult = mongodb::bson::from_document(doc)
                .map_err(|_| ApiError::Internal("Error al parsear datos de la BD".to_string()))?;

            // 5. Construir el Zabbix Code (Ej: GPON03ONU13)
            // El {:02} asegura que si el PON es 3, le ponga un 0 a la izquierda (03). Si es 11, lo deja como 11.
            let client_zabbix_code = format!("GPON{:02}ONU{:02}", res.n_pon, res.n_id_onu);

            Ok((client_zabbix_code, res.s_name_zabbix))
        } else {
            // Si el cursor está vacío significa que: o el cliente no existe, o no tiene ONU,
            // o su OLT no tiene configurado el 'sNameZabbix'. Retornamos 404.
            Err(ApiError::NotFound)
        }
    }
}
