use async_trait::async_trait;
use mongodb::bson::{doc, oid::ObjectId, DateTime, Document};
use mongodb::options::{FindOptions, UpdateOptions};
use futures::TryStreamExt;

use crate::db::WhatsAppRepository;
use crate::db::mongo::MongoDB;
use crate::models::whatsapp::{WaConversation, WaMessage};

impl MongoDB {
    pub(crate) fn wa_conversations(&self) -> mongodb::Collection<WaConversation> {
        self.db.collection::<WaConversation>("wa_conversations")
    }

    pub(crate) fn wa_messages(&self) -> mongodb::Collection<WaMessage> {
        self.db.collection::<WaMessage>("wa_messages")
    }
}

#[async_trait]
impl WhatsAppRepository for MongoDB {
    async fn find_conversation_by_phone(&self, phone: &str) -> Result<Option<WaConversation>, String> {
        self.wa_conversations()
            .find_one(doc! { "phone": phone })
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
        phone: &str,
        name: Option<String>,
    ) -> Result<WaConversation, String> {
        let now = DateTime::now();
        let col = self.wa_conversations();

        let opts = UpdateOptions::builder().upsert(true).build();
        col.update_one(
            doc! { "phone": phone },
            doc! {
                "$setOnInsert": {
                    "phone": phone,
                    "status": "open",
                    "unread_count": 0,
                    "created_at": now,
                    "last_message_at": now,
                },
                "$set": {
                    "name": &name,
                }
            },
        )
        .with_options(opts)
        .await
        .map_err(|e| e.to_string())?;

        self.find_conversation_by_phone(phone)
            .await?
            .ok_or_else(|| "conversation not found after upsert".to_string())
    }

    async fn touch_conversation(
        &self,
        id: &ObjectId,
        preview: &str,
        increment_unread: bool,
    ) -> Result<(), String> {
        let now = DateTime::now();
        let unread_update: i32 = if increment_unread { 1 } else { 0 };

        self.wa_conversations()
            .update_one(
                doc! { "_id": id },
                doc! {
                    "$set": {
                        "last_message_at": now,
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

        // Deduplicar por wa_message_id
        if let Ok(Some(existing)) = col
            .find_one(doc! { "wa_message_id": &message.wa_message_id })
            .await
        {
            return Ok(existing);
        }

        let result = col.insert_one(&message).await.map_err(|e| e.to_string())?;
        let id = result.inserted_id.as_object_id().unwrap();

        col.find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "message not found after insert".to_string())
    }

    async fn get_conversations(
        &self,
        status: Option<&str>,
        assigned_to: Option<&str>,
        skip: u64,
        limit: i64,
    ) -> Result<(Vec<WaConversation>, u64), String> {
        let mut filter = Document::new();
        if let Some(s) = status {
            filter.insert("status", s);
        }
        if let Some(a) = assigned_to {
            filter.insert("assigned_to", a);
        }

        let total = self.wa_conversations()
            .count_documents(filter.clone())
            .await
            .map_err(|e| e.to_string())?;

        let opts = FindOptions::builder()
            .sort(doc! { "last_message_at": -1 })
            .skip(skip)
            .limit(limit)
            .build();

        let items = self.wa_conversations()
            .find(filter)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())?;

        Ok((items, total))
    }

    async fn get_messages(
        &self,
        conversation_id: &ObjectId,
        skip: u64,
        limit: i64,
    ) -> Result<(Vec<WaMessage>, u64), String> {
        let filter = doc! { "conversation_id": conversation_id };

        let total = self.wa_messages()
            .count_documents(filter.clone())
            .await
            .map_err(|e| e.to_string())?;

        let opts = FindOptions::builder()
            .sort(doc! { "timestamp": -1 })
            .skip(skip)
            .limit(limit)
            .build();

        let items = self.wa_messages()
            .find(filter)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())?;

        Ok((items, total))
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
            Some(uid) => doc! { "$set": { "assigned_to": uid } },
            None => doc! { "$unset": { "assigned_to": "" } },
        };
        self.wa_conversations()
            .update_one(doc! { "_id": id }, update)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn reset_unread(&self, id: &ObjectId) -> Result<(), String> {
        self.wa_conversations()
            .update_one(doc! { "_id": id }, doc! { "$set": { "unread_count": 0 } })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn update_message_status(&self, wa_message_id: &str, status: &str) -> Result<(), String> {
        self.wa_messages()
            .update_one(
                doc! { "wa_message_id": wa_message_id },
                doc! { "$set": { "status": status } },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
