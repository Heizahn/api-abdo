use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    db::{UserRepository, WhatsAppRepository},
    error::ApiError,
    models::whatsapp::{MessageItem, ReplyToItem, WaConversation, WaMessage},
    state::AppState,
};

use super::{response, time, workspace};

pub(crate) fn msg_to_item(
    m: WaMessage,
    from_user_name: Option<String>,
    reply_to: Option<ReplyToItem>,
) -> MessageItem {
    MessageItem {
        id: m.id.map(|o| o.to_hex()).unwrap_or_default(),
        conversation_id: m.conversation_id.to_hex(),
        wa_message_id: m.wa_message_id,
        direction: m.direction,
        msg_type: m.msg_type,
        content: m.body,
        media_id: m.media_id,
        media_mime_type: m.media_mime_type,
        media_filename: m.media_filename,
        status: m.status,
        meta_error_code: m.meta_error_code,
        meta_error_title: m.meta_error_title,
        meta_error_message: m.meta_error_message,
        meta_error_details: m.meta_error_details,
        failed_at: m.failed_at.map(time::iso8601),
        from_user_id: m.sent_by,
        from_user_name,
        source: m.source,
        campaign_id: m.campaign_id.map(|id| id.to_hex()),
        campaign_recipient_id: m.campaign_recipient_id.map(|id| id.to_hex()),
        idempotency_key: m.idempotency_key,
        reply_to,
        is_forwarded: m.is_forwarded,
        is_frequently_forwarded: m.is_frequently_forwarded,
        url_preview: m.url_preview,
        voice: m.voice,
        template_name: m.template_name,
        template_language: m.template_language,
        template_components: m.template_components,
        interactive_payload: m.interactive_payload,
        contacts_payload: m.contacts_payload,
        location: m.location,
        reactions: m.reactions,
        raw_payload: m.raw_payload,
        ai_processed_at: m.ai_processed_at.map(time::iso8601),
        created_at: time::iso8601(m.timestamp),
    }
}

/// Resuelve un único mensaje (incluyendo `sent_by_name` y `reply_to`) para
/// contratos de WS / response.
pub(crate) async fn build_message_item(state: &Arc<AppState>, m: WaMessage) -> MessageItem {
    let name = match m.sent_by.as_deref() {
        Some(id) => state
            .db
            .find_user_by_id(id)
            .await
            .ok()
            .flatten()
            .map(|u| u.name),
        None => None,
    };
    let reply_to = resolve_reply_to_for_one(state, &m).await;
    msg_to_item(m, name, reply_to)
}

/// Atajo usado por listados/WS para un solo mensaje citado.
pub(crate) async fn resolve_reply_to_for_one(
    state: &Arc<AppState>,
    m: &WaMessage,
) -> Option<ReplyToItem> {
    let wid = m.reply_to_wa_message_id.as_ref()?;
    let items = resolve_reply_to_items(state, std::slice::from_ref(m)).await;
    items.get(wid).cloned()
}

/// Batch-resuelve los `reply_to` de un conjunto de mensajes.
pub(crate) async fn resolve_reply_to_items(
    state: &Arc<AppState>,
    messages: &[WaMessage],
) -> HashMap<String, ReplyToItem> {
    let mut wa_ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.reply_to_wa_message_id.clone())
        .collect();
    wa_ids.sort();
    wa_ids.dedup();
    if wa_ids.is_empty() {
        return HashMap::new();
    }

    let originals = match state.db.find_messages_by_wa_ids(&wa_ids).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "resolve_reply_to_items find_messages_by_wa_ids error: {}",
                e
            );
            return HashMap::new();
        }
    };

    let mut sender_ids: Vec<String> = originals
        .values()
        .filter_map(|m| m.sent_by.clone())
        .collect();
    sender_ids.sort();
    sender_ids.dedup();

    let mut names: HashMap<String, String> = HashMap::new();
    for id in sender_ids {
        if let Ok(Some(u)) = state.db.find_user_by_id(&id).await {
            names.insert(id, u.name);
        }
    }

    originals
        .into_iter()
        .map(|(wa_id, m)| {
            let preview_content = m
                .body
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| preview_truncate(s, 80));
            let from_user_name = m.sent_by.as_deref().and_then(|id| names.get(id).cloned());
            let item = ReplyToItem {
                wa_message_id: wa_id.clone(),
                preview_content,
                preview_type: m.msg_type,
                direction: m.direction,
                from_user_name,
            };
            (wa_id, item)
        })
        .collect()
}

fn preview_truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

pub(crate) async fn resolve_last_message_agent_name_one(
    state: &Arc<AppState>,
    conv: &WaConversation,
) -> Option<String> {
    let id = conv.last_message_from_user_id.as_deref()?;
    state
        .db
        .find_user_by_id(id)
        .await
        .ok()
        .flatten()
        .map(|u| u.name)
}

pub(crate) async fn resolve_assigned_agent_name_one(
    state: &Arc<AppState>,
    conv: &WaConversation,
) -> Option<String> {
    let id = conv.assigned_to.as_deref()?;
    state
        .db
        .find_user_by_id(id)
        .await
        .ok()
        .flatten()
        .map(|u| u.name)
}

pub(crate) async fn resolve_user_name_by_id(
    state: &Arc<AppState>,
    user_id: &str,
) -> Option<String> {
    if user_id.trim().is_empty() {
        return None;
    }
    state
        .db
        .find_user_by_id(user_id)
        .await
        .ok()
        .flatten()
        .map(|u| u.name)
}

pub(crate) async fn resolve_customer_name(
    state: &Arc<AppState>,
    conv: &WaConversation,
) -> Option<String> {
    use crate::db::ProfileRepository;

    if let Some(cid) = conv.client_id {
        let map = state.db.get_client_names_by_ids(&[cid]).await.ok()?;
        if let Some(n) = map.get(&cid) {
            return Some(n.clone());
        }
    }

    let map = state
        .db
        .get_client_names_by_phones(&[conv.phone.clone()])
        .await
        .ok()?;

    map.get(&conv.phone).cloned()
}

pub(crate) async fn build_conversation_item(
    state: &Arc<AppState>,
    conv: WaConversation,
    caller_id: &str,
) -> Result<crate::models::whatsapp::ConversationItem, ApiError> {
    let oid = conv.id.unwrap_or_default();
    let opens = state
        .db
        .get_conversation_opens(caller_id, &[oid])
        .await
        .map_err(ApiError::DatabaseError)?;
    let last_opened = opens.get(&oid).copied();
    let workspace_name = workspace::resolve_workspace_name(state, &conv.business_phone).await;
    let resolved = resolve_customer_name(state, &conv).await;
    let agent_name = resolve_last_message_agent_name_one(state, &conv).await;
    let assigned_name = resolve_assigned_agent_name_one(state, &conv).await;

    Ok(response::conv_to_item(
        conv,
        true,
        last_opened,
        workspace_name,
        resolved,
        agent_name,
        assigned_name,
    ))
}
