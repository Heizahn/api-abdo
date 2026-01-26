use super::MongoDB;
use crate::db::UtilsRepository;
use crate::models::db::LatestVersion;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
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
}
