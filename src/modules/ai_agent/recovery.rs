use std::sync::Arc;
use std::time::Duration;

use mongodb::bson::{oid::ObjectId, DateTime};

use crate::{db::WhatsAppRepository, models::whatsapp::WaMessage, state::AppState};

use super::dispatch::dispatch_inbound_async;

/// Runs once at startup. Finds conversations where the last message was inbound
/// and the AI never processed it (service crashed mid-dispatch or was down).
/// Re-triggers dispatch for each orphaned conversation with a staggered delay.
pub async fn run_ai_recovery(state: Arc<AppState>) {
    tokio::time::sleep(Duration::from_secs(5)).await;

    let cutoff_millis = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::hours(2))
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);
    let cutoff = DateTime::from_millis(cutoff_millis);

    let convs = match state.db.find_orphaned_ai_conversations(cutoff).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("[ai_recovery] DB query failed: {}", e);
            return;
        }
    };

    if convs.is_empty() {
        tracing::info!("[ai_recovery] no orphaned conversations");
        return;
    }

    tracing::info!(
        "[ai_recovery] {} orphaned conversation(s) — re-dispatching",
        convs.len()
    );

    for (i, conv) in convs.into_iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        let conv_id = match conv.id {
            Some(id) => id,
            None => continue,
        };

        let settings = match state
            .db
            .find_wa_settings_by_phone(&conv.business_phone)
            .await
        {
            Ok(Some(s)) => s,
            Ok(None) => {
                tracing::warn!(
                    "[ai_recovery] no WaSettings for {} — skipping conv {}",
                    conv.business_phone,
                    conv_id.to_hex()
                );
                continue;
            }
            Err(e) => {
                tracing::error!("[ai_recovery] settings lookup error: {}", e);
                continue;
            }
        };

        let workspace_id = match settings.id {
            Some(id) => id,
            None => continue,
        };

        let synthetic = WaMessage {
            id: Some(ObjectId::new()),
            conversation_id: conv_id,
            wa_message_id: format!("recovery_{}", conv_id.to_hex()),
            direction: "in".to_string(),
            msg_type: "text".to_string(),
            body: None,
            media_id: None,
            media_mime_type: None,
            media_filename: None,
            status: None,
            meta_error_code: None,
            meta_error_title: None,
            meta_error_message: None,
            meta_error_details: None,
            failed_at: None,
            sent_by: None,
            source: None,
            campaign_id: None,
            campaign_recipient_id: None,
            read_by_user_id: None,
            read_at: None,
            idempotency_key: None,
            reply_to_wa_message_id: None,
            is_forwarded: None,
            is_frequently_forwarded: None,
            ai_processed_at: None,
            url_preview: None,
            voice: false,
            template_name: None,
            template_language: None,
            template_components: None,
            interactive_payload: None,
            contacts_payload: None,
            location: None,
            reactions: vec![],
            raw_payload: None,
            audio_transcription: None,
            timestamp: DateTime::now(),
        };

        dispatch_inbound_async(state.clone(), synthetic, workspace_id);

        tracing::info!(
            "[ai_recovery] dispatched conv {} ({})",
            conv_id.to_hex(),
            conv.business_phone
        );
    }
}
