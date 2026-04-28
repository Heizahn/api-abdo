//! Implementación MongoDB de `AiAgentRepository`.
//!
//! Colecciones:
//! - `AiAgents` — un doc por agente. `workspace_ids` es array multikey.
//! - `AiAgentFaqs` — FAQs por `agent_id`.

use async_trait::async_trait;
use futures::stream::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId, Document};
use mongodb::options::{FindOneAndReplaceOptions, ReturnDocument};
use mongodb::Collection;

use super::MongoDB;
use crate::db::AiAgentRepository;
use crate::models::ai_agent::{AiAgent, AiAgentFaq};

impl MongoDB {
    fn ai_agents(&self) -> Collection<AiAgent> {
        self.db.collection::<AiAgent>("AiAgents")
    }

    fn ai_agent_faqs(&self) -> Collection<AiAgentFaq> {
        self.db.collection::<AiAgentFaq>("AiAgentFaqs")
    }
}

#[async_trait]
impl AiAgentRepository for MongoDB {
    async fn list_ai_agents(
        &self,
        workspace_id: Option<&ObjectId>,
    ) -> Result<Vec<AiAgent>, String> {
        let filter = match workspace_id {
            Some(oid) => doc! { "workspace_ids": oid },
            None => doc! {},
        };
        self.ai_agents()
            .find(filter)
            .sort(doc! { "created_at": -1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_ai_agent_by_id(&self, id: &ObjectId) -> Result<Option<AiAgent>, String> {
        self.ai_agents()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_ai_agent(&self, mut agent: AiAgent) -> Result<AiAgent, String> {
        let res = self
            .ai_agents()
            .insert_one(&agent)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(oid) = res.inserted_id.as_object_id() {
            agent.id = Some(oid);
        }
        Ok(agent)
    }

    async fn replace_ai_agent(
        &self,
        id: &ObjectId,
        mut agent: AiAgent,
    ) -> Result<Option<AiAgent>, String> {
        agent.id = Some(*id);
        let opts = FindOneAndReplaceOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.ai_agents()
            .find_one_and_replace(doc! { "_id": id }, agent)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_ai_agent(&self, id: &ObjectId) -> Result<bool, String> {
        // Cascada: borramos FAQs del agente antes para no dejar huérfanas.
        // Best-effort — si falla, no bloqueamos el delete del agente.
        let _ = self
            .ai_agent_faqs()
            .delete_many(doc! { "agent_id": id })
            .await;
        let res = self
            .ai_agents()
            .delete_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.deleted_count > 0)
    }

    async fn list_ai_agent_faqs(
        &self,
        agent_id: &ObjectId,
    ) -> Result<Vec<AiAgentFaq>, String> {
        self.ai_agent_faqs()
            .find(doc! { "agent_id": agent_id })
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
