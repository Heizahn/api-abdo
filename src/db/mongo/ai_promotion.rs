//! Implementación MongoDB de `AiPromotionRepository`.
//!
//! Colección `AiPromotions`. `list_active_ai_promotions` filtra por
//! `is_active=true` y `starts_at <= now <= ends_at`.

use async_trait::async_trait;
use futures::stream::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId};
use mongodb::options::{FindOneAndReplaceOptions, ReturnDocument};

use super::MongoDB;
use crate::db::AiPromotionRepository;
use crate::models::ai_agent::AiPromotion;

const COLLECTION: &str = "AiPromotions";

impl MongoDB {
    fn ai_promotions(&self) -> mongodb::Collection<AiPromotion> {
        self.db.collection::<AiPromotion>(COLLECTION)
    }
}

#[async_trait]
impl AiPromotionRepository for MongoDB {
    async fn list_ai_promotions(&self) -> Result<Vec<AiPromotion>, String> {
        self.ai_promotions()
            .find(doc! {})
            .sort(doc! { "starts_at": -1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_active_ai_promotions(
        &self,
        now: mongodb::bson::DateTime,
    ) -> Result<Vec<AiPromotion>, String> {
        self.ai_promotions()
            .find(doc! {
                "is_active": true,
                "starts_at": { "$lte": now },
                "ends_at":   { "$gte": now },
            })
            .sort(doc! { "ends_at": 1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_ai_promotion_by_id(
        &self,
        id: &ObjectId,
    ) -> Result<Option<AiPromotion>, String> {
        self.ai_promotions()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_ai_promotion(&self, mut promo: AiPromotion) -> Result<AiPromotion, String> {
        let res = self
            .ai_promotions()
            .insert_one(&promo)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(oid) = res.inserted_id.as_object_id() {
            promo.id = Some(oid);
        }
        Ok(promo)
    }

    async fn replace_ai_promotion(
        &self,
        id: &ObjectId,
        mut promo: AiPromotion,
    ) -> Result<Option<AiPromotion>, String> {
        promo.id = Some(*id);
        let opts = FindOneAndReplaceOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.ai_promotions()
            .find_one_and_replace(doc! { "_id": id }, promo)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_ai_promotion(&self, id: &ObjectId) -> Result<bool, String> {
        let res = self
            .ai_promotions()
            .delete_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.deleted_count > 0)
    }
}
