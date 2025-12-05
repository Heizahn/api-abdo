use super::MongoDB;
use crate::db::UtilsRepository;
use crate::models::db::LatestVersion;
use async_trait::async_trait;
use mongodb::bson::doc;

#[async_trait]
impl UtilsRepository for MongoDB {
    async fn find_latest_version(&self) -> Result<Option<LatestVersion>, String> {
        let collection = self.db.collection::<LatestVersion>("VersionCode");
        collection
            .find_one(doc! {})
            .await
            .map_err(|e| e.to_string())
    }
}
