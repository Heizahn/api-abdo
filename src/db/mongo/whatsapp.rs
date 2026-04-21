use async_trait::async_trait;
use mongodb::bson::{doc, oid::ObjectId, DateTime, Document};
use mongodb::options::{FindOptions, UpdateOptions};
use futures::TryStreamExt;

use crate::db::WhatsAppRepository;
use crate::db::mongo::MongoDB;
use crate::models::whatsapp::{WaConversation, WaMessage, WaSettings};

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
        preview: &str,
        increment_unread: bool,
        last_message_at: Option<DateTime>,
    ) -> Result<(), String> {
        let ts = last_message_at.unwrap_or_else(DateTime::now);
        let unread_update: i32 = if increment_unread { 1 } else { 0 };

        self.wa_conversations()
            .update_one(
                doc! { "_id": id },
                doc! {
                    "$set": {
                        "last_message_at": ts,
                        "last_message_preview": preview,
                    },
                    "$inc": { "unread_count": unread_update }
                },
            )
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
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

    async fn assign_conversation(
        &self,
        id: &ObjectId,
        assigned_to: Option<&str>,
    ) -> Result<(), String> {
        let update = match assigned_to {
            Some(uid) => doc! { "$set": { "assigned_to": uid, "status": "in_progress" } },
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
        // Atómico: solo toma si sigue siendo pending y sin agente asignado.
        // `assigned_to: null` matchea tanto null como "campo ausente".
        self.wa_conversations()
            .find_one_and_update(
                doc! {
                    "_id": id,
                    "status": "pending",
                    "assigned_to": mongodb::bson::Bson::Null,
                },
                doc! { "$set": { "assigned_to": agent_id, "status": "in_progress" } },
            )
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())
    }

    async fn reset_unread(&self, id: &ObjectId) -> Result<(), String> {
        self.wa_conversations()
            .update_one(doc! { "_id": id }, doc! { "$set": { "unread_count": 0 } })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
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
        agents: Option<Vec<String>>,
        active: Option<bool>,
    ) -> Result<(), String> {
        let mut set_doc = doc! { "updated_at": DateTime::now() };
        if let Some(a) = agents {
            set_doc.insert("agents", mongodb::bson::to_bson(&a).unwrap());
        }
        if let Some(act) = active {
            set_doc.insert("active", act);
        }
        self.wa_settings()
            .update_one(doc! { "_id": id }, doc! { "$set": set_doc })
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
}

/// Decodifica cursor con formato `<millis>_<hex_id>`.
/// Retorna `None` si el formato es inválido (se ignora silenciosamente → primera página).
fn decode_cursor(cursor: &str) -> Option<(DateTime, ObjectId)> {
    let (millis_str, oid_str) = cursor.split_once('_')?;
    let millis: i64 = millis_str.parse().ok()?;
    let oid = ObjectId::parse_str(oid_str).ok()?;
    Some((DateTime::from_millis(millis), oid))
}
