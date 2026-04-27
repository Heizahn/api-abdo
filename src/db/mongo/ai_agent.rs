//! Implementación MongoDB de `AiAgentRepository`.
//!
//! Colecciones:
//! - `AiAgentSettings` — única por `workspace_id` (índice unique).
//! - `AiAgentFaqs` — varias por workspace.

use async_trait::async_trait;
use futures::stream::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId, Document};
use mongodb::options::{FindOneAndReplaceOptions, ReturnDocument};
use mongodb::Collection;

use super::MongoDB;
use crate::db::AiAgentRepository;
use crate::models::ai_agent::{AiAgentFaq, AiAgentSetting};

impl MongoDB {
    fn ai_agent_settings(&self) -> Collection<AiAgentSetting> {
        self.db.collection::<AiAgentSetting>("AiAgentSettings")
    }

    fn ai_agent_faqs(&self) -> Collection<AiAgentFaq> {
        self.db.collection::<AiAgentFaq>("AiAgentFaqs")
    }
}

#[async_trait]
impl AiAgentRepository for MongoDB {
    async fn find_ai_agent_setting_by_workspace(
        &self,
        workspace_id: &ObjectId,
    ) -> Result<Option<AiAgentSetting>, String> {
        self.ai_agent_settings()
            .find_one(doc! { "workspace_id": workspace_id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_ai_agent_settings(&self) -> Result<Vec<AiAgentSetting>, String> {
        self.ai_agent_settings()
            .find(doc! {})
            .sort(doc! { "created_at": -1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_ai_agent_setting(
        &self,
        mut setting: AiAgentSetting,
    ) -> Result<AiAgentSetting, String> {
        let res = self
            .ai_agent_settings()
            .insert_one(&setting)
            .await
            .map_err(|e| {
                // El índice único `workspace_id` puede disparar duplicate-key
                // (E11000). Lo mapeamos a un código estable para que el
                // handler pueda servir 409 sin parsear el mensaje.
                if e.to_string().contains("E11000") {
                    "workspace_id_already_exists".to_string()
                } else {
                    e.to_string()
                }
            })?;
        if let Some(oid) = res.inserted_id.as_object_id() {
            setting.id = Some(oid);
        }
        Ok(setting)
    }

    async fn replace_ai_agent_setting(
        &self,
        id: &ObjectId,
        mut setting: AiAgentSetting,
    ) -> Result<Option<AiAgentSetting>, String> {
        // El replace preserva `_id` y debe respetar `created_at` original.
        // El caller pasa el doc con `created_at` ya intacto.
        setting.id = Some(*id);
        let opts = FindOneAndReplaceOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.ai_agent_settings()
            .find_one_and_replace(doc! { "_id": id }, setting)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_ai_agent_faqs(
        &self,
        workspace_id: &ObjectId,
    ) -> Result<Vec<AiAgentFaq>, String> {
        self.ai_agent_faqs()
            .find(doc! { "workspace_id": workspace_id })
            .sort(doc! { "created_at": -1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_ai_agent_faq_by_id(
        &self,
        id: &ObjectId,
    ) -> Result<Option<AiAgentFaq>, String> {
        self.ai_agent_faqs()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_ai_agent_faq(&self, mut faq: AiAgentFaq) -> Result<AiAgentFaq, String> {
        let res = self
            .ai_agent_faqs()
            .insert_one(&faq)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(oid) = res.inserted_id.as_object_id() {
            faq.id = Some(oid);
        }
        Ok(faq)
    }

    async fn update_ai_agent_faq(
        &self,
        id: &ObjectId,
        question: Option<String>,
        answer: Option<String>,
        tags: Option<Vec<String>>,
    ) -> Result<Option<AiAgentFaq>, String> {
        let mut set = Document::new();
        if let Some(q) = question {
            set.insert("question", q);
        }
        if let Some(a) = answer {
            set.insert("answer", a);
        }
        if let Some(t) = tags {
            set.insert("tags", t);
        }
        // `updated_at` se toca siempre, aún si el patch viene vacío
        // (mantiene la regla "el doc fue tocado por última vez ahora").
        set.insert("updated_at", mongodb::bson::DateTime::now());

        let opts = mongodb::options::FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.ai_agent_faqs()
            .find_one_and_update(doc! { "_id": id }, doc! { "$set": set })
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_ai_agent_faq(&self, id: &ObjectId) -> Result<bool, String> {
        let res = self
            .ai_agent_faqs()
            .delete_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.deleted_count > 0)
    }
}
