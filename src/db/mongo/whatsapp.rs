use async_trait::async_trait;
use mongodb::bson::{doc, oid::ObjectId, DateTime, Document};
use mongodb::options::{FindOptions, UpdateOptions};
use mongodb::gridfs::GridFsBucket;
use futures::TryStreamExt;
use futures::{AsyncReadExt, AsyncWriteExt};

use std::collections::HashMap;

use crate::db::{
    AuditFirstResponse, AuditLifecycleByDayBucket, AuditMessageFilter,
    AuditMessagesByAgentBucket, AuditMessagesByDayBucket, AuditMessagesByTypeBucket,
    AuditMessagesSummary, AuditMetricsFilter, ConversationTouch, StoreTemplateMediaInput,
    UpdateQuickReplyPatch, WaTemplateListFilter, WaTemplateMediaRepository,
    WaTemplateMediaRef, WaTemplateRepository, WaTemplateUpdatePatch, WhatsAppRepository,
};
use crate::db::mongo::MongoDB;
use crate::models::whatsapp::{ConversationStats, UrlPreview, WaConversation, WaConversationEvent, WaConversationEventInput, WaConversationOpen, WaMessage, WaPurposesPatch, WaQuickReply, WaSettings, WaTemplate, WaTemplateStatus, WaPurposeUsage};

impl MongoDB {
    pub(crate) fn wa_conversations(&self) -> mongodb::Collection<WaConversation> {
        self.db.collection::<WaConversation>("WaConversations")
    }

    pub(crate) fn wa_messages(&self) -> mongodb::Collection<WaMessage> {
        self.db.collection::<WaMessage>("WaMessages")
    }

    pub(crate) fn wa_settings(&self) -> mongodb::Collection<WaSettings> {
        self.db.collection::<WaSettings>("WaSettings")
    }

    pub(crate) fn wa_conversation_opens(&self) -> mongodb::Collection<WaConversationOpen> {
        self.db.collection::<WaConversationOpen>("WaConversationOpens")
    }

    pub(crate) fn wa_conversation_events(&self) -> mongodb::Collection<WaConversationEvent> {
        self.db.collection::<WaConversationEvent>("WaConversationEvents")
    }

    pub(crate) fn wa_quick_replies(&self) -> mongodb::Collection<WaQuickReply> {
        self.db.collection::<WaQuickReply>("WaQuickReplies")
    }

    pub(crate) fn wa_templates(&self) -> mongodb::Collection<WaTemplate> {
        self.db.collection::<WaTemplate>("WaTemplates")
    }

    /// Bucket GridFS dedicado a media de headers de templates.
    /// Colecciones resultantes: `wa_template_media.files` y `wa_template_media.chunks`.
    pub(crate) fn wa_template_media_bucket(&self) -> GridFsBucket {
        let opts = mongodb::options::GridFsBucketOptions::builder()
            .bucket_name("wa_template_media".to_string())
            .build();
        self.db.gridfs_bucket(opts)
    }
}

#[async_trait]
impl WhatsAppRepository for MongoDB {
    async fn find_conversation_by_phones(
        &self,
        contact_phone: &str,
        business_phone: &str,
    ) -> Result<Option<WaConversation>, String> {
        self.wa_conversations()
            .find_one(doc! { "phone": contact_phone, "business_phone": business_phone })
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_conversation_by_id(&self, id: &ObjectId) -> Result<Option<WaConversation>, String> {
        self.wa_conversations()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn upsert_conversation(
        &self,
        contact_phone: &str,
        business_phone: &str,
        name: Option<String>,
    ) -> Result<(WaConversation, bool), String> {
        let now = DateTime::now();
        let col = self.wa_conversations();

        let mut set_on_insert = doc! {
            "phone": contact_phone,
            "business_phone": business_phone,
            "status": "pending",
            "unread_count": 0,
            "created_at": now,
            "last_message_at": now,
        };

        let mut update = doc! {};

        if let Some(n) = name.as_ref() {
            update.insert("$set", doc! { "name": n });
        } else {
            set_on_insert.insert("name", mongodb::bson::Bson::Null);
        }

        update.insert("$setOnInsert", set_on_insert);

        let opts = UpdateOptions::builder().upsert(true).build();
        let res = col
            .update_one(
                doc! { "phone": contact_phone, "business_phone": business_phone },
                update,
            )
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?;

        let created = res.upserted_id.is_some();

        let conv = self
            .find_conversation_by_phones(contact_phone, business_phone)
            .await?
            .ok_or_else(|| "conversation not found after upsert".to_string())?;

        Ok((conv, created))
    }

    async fn touch_conversation(
        &self,
        id: &ObjectId,
        touch: ConversationTouch<'_>,
    ) -> Result<(), String> {
        let ts = touch.last_message_at.unwrap_or_else(DateTime::now);
        let unread_update: i32 = if touch.increment_unread { 1 } else { 0 };

        let mut set_doc = doc! {
            "last_message_at": ts,
            "last_message_preview": touch.preview,
            "last_message_type": touch.msg_type,
            "last_message_direction": touch.direction,
            "last_message_wa_id": touch.wa_message_id,
        };
        let mut unset_doc = Document::new();

        match touch.status {
            Some(s) => { set_doc.insert("last_message_status", s); }
            None    => { unset_doc.insert("last_message_status", ""); }
        }
        match touch.from_user_id {
            Some(u) => { set_doc.insert("last_message_from_user_id", u); }
            None    => { unset_doc.insert("last_message_from_user_id", ""); }
        }
        match touch.media_filename {
            Some(f) => { set_doc.insert("last_message_media_filename", f); }
            None    => { unset_doc.insert("last_message_media_filename", ""); }
        }

        let mut update_doc = doc! {
            "$set": set_doc,
            "$inc": { "unread_count": unread_update },
        };
        if !unset_doc.is_empty() {
            update_doc.insert("$unset", unset_doc);
        }

        self.wa_conversations()
            .update_one(doc! { "_id": id }, update_doc)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }

    async fn update_conversation_status_if_last(
        &self,
        id: &ObjectId,
        wa_message_id: &str,
        status: &str,
    ) -> Result<bool, String> {
        let res = self.wa_conversations()
            .update_one(
                doc! { "_id": id, "last_message_wa_id": wa_message_id },
                doc! { "$set": { "last_message_status": status } },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.modified_count > 0)
    }

    async fn update_last_inbound_at(
        &self,
        id: &ObjectId,
        when: DateTime,
    ) -> Result<(), String> {
        // El inbound libera cualquier engagement throttle (131049) activo:
        // el cliente acaba de responder, así que Meta deja de rate-limitar.
        self.wa_conversations()
            .update_one(
                doc! { "_id": id },
                doc! {
                    "$set": { "last_inbound_at": when },
                    "$unset": { "meta_throttle_until": "" },
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn set_meta_throttle_until(
        &self,
        id: &ObjectId,
        until: DateTime,
    ) -> Result<(), String> {
        self.wa_conversations()
            .update_one(
                doc! { "_id": id },
                doc! { "$set": { "meta_throttle_until": until } },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn update_conversation_client_id(
        &self,
        id: &ObjectId,
        client_id: &ObjectId,
    ) -> Result<(), String> {
        self.wa_conversations()
            .update_one(
                doc! { "_id": id },
                doc! { "$set": { "client_id": client_id } },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn backfill_last_inbound_at(&self) -> Result<u64, String> {
        let messages = self.wa_messages();
        let conversations = self.wa_conversations();

        // Max timestamp de inbound por conversación.
        let pipeline = vec![
            doc! { "$match": { "direction": "in" } },
            doc! { "$group": { "_id": "$conversation_id", "maxTs": { "$max": "$timestamp" } } },
        ];

        let mut cursor = messages.aggregate(pipeline).await.map_err(|e| e.to_string())?;

        let mut updated: u64 = 0;
        while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
            let conv_id = match doc.get_object_id("_id") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let ts = match doc.get_datetime("maxTs") {
                Ok(v) => *v,
                Err(_) => continue,
            };

            // Sólo setea si falta o está en null — no pisa valores ya seteados
            // por el webhook (que son la verdad más fresca).
            let res = conversations
                .update_one(
                    doc! {
                        "_id": conv_id,
                        "$or": [
                            { "last_inbound_at": { "$exists": false } },
                            { "last_inbound_at": null },
                        ],
                    },
                    doc! { "$set": { "last_inbound_at": ts } },
                )
                .await
                .map_err(|e| e.to_string())?;

            updated += res.modified_count;
        }

        Ok(updated)
    }

    async fn save_message(&self, message: WaMessage) -> Result<WaMessage, String> {
        let col = self.wa_messages();

        let insert_doc = mongodb::bson::to_document(&message).map_err(|e| e.to_string())?;
        let opts = UpdateOptions::builder().upsert(true).build();
        col.update_one(
            doc! { "wa_message_id": &message.wa_message_id },
            doc! { "$setOnInsert": insert_doc },
        )
        .with_options(opts)
        .await
        .map_err(|e| e.to_string())?;

        col.find_one(doc! { "wa_message_id": &message.wa_message_id })
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "message not found after upsert".to_string())
    }

    async fn get_conversations(
        &self,
        status: Option<&str>,
        assigned_to: Option<&str>,
        business_phone: Option<&str>,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<Vec<WaConversation>, String> {
        let mut filter = Document::new();
        if let Some(s) = status {
            filter.insert("status", s);
        }
        if let Some(a) = assigned_to {
            filter.insert("assigned_to", a);
        }
        if let Some(bp) = business_phone {
            filter.insert("business_phone", bp);
        }

        if let Some(c) = cursor {
            if let Some((ts, oid)) = decode_cursor(c) {
                filter.insert(
                    "$or",
                    vec![
                        doc! { "last_message_at": { "$lt": ts } },
                        doc! { "last_message_at": ts, "_id": { "$lt": oid } },
                    ],
                );
            }
        }

        let opts = FindOptions::builder()
            .sort(doc! { "last_message_at": -1, "_id": -1 })
            .limit(limit)
            .build();

        self.wa_conversations()
            .find(filter)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_conversation_stats(
        &self,
        business_phone: Option<&str>,
        current_user_id: &str,
    ) -> Result<ConversationStats, String> {
        let mut match_stage = Document::new();
        if let Some(bp) = business_phone {
            match_stage.insert("business_phone", bp);
        }

        let pipeline = vec![
            doc! { "$match": match_stage },
            doc! { "$facet": {
                "total":       [ { "$count": "n" } ],
                "mine":        [ { "$match": { "assigned_to": current_user_id } }, { "$count": "n" } ],
                "pending":     [ { "$match": { "status": "pending" } },            { "$count": "n" } ],
                "in_progress": [ { "$match": { "status": "in_progress" } },        { "$count": "n" } ],
                "closed":      [ { "$match": { "status": "closed" } },             { "$count": "n" } ],
            } },
        ];

        let mut cursor = self
            .wa_conversations()
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let doc = cursor
            .try_next()
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_default();

        let extract = |key: &str| -> u64 {
            doc.get_array(key)
                .ok()
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_document())
                .and_then(|d| d.get("n"))
                .and_then(|v| v.as_i32().map(|n| n as u64).or_else(|| v.as_i64().map(|n| n as u64)))
                .unwrap_or(0)
        };

        Ok(ConversationStats {
            total: extract("total"),
            mine: extract("mine"),
            pending: extract("pending"),
            in_progress: extract("in_progress"),
            closed: extract("closed"),
        })
    }

    async fn get_messages(
        &self,
        conversation_id: &ObjectId,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<Vec<WaMessage>, String> {
        let mut filter = doc! { "conversation_id": conversation_id };

        if let Some(c) = cursor {
            if let Some((ts, oid)) = decode_cursor(c) {
                filter.insert(
                    "$or",
                    vec![
                        doc! { "timestamp": { "$lt": ts } },
                        doc! { "timestamp": ts, "_id": { "$lt": oid } },
                    ],
                );
            }
        }

        let opts = FindOptions::builder()
            .sort(doc! { "timestamp": -1, "_id": -1 })
            .limit(limit)
            .build();

        self.wa_messages()
            .find(filter)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_conversation_status(&self, id: &ObjectId, status: &str) -> Result<(), String> {
        self.wa_conversations()
            .update_one(doc! { "_id": id }, doc! { "$set": { "status": status } })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn close_conversation(&self, id: &ObjectId) -> Result<(), String> {
        self.wa_conversations()
            .update_one(
                doc! { "_id": id },
                doc! {
                    "$set": { "status": "closed" },
                    "$unset": { "assigned_to": "" },
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn reopen_conversation(&self, id: &ObjectId) -> Result<bool, String> {
        let res = self.wa_conversations()
            .update_one(
                doc! { "_id": id, "status": "closed" },
                doc! {
                    "$set": { "status": "pending" },
                    "$unset": { "assigned_to": "" },
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.modified_count > 0)
    }

    async fn assign_conversation(
        &self,
        id: &ObjectId,
        assigned_to: Option<&str>,
    ) -> Result<(), String> {
        // Tanto transfer (Some) como release (None) dejan la conversación en
        // `pending`. El status sólo pasa a `in_progress` cuando el agente
        // asignado abre el chat por primera vez (primer GET /messages).
        let update = match assigned_to {
            Some(uid) => doc! { "$set": { "assigned_to": uid, "status": "pending" } },
            None => doc! { "$unset": { "assigned_to": "" }, "$set": { "status": "pending" } },
        };
        self.wa_conversations()
            .update_one(doc! { "_id": id }, update)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn take_conversation(
        &self,
        id: &ObjectId,
        agent_id: &str,
    ) -> Result<Option<WaConversation>, String> {
        use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};
        use mongodb::options::UpdateModifications;
        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();

        // Atómico: acepta `pending` (toma/reasignación) y `closed` (reopen+take).
        // - Si era `pending`:  asigna el agente, deja `status` intacto.
        // - Si era `closed`:   asigna el agente y fuerza `status = "in_progress"`.
        // Idempotente: tomar mi propia conv `pending` devuelve el doc sin cambios.
        let filter = doc! { "_id": id, "status": { "$in": ["pending", "closed"] } };
        let pipeline = vec![
            doc! {
                "$set": {
                    "assigned_to": agent_id,
                    "status": {
                        "$cond": [
                            { "$eq": ["$status", "closed"] },
                            "in_progress",
                            "$status"
                        ]
                    }
                }
            }
        ];
        let res = self.wa_conversations()
            .find_one_and_update(filter, UpdateModifications::Pipeline(pipeline))
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?;

        Ok(res)
    }

    async fn reset_unread(&self, id: &ObjectId) -> Result<(), String> {
        self.wa_conversations()
            .update_one(doc! { "_id": id }, doc! { "$set": { "unread_count": 0 } })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn mark_inbound_as_read(&self, conversation_id: &ObjectId) -> Result<Vec<String>, String> {
        let col = self.wa_messages();

        // Buscar inbound no leídos antes del update para poder devolverlos.
        let filter = doc! {
            "conversation_id": conversation_id,
            "direction": "in",
            "$or": [
                { "status": { "$exists": false } },
                { "status": { "$ne": "read" } },
            ],
        };

        // Struct de proyección dedicado: la proyección excluye campos requeridos
        // de `WaMessage` (conversation_id, direction, msg_type, timestamp), así
        // que deserializar la Collection tipada fallaría.
        #[derive(serde::Deserialize)]
        struct MsgIdProj {
            wa_message_id: String,
        }

        let projection = FindOptions::builder()
            .projection(doc! { "wa_message_id": 1, "_id": 0 })
            .build();

        let mut ids = Vec::new();
        let mut cursor = self.db
            .collection::<MsgIdProj>("WaMessages")
            .find(filter.clone())
            .with_options(projection)
            .await
            .map_err(|e| e.to_string())?;

        while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
            ids.push(doc.wa_message_id);
        }

        if ids.is_empty() {
            return Ok(Vec::new());
        }

        col.update_many(filter, doc! { "$set": { "status": "read" } })
            .await
            .map_err(|e| e.to_string())?;

        Ok(ids)
    }

    async fn find_message_by_idempotency(
        &self,
        conversation_id: &ObjectId,
        idempotency_key: &str,
    ) -> Result<Option<WaMessage>, String> {
        self.wa_messages()
            .find_one(doc! {
                "conversation_id": conversation_id,
                "idempotency_key": idempotency_key,
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_message_retry(
        &self,
        id: &ObjectId,
        new_wa_message_id: &str,
        status: &str,
    ) -> Result<Option<WaMessage>, String> {
        use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};
        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.wa_messages()
            .find_one_and_update(
                doc! { "_id": id },
                doc! {
                    "$set": {
                        "wa_message_id": new_wa_message_id,
                        "status": status,
                        "timestamp": DateTime::now(),
                    },
                },
            )
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn set_message_url_preview(
        &self,
        id: &ObjectId,
        preview: &UrlPreview,
    ) -> Result<Option<WaMessage>, String> {
        use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};
        let bson_preview = mongodb::bson::to_bson(preview).map_err(|e| e.to_string())?;
        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.wa_messages()
            .find_one_and_update(
                doc! { "_id": id },
                doc! { "$set": { "url_preview": bson_preview } },
            )
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_messages_by_wa_ids(
        &self,
        wa_ids: &[String],
    ) -> Result<HashMap<String, WaMessage>, String> {
        if wa_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let cursor = self.wa_messages()
            .find(doc! { "wa_message_id": { "$in": wa_ids } })
            .await
            .map_err(|e| e.to_string())?;
        let msgs: Vec<WaMessage> = cursor.try_collect().await.map_err(|e| e.to_string())?;
        Ok(msgs.into_iter().map(|m| (m.wa_message_id.clone(), m)).collect())
    }

    async fn find_message_by_media_id(
        &self,
        media_id: &str,
    ) -> Result<Option<WaMessage>, String> {
        self.wa_messages()
            .find_one(doc! { "media_id": media_id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_message_status(&self, wa_message_id: &str, status: &str) -> Result<Option<WaMessage>, String> {
        use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};
        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.wa_messages()
            .find_one_and_update(
                doc! { "wa_message_id": wa_message_id },
                doc! { "$set": { "status": status } },
            )
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_wa_settings_by_phone(&self, phone: &str) -> Result<Option<WaSettings>, String> {
        self.wa_settings()
            .find_one(doc! { "phone": phone, "active": true })
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_wa_settings_by_id(&self, id: &ObjectId) -> Result<Option<WaSettings>, String> {
        self.wa_settings()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_workspace_names(&self, phones: &[String]) -> Result<HashMap<String, String>, String> {
        if phones.is_empty() {
            return Ok(HashMap::new());
        }
        // Struct de proyección dedicado: `WaSettings` completo tiene campos sin `#[serde(default)]`
        // (agents, active, timestamps) que la proyección no devuelve.
        #[derive(serde::Deserialize)]
        struct WorkspaceProj {
            phone: String,
            #[serde(default)]
            workspace_name: String,
        }

        let mut cursor = self.db
            .collection::<WorkspaceProj>("WaSettings")
            .find(doc! { "phone": { "$in": phones } })
            .with_options(
                FindOptions::builder()
                    .projection(doc! { "phone": 1, "workspace_name": 1, "_id": 0 })
                    .build(),
            )
            .await
            .map_err(|e| e.to_string())?;

        let mut out = HashMap::with_capacity(phones.len());
        while let Some(s) = cursor.try_next().await.map_err(|e| e.to_string())? {
            if !s.workspace_name.is_empty() {
                out.insert(s.phone, s.workspace_name);
            }
        }
        Ok(out)
    }

    async fn get_all_wa_settings(&self) -> Result<Vec<WaSettings>, String> {
        self.wa_settings()
            .find(doc! {})
            .sort(doc! { "created_at": -1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_wa_settings(&self, settings: WaSettings) -> Result<WaSettings, String> {
        let result = self.wa_settings()
            .insert_one(&settings)
            .await
            .map_err(|e| e.to_string())?;
        let id = result.inserted_id.as_object_id().unwrap();
        self.wa_settings()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "settings not found after insert".to_string())
    }

    async fn update_wa_settings(
        &self,
        id: &ObjectId,
        workspace_name: Option<String>,
        phone_number_id: Option<String>,
        whatsapp_business_account_id: Option<String>,
        access_token_cipher: Option<String>,
        agents: Option<Vec<String>>,
        active: Option<bool>,
        purposes: Option<WaPurposesPatch>,
    ) -> Result<(), String> {
        let mut set_doc = doc! { "updated_at": DateTime::now() };
        let mut unset_doc = Document::new();
        if let Some(w) = workspace_name {
            set_doc.insert("workspace_name", w);
        }
        if let Some(p) = phone_number_id {
            set_doc.insert("phone_number_id", p);
        }
        if let Some(wa) = whatsapp_business_account_id {
            set_doc.insert("whatsapp_business_account_id", wa);
        }
        // Sólo tocar el token si viene no-vacío — `Some("")` no debe borrarlo.
        if let Some(t) = access_token_cipher {
            if !t.is_empty() {
                set_doc.insert("access_token", t);
            }
        }
        if let Some(a) = agents {
            set_doc.insert("agents", mongodb::bson::to_bson(&a).unwrap());
        }
        if let Some(act) = active {
            set_doc.insert("active", act);
        }

        // purposes: tri-state per key. `None` = no tocar ese propósito;
        // `Some(None)` = limpiar (unset); `Some(Some(cfg))` = setear.
        if let Some(p) = purposes {
            // OTP
            match p.otp {
                None => {}
                Some(None) => { unset_doc.insert("purposes.otp", ""); }
                Some(Some(cfg)) => {
                    set_doc.insert(
                        "purposes.otp",
                        mongodb::bson::to_bson(&cfg).map_err(|e| e.to_string())?,
                    );
                }
            }
            // Notifications
            match p.notifications {
                None => {}
                Some(None) => { unset_doc.insert("purposes.notifications", ""); }
                Some(Some(cfg)) => {
                    set_doc.insert(
                        "purposes.notifications",
                        mongodb::bson::to_bson(&cfg).map_err(|e| e.to_string())?,
                    );
                }
            }
            // Payment reminder
            match p.payment_reminder {
                None => {}
                Some(None) => { unset_doc.insert("purposes.payment_reminder", ""); }
                Some(Some(cfg)) => {
                    set_doc.insert(
                        "purposes.payment_reminder",
                        mongodb::bson::to_bson(&cfg).map_err(|e| e.to_string())?,
                    );
                }
            }
        }

        let mut update_doc = doc! { "$set": set_doc };
        if !unset_doc.is_empty() {
            update_doc.insert("$unset", unset_doc);
        }
        self.wa_settings()
            .update_one(doc! { "_id": id }, update_doc)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn find_wa_settings_for_purpose(
        &self,
        purpose: &str,
    ) -> Result<Vec<WaSettings>, String> {
        let field = match purpose {
            "otp" => "purposes.otp",
            "notifications" => "purposes.notifications",
            "payment_reminder" => "purposes.payment_reminder",
            _ => return Err(format!("unknown purpose: {}", purpose)),
        };
        let filter = doc! {
            "active": true,
            field: { "$exists": true, "$ne": null },
        };
        self.wa_settings()
            .find(filter)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_wa_settings_by_phone_number_id(&self, phone_number_id: &str) -> Result<Option<WaSettings>, String> {
        self.wa_settings()
            .find_one(doc! { "phone_number_id": phone_number_id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_wa_settings_missing_waba(&self) -> Result<Vec<WaSettings>, String> {
        // "Vacío" = ausente o string "". Necesitamos $or para cubrir ambos.
        let filter = doc! {
            "$or": [
                { "whatsapp_business_account_id": { "$exists": false } },
                { "whatsapp_business_account_id": "" },
            ]
        };
        self.wa_settings()
            .find(filter)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn set_wa_settings_waba_id(&self, id: &ObjectId, waba_id: &str) -> Result<(), String> {
        self.wa_settings()
            .update_one(
                doc! { "_id": id },
                doc! {
                    "$set": {
                        "whatsapp_business_account_id": waba_id,
                        "updated_at": DateTime::now(),
                    }
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn delete_wa_settings(&self, id: &ObjectId) -> Result<(), String> {
        self.wa_settings()
            .delete_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn record_conversation_open(
        &self,
        user_id: &str,
        conversation_id: &ObjectId,
    ) -> Result<(), String> {
        let now = DateTime::now();
        let opts = UpdateOptions::builder().upsert(true).build();
        self.wa_conversation_opens()
            .update_one(
                doc! { "user_id": user_id, "conversation_id": conversation_id },
                doc! {
                    "$set": { "last_opened_at": now },
                    "$setOnInsert": {
                        "user_id": user_id,
                        "conversation_id": conversation_id,
                    },
                },
            )
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn get_conversation_opens(
        &self,
        user_id: &str,
        conversation_ids: &[ObjectId],
    ) -> Result<HashMap<ObjectId, DateTime>, String> {
        if conversation_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let filter = doc! {
            "user_id": user_id,
            "conversation_id": { "$in": conversation_ids },
        };
        let mut out = HashMap::new();
        let mut cursor = self
            .wa_conversation_opens()
            .find(filter)
            .await
            .map_err(|e| e.to_string())?;
        while let Some(open) = cursor.try_next().await.map_err(|e| e.to_string())? {
            out.insert(open.conversation_id, open.last_opened_at);
        }
        Ok(out)
    }

    async fn get_user_workspaces(&self, user_id: &str) -> Result<Vec<ObjectId>, String> {
        #[derive(serde::Deserialize)]
        struct IdOnly {
            #[serde(rename = "_id")]
            id: ObjectId,
        }
        let mut cursor = self.db
            .collection::<IdOnly>("WaSettings")
            .find(doc! { "agents": user_id })
            .with_options(
                FindOptions::builder()
                    .projection(doc! { "_id": 1 })
                    .build(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(s) = cursor.try_next().await.map_err(|e| e.to_string())? {
            out.push(s.id);
        }
        Ok(out)
    }

    async fn wa_settings_exist(&self, ids: &[ObjectId]) -> Result<bool, String> {
        if ids.is_empty() {
            return Ok(false);
        }
        let count = self.wa_settings()
            .count_documents(doc! { "_id": { "$in": ids } })
            .await
            .map_err(|e| e.to_string())?;
        Ok(count as usize == ids.len())
    }

    async fn list_quick_replies(
        &self,
        filter_workspace_id: Option<&ObjectId>,
        active_filter: Option<bool>,
    ) -> Result<Vec<WaQuickReply>, String> {
        // Autorización del caller se resuelve en el handler (bCanChat). Acá
        // sólo aplicamos el filtro opcional de workspace.
        let mut filter = match filter_workspace_id {
            Some(id) => doc! { "workspace_ids": id },
            None => doc! {},
        };
        if let Some(a) = active_filter {
            filter.insert("active", a);
        }
        self.wa_quick_replies()
            .find(filter)
            .sort(doc! { "updated_at": -1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_quick_reply_by_id(&self, id: &ObjectId) -> Result<Option<WaQuickReply>, String> {
        self.wa_quick_replies()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_quick_reply(&self, doc: WaQuickReply) -> Result<WaQuickReply, String> {
        let result = self.wa_quick_replies()
            .insert_one(&doc)
            .await
            .map_err(|e| e.to_string())?;
        let id = result.inserted_id.as_object_id().ok_or_else(|| "insert sin ObjectId".to_string())?;
        self.wa_quick_replies()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "quick_reply no encontrado tras insert".to_string())
    }

    async fn update_quick_reply(
        &self,
        id: &ObjectId,
        patch: UpdateQuickReplyPatch,
    ) -> Result<Option<WaQuickReply>, String> {
        use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};

        let mut set_doc = doc! { "updated_at": DateTime::now() };
        let mut unset_doc = Document::new();

        if let Some(t) = patch.title {
            set_doc.insert("title", t);
        }
        if let Some(c) = patch.content {
            set_doc.insert("content", c);
        }
        if let Some(ws) = patch.workspace_ids {
            set_doc.insert("workspace_ids", mongodb::bson::to_bson(&ws).map_err(|e| e.to_string())?);
        }
        if let Some(a) = patch.active {
            set_doc.insert("active", a);
        }

        // Campos nullable: Some(Some(v)) → $set, Some(None) → $unset, None → ignorar.
        match patch.header {
            Some(Some(h)) => { set_doc.insert("header", mongodb::bson::to_bson(&h).map_err(|e| e.to_string())?); }
            Some(None) => { unset_doc.insert("header", ""); }
            None => {}
        }
        match patch.footer {
            Some(Some(f)) => { set_doc.insert("footer", f); }
            Some(None) => { unset_doc.insert("footer", ""); }
            None => {}
        }
        match patch.buttons {
            Some(Some(b)) => { set_doc.insert("buttons", mongodb::bson::to_bson(&b).map_err(|e| e.to_string())?); }
            Some(None) => { unset_doc.insert("buttons", ""); }
            None => {}
        }
        match patch.list {
            Some(Some(l)) => { set_doc.insert("list", mongodb::bson::to_bson(&l).map_err(|e| e.to_string())?); }
            Some(None) => { unset_doc.insert("list", ""); }
            None => {}
        }
        match patch.cta_url {
            Some(Some(c)) => { set_doc.insert("cta_url", mongodb::bson::to_bson(&c).map_err(|e| e.to_string())?); }
            Some(None) => { unset_doc.insert("cta_url", ""); }
            None => {}
        }

        let mut update = doc! { "$set": set_doc };
        if !unset_doc.is_empty() {
            update.insert("$unset", unset_doc);
        }

        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.wa_quick_replies()
            .find_one_and_update(doc! { "_id": id }, update)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn set_quick_reply_active(
        &self,
        id: &ObjectId,
        active: bool,
    ) -> Result<Option<WaQuickReply>, String> {
        use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};
        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();
        self.wa_quick_replies()
            .find_one_and_update(
                doc! { "_id": id },
                doc! { "$set": { "active": active, "updated_at": DateTime::now() } },
            )
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn increment_quick_reply_use(&self, id: &ObjectId) -> Result<(), String> {
        self.wa_quick_replies()
            .update_one(
                doc! { "_id": id },
                doc! { "$inc": { "use_count": 1 }, "$set": { "last_used_at": DateTime::now() } },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn delete_quick_reply(&self, id: &ObjectId) -> Result<bool, String> {
        let res = self.wa_quick_replies()
            .delete_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.deleted_count > 0)
    }

    async fn record_conversation_event(
        &self,
        input: WaConversationEventInput<'_>,
    ) -> Result<(), String> {
        let doc = WaConversationEvent {
            id: None,
            conversation_id: *input.conversation_id,
            business_phone: input.business_phone.to_string(),
            event_type: input.event_type.to_string(),
            actor_id: input.actor_id.map(str::to_string),
            actor_name: input.actor_name.map(str::to_string),
            target_id: input.target_id.map(str::to_string),
            target_name: input.target_name.map(str::to_string),
            note: input.note.map(str::to_string),
            created_at: DateTime::now(),
        };
        self.wa_conversation_events()
            .insert_one(doc)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn list_conversation_events(
        &self,
        conversation_id: &ObjectId,
    ) -> Result<Vec<WaConversationEvent>, String> {
        let opts = FindOptions::builder()
            .sort(doc! { "created_at": 1, "_id": 1 })
            .build();
        self.wa_conversation_events()
            .find(doc! { "conversation_id": conversation_id })
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn audit_list_messages(
        &self,
        filter: AuditMessageFilter<'_>,
    ) -> Result<Vec<WaMessage>, String> {
        let mut q = Document::new();

        // Rango de fechas sobre `timestamp`.
        let mut ts_range = Document::new();
        if let Some(from) = filter.from_date {
            ts_range.insert("$gte", from);
        }
        if let Some(to) = filter.to_date {
            ts_range.insert("$lte", to);
        }
        if !ts_range.is_empty() {
            q.insert("timestamp", ts_range);
        }

        if let Some(agent) = filter.agent_id {
            q.insert("sent_by", agent);
        }

        if let Some(ids) = filter.conversation_ids {
            // `Some([])` deliberado: sin matches.
            q.insert("conversation_id", doc! { "$in": ids });
        }

        if let Some(d) = filter.direction {
            if d == "in" || d == "out" {
                q.insert("direction", d);
            }
        }

        if let Some(t) = filter.msg_type {
            q.insert("msg_type", t);
        }

        if let Some(s) = filter.search.filter(|s| !s.is_empty()) {
            // Regex case-insensitive sobre `body` — escape básico de los
            // metacaracteres más comunes para evitar regex injection.
            let escaped = regex_escape(s);
            q.insert(
                "body",
                doc! { "$regex": escaped, "$options": "i" },
            );
        }

        // Cursor descendente por (timestamp, _id).
        if let Some(c) = filter.cursor {
            if let Some((ts, oid)) = decode_cursor(c) {
                q.insert(
                    "$or",
                    vec![
                        doc! { "timestamp": { "$lt": ts } },
                        doc! { "timestamp": ts, "_id": { "$lt": oid } },
                    ],
                );
            }
        }

        let opts = FindOptions::builder()
            .sort(doc! { "timestamp": -1, "_id": -1 })
            .limit(filter.limit)
            .build();

        self.wa_messages()
            .find(q)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn audit_count_messages(
        &self,
        filter: &AuditMessageFilter<'_>,
    ) -> Result<u64, String> {
        // Reusamos la misma lógica de armado de filtro que `audit_list_messages`,
        // pero ignoramos cursor/limit (no aplican para count).
        let mut q = Document::new();

        let mut ts_range = Document::new();
        if let Some(from) = filter.from_date {
            ts_range.insert("$gte", from);
        }
        if let Some(to) = filter.to_date {
            ts_range.insert("$lte", to);
        }
        if !ts_range.is_empty() {
            q.insert("timestamp", ts_range);
        }
        if let Some(agent) = filter.agent_id {
            q.insert("sent_by", agent);
        }
        if let Some(ids) = filter.conversation_ids {
            q.insert("conversation_id", doc! { "$in": ids });
        }
        if let Some(d) = filter.direction {
            if d == "in" || d == "out" {
                q.insert("direction", d);
            }
        }
        if let Some(t) = filter.msg_type {
            q.insert("msg_type", t);
        }
        if let Some(s) = filter.search.filter(|s| !s.is_empty()) {
            let escaped = regex_escape(s);
            q.insert("body", doc! { "$regex": escaped, "$options": "i" });
        }

        self.wa_messages()
            .count_documents(q)
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_conversation_ids_by_phones(
        &self,
        customer_phone: Option<&str>,
        business_phone: Option<&str>,
    ) -> Result<Vec<ObjectId>, String> {
        if customer_phone.is_none() && business_phone.is_none() {
            return Err("at least one phone filter required".into());
        }

        let mut q = Document::new();
        if let Some(p) = customer_phone {
            q.insert("phone", p);
        }
        if let Some(b) = business_phone {
            q.insert("business_phone", b);
        }

        #[derive(serde::Deserialize)]
        struct ConvIdProj {
            #[serde(rename = "_id")]
            id: ObjectId,
        }

        let projection = FindOptions::builder()
            .projection(doc! { "_id": 1 })
            .build();

        let mut ids = Vec::new();
        let mut cursor = self
            .db
            .collection::<ConvIdProj>("WaConversations")
            .find(q)
            .with_options(projection)
            .await
            .map_err(|e| e.to_string())?;
        while let Some(d) = cursor.try_next().await.map_err(|e| e.to_string())? {
            ids.push(d.id);
        }
        Ok(ids)
    }

    async fn find_conversations_by_ids(
        &self,
        ids: &[ObjectId],
    ) -> Result<HashMap<ObjectId, WaConversation>, String> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut cursor = self
            .wa_conversations()
            .find(doc! { "_id": { "$in": ids } })
            .await
            .map_err(|e| e.to_string())?;
        let mut out = HashMap::new();
        while let Some(c) = cursor.try_next().await.map_err(|e| e.to_string())? {
            if let Some(id) = c.id {
                out.insert(id, c);
            }
        }
        Ok(out)
    }

    async fn count_messages_for_conversation(
        &self,
        conversation_id: &ObjectId,
    ) -> Result<u64, String> {
        self.wa_messages()
            .count_documents(doc! { "conversation_id": conversation_id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn audit_messages_summary(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<AuditMessagesSummary, String> {
        let match_stage = build_messages_match(filter);
        // $facet single-pipeline: 4 cifras en una sola query.
        let pipeline = vec![
            doc! { "$match": match_stage },
            doc! {
                "$facet": {
                    "by_dir": [
                        { "$group": { "_id": "$direction", "n": { "$sum": 1_i64 } } }
                    ],
                    "convs": [
                        { "$group": { "_id": "$conversation_id" } },
                        { "$count": "n" }
                    ],
                }
            },
        ];

        let mut cursor = self.wa_messages()
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let doc = cursor.try_next().await.map_err(|e| e.to_string())?;
        let mut summary = AuditMessagesSummary { total: 0, inbound: 0, outbound: 0, distinct_conversations: 0 };
        if let Some(d) = doc {
            if let Ok(by_dir) = d.get_array("by_dir") {
                for item in by_dir {
                    if let Some(obj) = item.as_document() {
                        let dir = obj.get_str("_id").unwrap_or("");
                        let n = obj.get_i64("n").unwrap_or(0).max(0) as u64;
                        match dir {
                            "in" => summary.inbound = n,
                            "out" => summary.outbound = n,
                            _ => {}
                        }
                        summary.total += n;
                    }
                }
            }
            if let Ok(convs) = d.get_array("convs") {
                if let Some(first) = convs.first().and_then(|b| b.as_document()) {
                    summary.distinct_conversations = first.get_i64("n").unwrap_or(0).max(0) as u64;
                }
            }
        }
        Ok(summary)
    }

    async fn audit_messages_by_day(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<Vec<AuditMessagesByDayBucket>, String> {
        let match_stage = build_messages_match(filter);
        let date_format = granularity_to_date_format(filter.granularity);
        let pipeline = vec![
            doc! { "$match": match_stage },
            doc! {
                "$group": {
                    "_id": {
                        "date": { "$dateToString": { "format": date_format, "date": "$timestamp" } },
                        "direction": "$direction",
                    },
                    "n": { "$sum": 1_i64 }
                }
            },
            doc! { "$sort": { "_id.date": 1 } },
        ];

        let mut cursor = self.wa_messages()
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let mut buckets: HashMap<String, AuditMessagesByDayBucket> = HashMap::new();
        while let Some(d) = cursor.try_next().await.map_err(|e| e.to_string())? {
            let id = d.get_document("_id").map_err(|e| e.to_string())?;
            let date = id.get_str("date").unwrap_or_default().to_string();
            let dir = id.get_str("direction").unwrap_or_default().to_string();
            let n = d.get_i64("n").unwrap_or(0).max(0) as u64;
            let entry = buckets.entry(date.clone()).or_insert_with(|| AuditMessagesByDayBucket {
                date,
                inbound: 0,
                outbound: 0,
            });
            match dir.as_str() {
                "in" => entry.inbound += n,
                "out" => entry.outbound += n,
                _ => {}
            }
        }
        let mut out: Vec<_> = buckets.into_values().collect();
        out.sort_by(|a, b| a.date.cmp(&b.date));
        Ok(out)
    }

    async fn audit_messages_by_agent(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<Vec<AuditMessagesByAgentBucket>, String> {
        let mut match_stage = build_messages_match(filter);
        match_stage.insert("direction", "out");
        match_stage.insert("sent_by", doc! { "$ne": null, "$exists": true });

        let pipeline = vec![
            doc! { "$match": match_stage },
            doc! {
                "$group": {
                    "_id": "$sent_by",
                    "messages_sent": { "$sum": 1_i64 },
                    "convs": { "$addToSet": "$conversation_id" },
                }
            },
            doc! {
                "$project": {
                    "_id": 1,
                    "messages_sent": 1,
                    "conversations_handled": { "$size": "$convs" }
                }
            },
            doc! { "$sort": { "messages_sent": -1 } },
        ];

        let mut cursor = self.wa_messages()
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        while let Some(d) = cursor.try_next().await.map_err(|e| e.to_string())? {
            out.push(AuditMessagesByAgentBucket {
                agent_id: d.get_str("_id").unwrap_or_default().to_string(),
                messages_sent: d.get_i64("messages_sent").unwrap_or(0).max(0) as u64,
                conversations_handled: d.get_i32("conversations_handled").unwrap_or(0).max(0) as u64,
            });
        }
        Ok(out)
    }

    async fn audit_messages_by_type(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<Vec<AuditMessagesByTypeBucket>, String> {
        let match_stage = build_messages_match(filter);
        let pipeline = vec![
            doc! { "$match": match_stage },
            doc! { "$group": { "_id": "$msg_type", "n": { "$sum": 1_i64 } } },
            doc! { "$sort": { "n": -1 } },
        ];

        let mut cursor = self.wa_messages()
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        while let Some(d) = cursor.try_next().await.map_err(|e| e.to_string())? {
            out.push(AuditMessagesByTypeBucket {
                msg_type: d.get_str("_id").unwrap_or_default().to_string(),
                count: d.get_i64("n").unwrap_or(0).max(0) as u64,
            });
        }
        Ok(out)
    }

    async fn audit_first_responses(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<Vec<AuditFirstResponse>, String> {
        // Estrategia: por cada conversación con tráfico en el rango, calcular
        // (primer_out_después_del_primer_in - primer_in). Se hace en dos pasos
        // para evitar lookups o $cond complejos sobre nulls:
        //
        // 1. Aggregate: agrupar por (conversation_id, direction), tomar el
        //    primer timestamp + (sólo para `out`) el primer `sent_by`.
        // 2. En Rust: unir los dos lados por conversation_id, descartar las
        //    que no tienen ambos o donde first_out <= first_in.
        let match_stage = build_messages_match(filter);
        let pipeline = vec![
            doc! { "$match": match_stage },
            doc! { "$sort": { "conversation_id": 1, "timestamp": 1 } },
            doc! {
                "$group": {
                    "_id": { "conv": "$conversation_id", "dir": "$direction" },
                    "ts": { "$first": "$timestamp" },
                    "agent": { "$first": "$sent_by" },
                }
            },
        ];

        let mut cursor = self.wa_messages()
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        // (conv_id, "in"|"out") → (ts_millis, agent_id)
        let mut firsts: HashMap<(ObjectId, String), (i64, Option<String>)> = HashMap::new();
        while let Some(d) = cursor.try_next().await.map_err(|e| e.to_string())? {
            let id = match d.get_document("_id") {
                Ok(doc) => doc,
                Err(_) => continue,
            };
            let conv_id = match id.get_object_id("conv") {
                Ok(o) => o,
                Err(_) => continue,
            };
            let dir = id.get_str("dir").unwrap_or("").to_string();
            let ts_millis = match d.get_datetime("ts") {
                Ok(ts) => ts.timestamp_millis(),
                Err(_) => continue,
            };
            let agent = d.get_str("agent").ok().map(str::to_string);
            firsts.insert((conv_id, dir), (ts_millis, agent));
        }

        let mut out = Vec::new();
        // Recorrer las claves "in" y buscar la "out" hermana.
        let in_keys: Vec<ObjectId> = firsts
            .keys()
            .filter(|(_, d)| d == "in")
            .map(|(c, _)| *c)
            .collect();
        for conv_id in in_keys {
            let (in_ts, _) = match firsts.get(&(conv_id, "in".into())) {
                Some(v) => v.clone(),
                None => continue,
            };
            let (out_ts, agent) = match firsts.get(&(conv_id, "out".into())) {
                Some(v) => v.clone(),
                None => continue,
            };
            if out_ts <= in_ts {
                continue;
            }
            out.push(AuditFirstResponse {
                agent_id: agent,
                delta_seconds: (out_ts - in_ts) / 1000,
            });
        }
        Ok(out)
    }

    async fn audit_lifecycle_by_day(
        &self,
        from: DateTime,
        to: DateTime,
        business_phone: Option<&str>,
        granularity: &str,
    ) -> Result<Vec<AuditLifecycleByDayBucket>, String> {
        let date_format = granularity_to_date_format(granularity);
        let mut match_stage = doc! {
            "created_at": { "$gte": from, "$lte": to },
            "event_type": { "$in": ["created", "closed"] },
        };
        if let Some(b) = business_phone {
            match_stage.insert("business_phone", b);
        }

        let pipeline = vec![
            doc! { "$match": match_stage },
            doc! {
                "$group": {
                    "_id": {
                        "date": { "$dateToString": { "format": date_format, "date": "$created_at" } },
                        "type": "$event_type",
                    },
                    "n": { "$sum": 1_i64 }
                }
            },
            doc! { "$sort": { "_id.date": 1 } },
        ];

        let mut cursor = self.wa_conversation_events()
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let mut buckets: HashMap<String, AuditLifecycleByDayBucket> = HashMap::new();
        while let Some(d) = cursor.try_next().await.map_err(|e| e.to_string())? {
            let id = d.get_document("_id").map_err(|e| e.to_string())?;
            let date = id.get_str("date").unwrap_or_default().to_string();
            let etype = id.get_str("type").unwrap_or_default().to_string();
            let n = d.get_i64("n").unwrap_or(0).max(0) as u64;
            let entry = buckets.entry(date.clone()).or_insert_with(|| AuditLifecycleByDayBucket {
                date,
                new_conversations: 0,
                closed_conversations: 0,
            });
            match etype.as_str() {
                "created" => entry.new_conversations += n,
                "closed" => entry.closed_conversations += n,
                _ => {}
            }
        }
        let mut out: Vec<_> = buckets.into_values().collect();
        out.sort_by(|a, b| a.date.cmp(&b.date));
        Ok(out)
    }

    async fn audit_resolution_times(
        &self,
        from: DateTime,
        to: DateTime,
        business_phone: Option<&str>,
    ) -> Result<Vec<i64>, String> {
        // 1. Conversaciones cerradas en el rango (un evento `closed`).
        let mut match_closed = doc! {
            "event_type": "closed",
            "created_at": { "$gte": from, "$lte": to },
        };
        if let Some(b) = business_phone {
            match_closed.insert("business_phone", b);
        }

        let pipeline = vec![
            doc! { "$match": match_closed },
            doc! { "$sort": { "created_at": 1 } },
            // El último closed por conversación dentro del rango.
            doc! {
                "$group": {
                    "_id": "$conversation_id",
                    "closed_at": { "$last": "$created_at" }
                }
            },
        ];

        let mut cursor = self.wa_conversation_events()
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let mut closed_by_conv: HashMap<ObjectId, i64> = HashMap::new();
        while let Some(d) = cursor.try_next().await.map_err(|e| e.to_string())? {
            let conv_id = match d.get_object_id("_id") {
                Ok(o) => o,
                Err(_) => continue,
            };
            if let Ok(ts) = d.get_datetime("closed_at") {
                closed_by_conv.insert(conv_id, ts.timestamp_millis());
            }
        }

        if closed_by_conv.is_empty() {
            return Ok(Vec::new());
        }

        // 2. Para esas conversaciones, levantar `created_at` desde WaConversations.
        let conv_ids: Vec<ObjectId> = closed_by_conv.keys().copied().collect();
        let convs = self.find_conversations_by_ids(&conv_ids).await?;

        let mut deltas = Vec::with_capacity(closed_by_conv.len());
        for (conv_id, closed_ms) in closed_by_conv {
            if let Some(c) = convs.get(&conv_id) {
                let created_ms = c.created_at.timestamp_millis();
                if closed_ms > created_ms {
                    deltas.push((closed_ms - created_ms) / 1000);
                }
            }
        }
        Ok(deltas)
    }

    async fn backfill_conversation_events(&self) -> Result<u64, String> {
        // Recorre WaConversations y, para cada una sin eventos previos,
        // siembra: created (con created_at) y, si tiene assigned_to, taken
        // (con last_message_at como mejor proxy de cuándo fue asignada).
        // Idempotente vía chequeo de existencia previa.
        let convs_col = self.wa_conversations();
        let events_col = self.wa_conversation_events();

        let mut cursor = convs_col
            .find(doc! {})
            .await
            .map_err(|e| e.to_string())?;

        let mut inserted: u64 = 0;
        while let Some(conv) = cursor.try_next().await.map_err(|e| e.to_string())? {
            let conv_id = match conv.id {
                Some(id) => id,
                None => continue,
            };

            let already = events_col
                .count_documents(doc! { "conversation_id": &conv_id })
                .limit(1)
                .await
                .map_err(|e| e.to_string())?;
            if already > 0 {
                continue;
            }

            let created_doc = WaConversationEvent {
                id: None,
                conversation_id: conv_id,
                business_phone: conv.business_phone.clone(),
                event_type: "created".to_string(),
                actor_id: None,
                actor_name: None,
                target_id: None,
                target_name: None,
                note: Some("backfill".to_string()),
                created_at: conv.created_at,
            };
            events_col
                .insert_one(created_doc)
                .await
                .map_err(|e| e.to_string())?;
            inserted += 1;

            if let Some(assignee) = conv.assigned_to.as_deref() {
                let taken_doc = WaConversationEvent {
                    id: None,
                    conversation_id: conv_id,
                    business_phone: conv.business_phone.clone(),
                    event_type: "taken".to_string(),
                    actor_id: Some(assignee.to_string()),
                    actor_name: None,
                    target_id: None,
                    target_name: None,
                    note: Some("backfill".to_string()),
                    created_at: conv.last_message_at,
                };
                events_col
                    .insert_one(taken_doc)
                    .await
                    .map_err(|e| e.to_string())?;
                inserted += 1;
            }
        }

        Ok(inserted)
    }
}

/// Decodifica cursor con formato `<millis>_<hex_id>`.
/// Retorna `None` si el formato es inválido (se ignora silenciosamente → primera página).
fn decode_cursor(cursor: &str) -> Option<(DateTime, ObjectId)> {
    let (millis_str, oid_str) = cursor.split_once('_')?;
    let millis: i64 = millis_str.parse().ok()?;
    let oid = ObjectId::parse_str(oid_str).ok()?;
    Some((DateTime::from_millis(millis), oid))
}

/// Construye el `$match` común para los aggregates de `/audit/metrics` sobre
/// `WaMessages`: rango temporal + (opcional) `conversation_id ∈ ids`.
fn build_messages_match(filter: &AuditMetricsFilter<'_>) -> Document {
    let mut q = doc! {
        "timestamp": { "$gte": filter.from_date, "$lte": filter.to_date },
    };
    if let Some(ids) = filter.conversation_ids {
        q.insert("conversation_id", doc! { "$in": ids });
    }
    q
}

/// Mapea `granularity` (`day`/`week`/`month`) al formato `$dateToString` de Mongo.
/// `week` usa el ISO week (`%G-W%V`); `month` `%Y-%m`; default `day` `%Y-%m-%d`.
fn granularity_to_date_format(g: &str) -> &'static str {
    match g {
        "week" => "%G-W%V",
        "month" => "%Y-%m",
        _ => "%Y-%m-%d",
    }
}

// ============================================
// WaTemplateRepository — impl
// ============================================

#[async_trait]
impl WaTemplateRepository for MongoDB {
    async fn create_template(&self, template: WaTemplate) -> Result<WaTemplate, String> {
        let col = self.wa_templates();
        match col.insert_one(&template).await {
            Ok(res) => {
                let inserted_id = res.inserted_id
                    .as_object_id()
                    .ok_or_else(|| "inserted_id is not ObjectId".to_string())?;
                col.find_one(doc! { "_id": inserted_id })
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "template not found after insert".to_string())
            }
            Err(e) => {
                // Detectar violación de índice único (código 11000)
                if let mongodb::error::ErrorKind::Write(
                    mongodb::error::WriteFailure::WriteError(ref we)
                ) = *e.kind {
                    if we.code == 11000 {
                        return Err("name_already_exists".into());
                    }
                }
                Err(e.to_string())
            }
        }
    }

    async fn find_template_by_id(&self, id: &ObjectId) -> Result<Option<WaTemplate>, String> {
        self.wa_templates()
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_template_by_phone_name_lang(
        &self,
        phone_number_id: &str,
        name: &str,
        language: &str,
    ) -> Result<Option<WaTemplate>, String> {
        self.wa_templates()
            .find_one(doc! {
                "phone_number_id": phone_number_id,
                "name": name,
                "language": language,
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_template_by_meta_id(
        &self,
        meta_template_id: &str,
    ) -> Result<Option<WaTemplate>, String> {
        self.wa_templates()
            .find_one(doc! { "meta_template_id": meta_template_id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_templates_filtered(
        &self,
        filter: WaTemplateListFilter<'_>,
    ) -> Result<Vec<WaTemplate>, String> {
        let mut query = doc! { "phone_number_id": filter.phone_number_id };

        // Status filter (multi)
        if let Some(statuses) = filter.status {
            if !statuses.is_empty() {
                let status_bson: Vec<mongodb::bson::Bson> = statuses
                    .iter()
                    .filter_map(|s| mongodb::bson::to_bson(s).ok())
                    .collect();
                query.insert("status", doc! { "$in": status_bson });
            }
        }

        // Category filter
        if let Some(cat) = filter.category {
            let cat_bson = mongodb::bson::to_bson(&cat).map_err(|e| e.to_string())?;
            query.insert("category", cat_bson);
        }

        // only_system filter
        if filter.only_system {
            query.insert("is_system", true);
        }

        // Search: substring case-insensitive en display_name OR name
        // Se usa $and para poder combinar sin colisión con el cursor $or.
        if let Some(search) = filter.search {
            if !search.is_empty() {
                let escaped = regex_escape(search);
                let search_clause = doc! {
                    "$or": [
                        { "display_name": { "$regex": &escaped, "$options": "i" } },
                        { "name": { "$regex": &escaped, "$options": "i" } },
                    ]
                };
                // Acumular en $and para no colisionar con el cursor $or
                let mut and_clauses: Vec<mongodb::bson::Document> = query
                    .remove("$and")
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|b| b.as_document().cloned())
                    .collect();
                and_clauses.push(search_clause);
                query.insert("$and", mongodb::bson::to_bson(&and_clauses).unwrap_or(mongodb::bson::Bson::Array(vec![])));
            }
        }

        // Cursor-based pagination (mismo patrón que get_conversations)
        if let Some(c) = filter.cursor {
            if let Some((ts, oid)) = decode_cursor(c) {
                let cursor_clause = doc! {
                    "$or": [
                        { "created_at": { "$lt": ts } },
                        { "created_at": ts, "_id": { "$lt": oid } },
                    ]
                };
                let mut and_clauses: Vec<mongodb::bson::Document> = query
                    .remove("$and")
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|b| b.as_document().cloned())
                    .collect();
                and_clauses.push(cursor_clause);
                query.insert("$and", mongodb::bson::to_bson(&and_clauses).unwrap_or(mongodb::bson::Bson::Array(vec![])));
            }
        }

        // Hard-cap limit a 100
        let limit = filter.limit.min(100).max(1);

        let opts = FindOptions::builder()
            .sort(doc! { "created_at": -1, "_id": -1 })
            .limit(limit)
            .build();

        self.wa_templates()
            .find(query)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_template(
        &self,
        id: &ObjectId,
        patch: WaTemplateUpdatePatch,
    ) -> Result<Option<WaTemplate>, String> {
        use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};

        let mut set_doc = doc! { "updated_at": DateTime::now() };
        let mut unset_doc = Document::new();

        if let Some(v) = patch.name {
            set_doc.insert("name", v);
        }
        if let Some(v) = patch.display_name {
            set_doc.insert("display_name", v);
        }
        if let Some(v) = patch.name_input {
            set_doc.insert("name_input", v);
        }
        if let Some(v) = patch.category {
            let bson = mongodb::bson::to_bson(&v).map_err(|e| e.to_string())?;
            set_doc.insert("category", bson);
        }
        if let Some(v) = patch.components {
            let bson = mongodb::bson::to_bson(&v).map_err(|e| e.to_string())?;
            set_doc.insert("components", bson);
        }
        if let Some(v) = patch.body_placeholders {
            set_doc.insert("body_placeholders", v as i64);
        }
        if let Some(v) = patch.status {
            let bson = mongodb::bson::to_bson(&v).map_err(|e| e.to_string())?;
            set_doc.insert("status", bson);
        }
        if let Some(v) = patch.is_system {
            set_doc.insert("is_system", v);
        }
        if let Some(v) = patch.submit_to_meta {
            set_doc.insert("submit_to_meta", v);
        }

        // Campos nullable (tri-state)
        match patch.rejection_reason {
            Some(Some(r)) => { set_doc.insert("rejection_reason", r); }
            Some(None)    => { unset_doc.insert("rejection_reason", ""); }
            None          => {}
        }
        match patch.meta_template_id {
            Some(Some(m)) => { set_doc.insert("meta_template_id", m); }
            Some(None)    => { unset_doc.insert("meta_template_id", ""); }
            None          => {}
        }

        let mut update = doc! { "$set": set_doc };
        if !unset_doc.is_empty() {
            update.insert("$unset", unset_doc);
        }

        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();

        self.wa_templates()
            .find_one_and_update(doc! { "_id": id }, update)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn update_template_status(
        &self,
        meta_template_id: &str,
        status: WaTemplateStatus,
        rejection_reason: Option<String>,
    ) -> Result<Option<(WaTemplate, WaTemplateStatus)>, String> {
        use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};

        // 1. Leer el doc actual para capturar el status previo
        let existing = self.wa_templates()
            .find_one(doc! { "meta_template_id": meta_template_id })
            .await
            .map_err(|e| e.to_string())?;

        let prev = match existing {
            Some(ref t) => t.status,
            None => return Ok(None),
        };

        // 2. Armar el update
        let status_bson = mongodb::bson::to_bson(&status).map_err(|e| e.to_string())?;
        let mut set_doc = doc! {
            "status": status_bson,
            "updated_at": DateTime::now(),
        };
        let mut unset_doc = Document::new();

        match rejection_reason {
            Some(r) => { set_doc.insert("rejection_reason", r); }
            None    => { unset_doc.insert("rejection_reason", ""); }
        }

        let mut update = doc! { "$set": set_doc };
        if !unset_doc.is_empty() {
            update.insert("$unset", unset_doc);
        }

        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();

        let updated = self.wa_templates()
            .find_one_and_update(doc! { "meta_template_id": meta_template_id }, update)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?;

        Ok(updated.map(|t| (t, prev)))
    }

    async fn delete_template(&self, id: &ObjectId) -> Result<bool, String> {
        let res = self.wa_templates()
            .delete_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.deleted_count > 0)
    }

    async fn count_templates_in_purposes(
        &self,
        phone_number_id: &str,
        name: &str,
    ) -> Result<Vec<WaPurposeUsage>, String> {
        // Buscar el WaSettings del phone_number_id
        let settings = self.wa_settings()
            .find_one(doc! { "phone_number_id": phone_number_id })
            .await
            .map_err(|e| e.to_string())?;

        let settings = match settings {
            Some(s) => s,
            None => return Ok(vec![]),
        };

        let mut usages = Vec::new();
        let purposes = &settings.purposes;

        // Verificar cada propósito configurado
        if let Some(ref cfg) = purposes.otp {
            if cfg.template_name == name {
                usages.push(WaPurposeUsage {
                    key: "otp".to_string(),
                    label: "OTP / Códigos de verificación".to_string(),
                });
            }
        }
        if let Some(ref cfg) = purposes.notifications {
            if cfg.template_name == name {
                usages.push(WaPurposeUsage {
                    key: "notifications".to_string(),
                    label: "Notificaciones".to_string(),
                });
            }
        }
        if let Some(ref cfg) = purposes.payment_reminder {
            if cfg.template_name == name {
                usages.push(WaPurposeUsage {
                    key: "payment_reminder".to_string(),
                    label: "Recordatorios de pago".to_string(),
                });
            }
        }

        Ok(usages)
    }
}

/// Escapa caracteres especiales de regex para usar en `$regex` de MongoDB.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        if matches!(ch, '\\' | '^' | '$' | '.' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

// ============================================
// WaTemplateMediaRepository — impl (GridFS)
// ============================================

/// Struct de proyección para leer metadatos de `wa_template_media.files`.
/// GridFS almacena campos custom en `metadata`.
#[derive(serde::Deserialize)]
struct WaTemplateMediaFilesDoc {
    #[serde(rename = "_id")]
    id: ObjectId,
    length: i64,
    metadata: Option<WaTemplateMediaMetadata>,
}

#[derive(serde::Deserialize)]
struct WaTemplateMediaMetadata {
    #[serde(default)]
    phone_number_id: String,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    sha256: String,
}

#[async_trait]
impl WaTemplateMediaRepository for MongoDB {
    async fn store_template_media(
        &self,
        input: StoreTemplateMediaInput<'_>,
    ) -> Result<WaTemplateMediaRef, String> {
        let files_col = self
            .db
            .collection::<WaTemplateMediaFilesDoc>("wa_template_media.files");

        // --- Dedup: buscar si ya existe (phone_number_id, sha256) ---
        let existing = files_col
            .find_one(doc! {
                "metadata.phone_number_id": input.phone_number_id,
                "metadata.sha256": input.sha256,
            })
            .await
            .map_err(|e| format!("store_template_media dedup query: {e}"))?;

        if let Some(doc) = existing {
            let meta = doc.metadata.unwrap_or_else(|| WaTemplateMediaMetadata {
                phone_number_id: input.phone_number_id.to_string(),
                mime_type: input.mime_type.to_string(),
                sha256: input.sha256.to_string(),
            });
            return Ok(WaTemplateMediaRef {
                id: doc.id,
                phone_number_id: meta.phone_number_id,
                mime_type: meta.mime_type,
                sha256: meta.sha256,
                file_size: doc.length as u64,
            });
        }

        // --- Upload nuevo ---
        let filename = format!("{}_{}", input.phone_number_id, input.sha256);
        let meta_doc = doc! {
            "phone_number_id": input.phone_number_id,
            "mime_type":        input.mime_type,
            "sha256":           input.sha256,
            "format":           input.format,
            "uploaded_by":      input.uploaded_by,
            "uploaded_by_name": input.uploaded_by_name,
            "uploaded_at":      DateTime::now(),
        };
        let upload_opts = mongodb::options::GridFsUploadOptions::builder()
            .metadata(meta_doc)
            .build();

        let bucket = self.wa_template_media_bucket();
        let mut stream = bucket
            .open_upload_stream(filename)
            .with_options(upload_opts)
            .await
            .map_err(|e| format!("store_template_media open_upload_stream: {e}"))?;

        // Capturar el id ANTES de cerrar (generado client-side)
        let file_id = stream
            .id()
            .as_object_id()
            .ok_or_else(|| "store_template_media: upload stream id is not ObjectId".to_string())?;

        stream
            .write_all(input.bytes)
            .await
            .map_err(|e| format!("store_template_media write_all: {e}"))?;

        stream
            .close()
            .await
            .map_err(|e| {
                // Race condition: duplicate key en índice único → re-query y devolver existente
                let msg = e.to_string();
                if msg.contains("11000") || msg.contains("duplicate key") {
                    "store_template_media_duplicate_key".to_string()
                } else {
                    format!("store_template_media close: {e}")
                }
            })?;

        Ok(WaTemplateMediaRef {
            id: file_id,
            phone_number_id: input.phone_number_id.to_string(),
            mime_type: input.mime_type.to_string(),
            sha256: input.sha256.to_string(),
            file_size: input.bytes.len() as u64,
        })
    }

    async fn find_template_media_by_id(
        &self,
        id: &ObjectId,
    ) -> Result<Option<WaTemplateMediaRef>, String> {
        let files_col = self
            .db
            .collection::<WaTemplateMediaFilesDoc>("wa_template_media.files");

        let doc = files_col
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| format!("find_template_media_by_id: {e}"))?;

        Ok(doc.map(|d| {
            let meta = d.metadata.unwrap_or_else(|| WaTemplateMediaMetadata {
                phone_number_id: String::new(),
                mime_type: String::new(),
                sha256: String::new(),
            });
            WaTemplateMediaRef {
                id: d.id,
                phone_number_id: meta.phone_number_id,
                mime_type: meta.mime_type,
                sha256: meta.sha256,
                file_size: d.length as u64,
            }
        }))
    }

    async fn read_template_media_bytes(
        &self,
        id: &ObjectId,
    ) -> Result<Option<(Vec<u8>, String)>, String> {
        // 1. Obtener mime_type de .files antes de abrir el stream de descarga
        let files_col = self
            .db
            .collection::<WaTemplateMediaFilesDoc>("wa_template_media.files");

        let file_doc = files_col
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| format!("read_template_media_bytes files lookup: {e}"))?;

        let file_doc = match file_doc {
            Some(d) => d,
            None => return Ok(None),
        };

        let mime_type = file_doc
            .metadata
            .as_ref()
            .map(|m| m.mime_type.clone())
            .unwrap_or_default();

        // 2. Abrir download stream y leer todos los bytes
        let bucket = self.wa_template_media_bucket();
        let mut stream = bucket
            .open_download_stream(mongodb::bson::Bson::ObjectId(*id))
            .await
            .map_err(|e| {
                let msg = e.to_string();
                // FileNotFound → tratar como None
                if msg.contains("FileNotFound") || msg.contains("file not found") {
                    "read_template_media_not_found".to_string()
                } else {
                    format!("read_template_media_bytes open_download_stream: {e}")
                }
            })?;

        let mut bytes = Vec::with_capacity(file_doc.length as usize);
        stream
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| format!("read_template_media_bytes read_to_end: {e}"))?;

        Ok(Some((bytes, mime_type)))
    }

    async fn delete_template_media(
        &self,
        id: &ObjectId,
    ) -> Result<bool, String> {
        let bucket = self.wa_template_media_bucket();
        match bucket
            .delete(mongodb::bson::Bson::ObjectId(*id))
            .await
        {
            Ok(()) => Ok(true),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("FileNotFound") || msg.contains("file not found") {
                    Ok(false)
                } else {
                    Err(format!("delete_template_media: {e}"))
                }
            }
        }
    }
}
