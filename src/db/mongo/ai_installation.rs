//! Implementación MongoDB de `AiInstallationRepository`.
//!
//! Colección `AiInstallationConfigs` — máximo 2 docs (uno por `ConnectionType`).
//! Upsert idempotente con `filter: { connection_type: <slug> }`.

use async_trait::async_trait;
use futures::stream::TryStreamExt;
use mongodb::bson::{doc, DateTime as BsonDateTime};
use mongodb::options::{FindOneAndReplaceOptions, ReturnDocument};

use super::MongoDB;
use crate::db::AiInstallationRepository;
use crate::models::ai_agent::{AiInstallationConfig, ConnectionType};

const COLLECTION: &str = "AiInstallationConfigs";

impl MongoDB {
    fn ai_installations(&self) -> mongodb::Collection<AiInstallationConfig> {
        self.db.collection::<AiInstallationConfig>(COLLECTION)
    }
}

#[async_trait]
impl AiInstallationRepository for MongoDB {
    async fn get_ai_installation(
        &self,
        connection_type: ConnectionType,
    ) -> Result<Option<AiInstallationConfig>, String> {
        self.ai_installations()
            .find_one(doc! { "connection_type": connection_type.as_slug() })
            .await
            .map_err(|e| format!("ai_installation_find_one: {e}"))
    }

    async fn list_ai_installations(&self) -> Result<Vec<AiInstallationConfig>, String> {
        self.ai_installations()
            .find(doc! {})
            .sort(doc! { "connection_type": 1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn upsert_ai_installation(
        &self,
        mut config: AiInstallationConfig,
    ) -> Result<AiInstallationConfig, String> {
        config.updated_at = BsonDateTime::now();
        let slug = config.connection_type.as_slug().to_string();

        let opts = FindOneAndReplaceOptions::builder()
            .upsert(true)
            .return_document(ReturnDocument::After)
            .build();

        self.ai_installations()
            .find_one_and_replace(doc! { "connection_type": &slug }, config)
            .with_options(opts)
            .await
            .map_err(|e| format!("ai_installation_upsert: {e}"))?
            .ok_or_else(|| "ai_installation_upsert: no document returned after upsert".to_string())
    }
}
