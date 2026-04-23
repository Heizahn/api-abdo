use async_trait::async_trait;
use mongodb::bson::{doc, oid::ObjectId, DateTime, Document};
use mongodb::options::{FindOptions, UpdateOptions};
use futures::TryStreamExt;

use std::collections::HashMap;

use crate::db::{ConversationTouch, UpdateQuickReplyPatch, WhatsAppRepository};
use crate::db::mongo::MongoDB;
use crate::models::whatsapp::{ConversationStats, UrlPreview, WaConversation, WaConversationOpen, WaMessage, WaPurposesPatch, WaQuickReply, WaSettings};

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

    pub(crate) fn wa_quick_replies(&self) -> mongodb::Collection<WaQuickReply> {
        self.db.collection::<WaQuickReply>("WaQuickReplies")
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
        self.wa_conversations()
            .update_one(
                doc! { "_id": id },
                doc! { "$set": { "last_inbound_at": when } },
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
        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();

        // Atómico: toma si sigue `pending`, sin importar si ya tiene otro dueño.
        // Permite reasignación manual en chats `pending` asignados a otro agente.
        // Idempotente: tomar mi propia conversación devuelve el doc sin cambios.
        // No toca `status` — la transición a `in_progress` ocurre en el primer
        // GET /messages del asignado.
        let res = self.wa_conversations()
            .find_one_and_update(
                doc! { "_id": id, "status": "pending" },
                doc! { "$set": { "assigned_to": agent_id } },
            )
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
        user_workspaces: &[ObjectId],
        filter_workspace_id: Option<&ObjectId>,
        active_filter: Option<bool>,
    ) -> Result<Vec<WaQuickReply>, String> {
        if user_workspaces.is_empty() {
            return Ok(Vec::new());
        }
        let scope = match filter_workspace_id {
            // Si piden un workspace puntual, sólo devolver si está en los del user.
            Some(id) if user_workspaces.contains(id) => vec![*id],
            Some(_) => return Ok(Vec::new()),
            None => user_workspaces.to_vec(),
        };
        let mut filter = doc! { "workspace_ids": { "$in": &scope } };
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
}

/// Decodifica cursor con formato `<millis>_<hex_id>`.
/// Retorna `None` si el formato es inválido (se ignora silenciosamente → primera página).
fn decode_cursor(cursor: &str) -> Option<(DateTime, ObjectId)> {
    let (millis_str, oid_str) = cursor.split_once('_')?;
    let millis: i64 = millis_str.parse().ok()?;
    let oid = ObjectId::parse_str(oid_str).ok()?;
    Some((DateTime::from_millis(millis), oid))
}
