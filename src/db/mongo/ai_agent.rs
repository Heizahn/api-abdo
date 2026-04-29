//! Implementación MongoDB de `AiAgentRepository`.
//!
//! Colecciones:
//! - `AiAgents` — un doc por agente. `workspace_ids` es array multikey.
//! - `AiAgentFaqs` — FAQs por `agent_id`.

use async_trait::async_trait;
use futures::stream::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId, Document};
use mongodb::options::{FindOneAndReplaceOptions, FindOptions, ReturnDocument};
use mongodb::Collection;

use super::MongoDB;
use crate::db::AiAgentRepository;
use crate::models::ai_agent::{AiAgent, AiAgentFaq, AiCoverageZone, AiInteraction, AiPlan};
use crate::models::whatsapp::WaMessage;

impl MongoDB {
    fn ai_agents(&self) -> Collection<AiAgent> {
        self.db.collection::<AiAgent>("AiAgents")
    }

    fn ai_agent_faqs(&self) -> Collection<AiAgentFaq> {
        self.db.collection::<AiAgentFaq>("AiAgentFaqs")
    }

    fn ai_interactions(&self) -> Collection<AiInteraction> {
        self.db.collection::<AiInteraction>("AiInteractions")
    }

    fn ai_plans(&self) -> Collection<AiPlan> {
        self.db.collection::<AiPlan>("AiPlans")
    }

    fn ai_coverage_zones(&self) -> Collection<AiCoverageZone> {
        self.db.collection::<AiCoverageZone>("AiCoverageZones")
    }

    fn wa_messages_for_history(&self) -> Collection<WaMessage> {
        self.db.collection::<WaMessage>("WaMessages")
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

    async fn find_ai_agents_by_ids(&self, ids: &[ObjectId]) -> Result<Vec<AiAgent>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.ai_agents()
            .find(doc! { "_id": { "$in": ids } })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_receptionist_for_workspace(
        &self,
        workspace_id: &ObjectId,
    ) -> Result<Option<AiAgent>, String> {
        self.ai_agents()
            .find_one(doc! {
                "workspace_ids": workspace_id,
                "enabled": true,
                "is_receptionist": true,
            })
            .sort(doc! { "created_at": 1 })
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_active_agent_for_workspace(
        &self,
        workspace_id: &ObjectId,
    ) -> Result<Option<AiAgent>, String> {
        self.ai_agents()
            .find_one(doc! { "workspace_ids": workspace_id, "enabled": true })
            .sort(doc! { "created_at": 1 })
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

    async fn create_ai_interaction(&self, interaction: AiInteraction) -> Result<(), String> {
        self.ai_interactions()
            .insert_one(&interaction)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ─── AiPlans ────────────────────────────────────────────────────────────

    async fn list_ai_plans(&self, only_active: bool) -> Result<Vec<AiPlan>, String> {
        let filter = if only_active { doc! { "active": true } } else { doc! {} };
        self.ai_plans()
            .find(filter)
            .sort(doc! { "display_order": 1, "mbps": 1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_ai_plan_by_id(&self, id: &ObjectId) -> Result<Option<AiPlan>, String> {
        self.ai_plans()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_ai_plan(&self, mut plan: AiPlan) -> Result<AiPlan, String> {
        let res = self
            .ai_plans()
            .insert_one(&plan)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(oid) = res.inserted_id.as_object_id() {
            plan.id = Some(oid);
        }
        Ok(plan)
    }

    async fn replace_ai_plan(
        &self,
        id: &ObjectId,
        mut plan: AiPlan,
    ) -> Result<Option<AiPlan>, String> {
        plan.id = Some(*id);
        let opts = FindOneAndReplaceOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.ai_plans()
            .find_one_and_replace(doc! { "_id": id }, plan)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_ai_plan(&self, id: &ObjectId) -> Result<bool, String> {
        let res = self
            .ai_plans()
            .delete_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.deleted_count > 0)
    }

    async fn ai_plans_is_empty(&self) -> Result<bool, String> {
        let count = self
            .ai_plans()
            .count_documents(doc! {})
            .limit(1)
            .await
            .map_err(|e| e.to_string())?;
        Ok(count == 0)
    }

    // ─── AiCoverageZones ────────────────────────────────────────────────────

    async fn list_ai_coverage_zones(
        &self,
        only_active: bool,
    ) -> Result<Vec<AiCoverageZone>, String> {
        let filter = if only_active { doc! { "active": true } } else { doc! {} };
        self.ai_coverage_zones()
            .find(filter)
            .sort(doc! { "name": 1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_ai_coverage_zone_by_id(
        &self,
        id: &ObjectId,
    ) -> Result<Option<AiCoverageZone>, String> {
        self.ai_coverage_zones()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_ai_coverage_zone(
        &self,
        mut zone: AiCoverageZone,
    ) -> Result<AiCoverageZone, String> {
        let res = self
            .ai_coverage_zones()
            .insert_one(&zone)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(oid) = res.inserted_id.as_object_id() {
            zone.id = Some(oid);
        }
        Ok(zone)
    }

    async fn replace_ai_coverage_zone(
        &self,
        id: &ObjectId,
        mut zone: AiCoverageZone,
    ) -> Result<Option<AiCoverageZone>, String> {
        zone.id = Some(*id);
        let opts = FindOneAndReplaceOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.ai_coverage_zones()
            .find_one_and_replace(doc! { "_id": id }, zone)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_ai_coverage_zone(&self, id: &ObjectId) -> Result<bool, String> {
        let res = self
            .ai_coverage_zones()
            .delete_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.deleted_count > 0)
    }

    async fn ai_coverage_zones_is_empty(&self) -> Result<bool, String> {
        let count = self
            .ai_coverage_zones()
            .count_documents(doc! {})
            .limit(1)
            .await
            .map_err(|e| e.to_string())?;
        Ok(count == 0)
    }

    async fn list_recent_messages_for_conversation(
        &self,
        conversation_id: &ObjectId,
        limit: i64,
    ) -> Result<Vec<WaMessage>, String> {
        // Sort por `_id` (ObjectId) — refleja el orden de inserción en el
        // back, no el `timestamp` de Meta. Necesario porque inbounds de Meta
        // pueden tener timestamp menor que el outbound persistido un instante
        // después (cuando el cliente manda un mensaje mientras el bot
        // responde). El caller dispatch usa este orden para identificar
        // ráfagas pendientes correctamente.
        let opts = FindOptions::builder()
            .sort(doc! { "_id": -1 })
            .limit(limit.max(1))
            .build();
        let mut items: Vec<WaMessage> = self
            .wa_messages_for_history()
            .find(doc! { "conversation_id": conversation_id })
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_collect()
            .await
            .map_err(|e| e.to_string())?;
        items.reverse();
        Ok(items)
    }
}
