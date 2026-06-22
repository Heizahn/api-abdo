//! Implementación MongoDB de `AiConfigRepository`.
//!
//! Colección `AiConfig` — singleton: `filter: doc!{}` garantiza que
//! `find_one` + `find_one_and_update` operen siempre sobre el único doc.

use async_trait::async_trait;
use mongodb::bson::{self, doc, DateTime as BsonDateTime};
use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};

use super::MongoDB;
use crate::db::AiConfigRepository;
use crate::models::ai_agent::AiConfig;

const COLLECTION: &str = "AiConfig";

impl MongoDB {
    fn ai_config_col(&self) -> mongodb::Collection<AiConfig> {
        self.db.collection::<AiConfig>(COLLECTION)
    }
}

#[async_trait]
impl AiConfigRepository for MongoDB {
    async fn get_ai_config(&self) -> Result<Option<AiConfig>, String> {
        self.ai_config_col()
            .find_one(doc! {})
            .await
            .map_err(|e| format!("ai_config_find_one: {e}"))
    }

    async fn upsert_ai_config(
        &self,
        api_key_cipher: Option<String>,
        default_model: Option<String>,
        audio_transcription_enabled: Option<bool>,
        stt_model: Option<String>,
        stt_language: Option<String>,
        show_audio_transcription: Option<bool>,
        ai_uses_audio_transcription: Option<bool>,
        max_audio_transcription_seconds: Option<u32>,
        editor_id: &str,
    ) -> Result<AiConfig, String> {
        let mut set_doc = doc! {
            "updated_at": BsonDateTime::now(),
            "editor_id": editor_id,
        };

        // Solo escribir los campos que el caller proveyó (y que no estén vacíos).
        if let Some(c) = api_key_cipher.filter(|s| !s.is_empty()) {
            set_doc.insert("openrouter_api_key", c);
        }
        if let Some(m) = default_model {
            set_doc.insert("default_model", m);
        }
        if let Some(v) = audio_transcription_enabled {
            set_doc.insert("audio_transcription_enabled", v);
        }
        if let Some(v) = stt_model {
            set_doc.insert("stt_model", v);
        }
        if let Some(v) = stt_language {
            set_doc.insert("stt_language", v);
        }
        if let Some(v) = show_audio_transcription {
            set_doc.insert("show_audio_transcription", v);
        }
        if let Some(v) = ai_uses_audio_transcription {
            set_doc.insert("ai_uses_audio_transcription", v);
        }
        if let Some(v) = max_audio_transcription_seconds {
            set_doc.insert("max_audio_transcription_seconds", v);
        }

        // $setOnInsert: fields that seed the doc on first-ever insert.
        // Eliminamos claves que ya están en $set para evitar que Mongo rechace
        // paths solapados entre operadores.
        let on_insert_candidates = doc! {
            "openrouter_api_key": "",
            "default_model": "",
        };
        let mut on_insert_filtered = bson::Document::new();
        for (k, v) in on_insert_candidates {
            if !set_doc.contains_key(&k) {
                on_insert_filtered.insert(k, v);
            }
        }

        let update = doc! {
            "$set": set_doc,
            "$setOnInsert": on_insert_filtered,
        };

        let opts = FindOneAndUpdateOptions::builder()
            .upsert(true)
            .return_document(ReturnDocument::After)
            .build();

        self.ai_config_col()
            .find_one_and_update(doc! {}, update)
            .with_options(opts)
            .await
            .map_err(|e| format!("ai_config_upsert: {e}"))?
            .ok_or_else(|| "ai_config_upsert: no document returned after upsert".to_string())
    }
}
