use super::MongoDB;
use crate::models::db::LatestVersion;
use crate::services::zte_parse_update::OnuDetected;
use crate::{db::UtilsRepository, models::db::OnuIdentity};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use mongodb::{
    bson::{doc, Document},
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

    async fn get_device_serial_numbers(&self) -> Result<Vec<OnuIdentity>, String> {
        // 1. Apunta a la colección correcta.
        let collection: Collection<Document> = self.db.collection("Onus");

        // 2. Proyección: Solo traer _id y sSn (ahorra memoria y ancho de banda)
        let mut cursor = collection
            .find(doc! {})
            .projection(doc! {
                "_id": 1,
                "sSn": 1,
                "sMac": 1,
                "nMotherboard": 1,
                "nPon": 1,
                "nIdOnu": 1,
                "idOlt": 1,
            })
            .await
            .map_err(|e| e.to_string())?;

        let mut devices = Vec::new();

        // 3. Iterar el cursor de forma asíncrona
        while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
            // Extraemos el _id (ObjectId) y lo dejamos como ObjectId
            let id = doc.get_object_id("_id");
            let sn = doc.get_str("sSn");
            let mac = doc.get_str("sMac");
            let motherboard = doc.get_i32("nMotherboard");
            let pon = doc.get_i32("nPon");
            let id_onu = doc.get_i32("nIdOnu");
            let id_olt = doc.get_object_id("idOlt");

            if let (Ok(id), Ok(sn)) = (id, sn) {
                devices.push(OnuIdentity {
                    id,
                    sn: sn.to_string(),
                    mac: mac.ok().map(|s| s.to_string()),
                    motherboard: motherboard.ok(),
                    pon: pon.ok(),
                    id_onu: id_onu.ok(),
                    id_olt: id_olt.ok(),
                });
            }
        }

        Ok(devices)
    }

    async fn save_onu_from_zte(&self, onu: OnuDetected, id_editor: &str) -> Result<(), String> {
        // Placeholder implementation
        // Upsert or Insert the serial number
        let collection: Collection<Document> = self.db.collection("Onus");

        // Example: Update the "last_checked" or similar, or just ensure it exists
        // The user said "guardar las sn en la db", so maybe we are scraping them FROM ZTE and saving locally?
        // If so, upsert is best.

        let filter = doc! { "_id": &onu.id };
        let update = doc! {
            "$set": {
                "sMac": &onu.mac,
                "nMotherboard": &onu.motherboard,
                "nPon": &onu.pon,
                "nIdOnu": &onu.id_onu,
                "idOlt": &onu.id_olt,
                "dEdition": mongodb::bson::DateTime::now(),
                "idEditor": id_editor
            }
        };
        let options = mongodb::options::UpdateOptions::builder()
            .upsert(true)
            .build();

        collection
            .update_one(filter, update)
            .with_options(options)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }
}
