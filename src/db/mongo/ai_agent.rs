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
use std::collections::HashMap;

use super::MongoDB;
use crate::db::{
    AiAgentMetricsDailyBucket, AiAgentMetricsRaw, AiAgentMetricsSummary, AiAgentRepository,
    MetricsGranularity,
};
use crate::models::ai_agent::{AiAgent, AiAgentFaq, AiAgentPurpose, AiCoverageZone, AiInteraction, AiPlan};
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

    async fn count_ai_interactions_for_conversation(
        &self,
        conversation_id: &ObjectId,
    ) -> Result<u64, String> {
        self.ai_interactions()
            .count_documents(doc! { "conversation_id": conversation_id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn count_ai_interactions_for_agent_in_conv(
        &self,
        conversation_id: &ObjectId,
        agent_id: &ObjectId,
    ) -> Result<u64, String> {
        self.ai_interactions()
            .count_documents(doc! {
                "conversation_id": conversation_id,
                "agent_id": agent_id,
            })
            .await
            .map_err(|e| e.to_string())
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

    // ─── Phase 3a ──────────────────────────────────────────────────────────

    async fn find_active_agent_by_workspace_and_purpose(
        &self,
        workspace_id: &ObjectId,
        purpose: AiAgentPurpose,
    ) -> Result<Option<AiAgent>, String> {
        // Serializa el enum como snake_case (e.g. AiAgentPurpose::Soporte → "soporte").
        let purpose_str = serde_json::to_value(purpose)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();

        self.ai_agents()
            .find_one(doc! {
                "workspace_ids": workspace_id,
                "enabled": true,
                "purpose": &purpose_str,
            })
            .sort(doc! { "created_at": 1 })
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_ai_agent_metrics(
        &self,
        agent_id: &ObjectId,
        from: mongodb::bson::DateTime,
        to: mongodb::bson::DateTime,
        granularity: MetricsGranularity,
    ) -> Result<AiAgentMetricsRaw, String> {
        let coll = self.ai_interactions();
        let match_stage = doc! {
            "$match": {
                "agent_id": agent_id,
                "created_at": { "$gte": from, "$lte": to },
            }
        };

        // ── Aggregate A: resumen total (o daily) ─────────────────────────────
        let agg_a_pipeline: Vec<Document> = match granularity {
            MetricsGranularity::Summary => vec![
                match_stage.clone(),
                doc! { "$group": {
                    "_id": null,
                    "total_turns":           { "$sum": 1 },
                    "total_input_tokens":    { "$sum": { "$ifNull": ["$input_tokens",  0] } },
                    "total_output_tokens":   { "$sum": { "$ifNull": ["$output_tokens", 0] } },
                    "total_thinking_tokens": { "$sum": { "$ifNull": ["$thinking_tokens", 0] } },
                    "total_cached_tokens":   { "$sum": { "$ifNull": ["$cached_tokens", 0] } },
                    "total_cost_usd":        { "$sum": { "$ifNull": ["$cost_usd_estimate", 0.0] } },
                    "avg_latency_ms":        { "$avg": { "$ifNull": ["$latency_ms", 0] } },
                    "pre_classified_count":  { "$sum": { "$cond": [{ "$eq": ["$pre_classified", true] }, 1, 0] } },
                    "escalated_count":       { "$sum": { "$cond": [{ "$eq": ["$escalated", true] }, 1, 0] } },
                    "tool_calls_count":      { "$sum": { "$size": { "$ifNull": ["$tool_calls", []] } } },
                } },
            ],
            MetricsGranularity::Daily => vec![
                match_stage.clone(),
                doc! { "$group": {
                    "_id": {
                        "$dateToString": {
                            "format": "%Y-%m-%d",
                            "date": "$created_at",
                            "timezone": "America/Caracas",
                        }
                    },
                    "total_turns":           { "$sum": 1 },
                    "total_input_tokens":    { "$sum": { "$ifNull": ["$input_tokens",  0] } },
                    "total_output_tokens":   { "$sum": { "$ifNull": ["$output_tokens", 0] } },
                    "total_thinking_tokens": { "$sum": { "$ifNull": ["$thinking_tokens", 0] } },
                    "total_cached_tokens":   { "$sum": { "$ifNull": ["$cached_tokens", 0] } },
                    "total_cost_usd":        { "$sum": { "$ifNull": ["$cost_usd_estimate", 0.0] } },
                    "avg_latency_ms":        { "$avg": { "$ifNull": ["$latency_ms", 0] } },
                    "pre_classified_count":  { "$sum": { "$cond": [{ "$eq": ["$pre_classified", true] }, 1, 0] } },
                    "escalated_count":       { "$sum": { "$cond": [{ "$eq": ["$escalated", true] }, 1, 0] } },
                } },
                doc! { "$sort": { "_id": 1 } },
            ],
        };

        // ── Aggregate B: desglose por pre_class_result ────────────────────────
        let agg_b_pipeline = vec![
            doc! { "$match": {
                "agent_id": agent_id,
                "created_at": { "$gte": from, "$lte": to },
                "pre_classified": true,
            } },
            doc! { "$group": {
                "_id": "$pre_class_result",
                "count": { "$sum": 1 },
            } },
        ];

        // Correr los dos aggregates en paralelo.
        let (res_a, res_b) = tokio::join!(
            async {
                coll.aggregate(agg_a_pipeline)
                    .await
                    .map_err(|e| e.to_string())?
                    .try_collect::<Vec<Document>>()
                    .await
                    .map_err(|e| e.to_string())
            },
            async {
                coll.aggregate(agg_b_pipeline)
                    .await
                    .map_err(|e| e.to_string())?
                    .try_collect::<Vec<Document>>()
                    .await
                    .map_err(|e| e.to_string())
            },
        );

        let docs_a = res_a?;
        let docs_b = res_b?;

        // ── Parse Aggregate B ─────────────────────────────────────────────────
        let mut pre_class_breakdown: HashMap<String, u64> = HashMap::new();
        for doc in &docs_b {
            let key = doc
                .get_str("_id")
                .unwrap_or("unknown")
                .to_string();
            let count = doc
                .get_i64("count")
                .or_else(|_| doc.get_i32("count").map(|n| n as i64))
                .unwrap_or(0) as u64;
            pre_class_breakdown.insert(key, count);
        }

        // ── Parse Aggregate A ─────────────────────────────────────────────────
        match granularity {
            MetricsGranularity::Summary => {
                let summary = if let Some(d) = docs_a.first() {
                    parse_summary_doc(d)
                } else {
                    AiAgentMetricsSummary::default()
                };
                Ok(AiAgentMetricsRaw {
                    summary,
                    pre_class_breakdown,
                    daily: None,
                })
            }
            MetricsGranularity::Daily => {
                // Para daily, el "summary" lo computamos sumando los buckets
                // (evita un tercer aggregate).
                let mut buckets: Vec<AiAgentMetricsDailyBucket> = Vec::new();
                let mut summary = AiAgentMetricsSummary::default();
                let mut latency_sum: f64 = 0.0;

                for d in &docs_a {
                    let date = d.get_str("_id").unwrap_or("").to_string();
                    let turns = get_u64(d, "total_turns");
                    let input = get_u64(d, "total_input_tokens");
                    let output = get_u64(d, "total_output_tokens");
                    let thinking = get_u64(d, "total_thinking_tokens");
                    let cached = get_u64(d, "total_cached_tokens");
                    let cost = get_f64(d, "total_cost_usd");
                    let lat = get_f64(d, "avg_latency_ms");
                    let pre_cls = get_u64(d, "pre_classified_count");
                    let escalated = get_u64(d, "escalated_count");

                    summary.total_turns += turns;
                    summary.total_input_tokens += input;
                    summary.total_output_tokens += output;
                    summary.total_thinking_tokens += thinking;
                    summary.total_cached_tokens += cached;
                    summary.total_cost_usd += cost;
                    latency_sum += lat * turns as f64;
                    summary.pre_classified_count += pre_cls;
                    summary.escalated_count += escalated;

                    buckets.push(AiAgentMetricsDailyBucket {
                        date,
                        total_turns: turns,
                        total_input_tokens: input,
                        total_output_tokens: output,
                        total_thinking_tokens: thinking,
                        total_cached_tokens: cached,
                        total_cost_usd: cost,
                        pre_classified_count: pre_cls,
                        escalated_count: escalated,
                    });
                }

                if summary.total_turns > 0 {
                    summary.avg_latency_ms = latency_sum / summary.total_turns as f64;
                }

                Ok(AiAgentMetricsRaw {
                    summary,
                    pre_class_breakdown,
                    daily: Some(buckets),
                })
            }
        }
    }
}

// ── Helpers privados para parsear documentos BSON ─────────────────────────────

fn parse_summary_doc(d: &Document) -> AiAgentMetricsSummary {
    AiAgentMetricsSummary {
        total_turns: get_u64(d, "total_turns"),
        total_input_tokens: get_u64(d, "total_input_tokens"),
        total_output_tokens: get_u64(d, "total_output_tokens"),
        total_thinking_tokens: get_u64(d, "total_thinking_tokens"),
        total_cached_tokens: get_u64(d, "total_cached_tokens"),
        total_cost_usd: get_f64(d, "total_cost_usd"),
        avg_latency_ms: get_f64(d, "avg_latency_ms"),
        pre_classified_count: get_u64(d, "pre_classified_count"),
        escalated_count: get_u64(d, "escalated_count"),
        tool_calls_count: get_u64(d, "tool_calls_count"),
    }
}

fn get_u64(d: &Document, key: &str) -> u64 {
    d.get_i64(key)
        .or_else(|_| d.get_i32(key).map(|n| n as i64))
        .unwrap_or(0)
        .max(0) as u64
}

fn get_f64(d: &Document, key: &str) -> f64 {
    d.get_f64(key)
        .or_else(|_| d.get_i64(key).map(|n| n as f64))
        .or_else(|_| d.get_i32(key).map(|n| n as f64))
        .unwrap_or(0.0)
        .max(0.0)
}
