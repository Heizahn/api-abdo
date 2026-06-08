use mongodb::bson::{oid::ObjectId, DateTime};

use crate::models::whatsapp::{
    ConversationItem, QuickReplyItem, SettingsItem, WaConversation, WaQuickReply, WaSettings,
};

use super::time::{compute_freeform_state, iso8601};

pub(crate) fn compute_meta_throttle_state(until: Option<DateTime>) -> (bool, Option<String>) {
    match until {
        Some(t) => {
            let now_ms = DateTime::now().timestamp_millis();
            if t.timestamp_millis() > now_ms {
                (true, Some(iso8601(t)))
            } else {
                (false, None)
            }
        }
        None => (false, None),
    }
}

pub(crate) fn conv_to_item(
    c: WaConversation,
    include_client_id: bool,
    last_opened_at: Option<DateTime>,
    workspace_name: Option<String>,
    resolved_name: Option<String>,
    last_message_from_user_name: Option<String>,
    assigned_to_name: Option<String>,
) -> ConversationItem {
    let (can_send_freeform, expires_iso) = compute_freeform_state(c.last_inbound_at);
    let (meta_throttled, meta_throttle_until_iso) =
        compute_meta_throttle_state(c.meta_throttle_until);

    // Prioridad: DB (Clients.sName) → WhatsApp profile (c.name) → null
    let customer_name = resolved_name.filter(|s| !s.trim().is_empty()).or(c.name);

    ConversationItem {
        id: c.id.map(|o| o.to_hex()).unwrap_or_default(),
        customer_phone: c.phone,
        customer_name,
        business_phone: c.business_phone,
        workspace_name,
        status: c.status,
        assigned_to: c.assigned_to,
        assigned_to_name,
        last_message_at: iso8601(c.last_message_at),
        last_message_preview: c.last_message_preview,
        last_message_type: c.last_message_type,
        last_message_direction: c.last_message_direction,
        last_message_status: c.last_message_status,
        last_message_media_filename: c.last_message_media_filename,
        last_message_from_user_id: c.last_message_from_user_id,
        last_message_from_user_name,
        unread_count: c.unread_count,
        created_at: iso8601(c.created_at),
        client_id: if include_client_id {
            c.client_id.map(|o| o.to_hex())
        } else {
            None
        },
        last_opened_at: last_opened_at.map(iso8601),
        last_inbound_at: c.last_inbound_at.map(iso8601),
        can_send_freeform,
        freeform_expires_at: expires_iso,
        meta_throttled,
        meta_throttle_until: meta_throttle_until_iso,
        ai_active_agent_id: c.ai_active_agent_id.map(|o| o.to_hex()),
        ai_disabled: c.ai_disabled,
        ai_last_processed_at: c.ai_last_processed_at.map(iso8601),
        ai_conv_state: c.ai_conv_state,
    }
}

pub(crate) fn settings_to_item(s: WaSettings) -> SettingsItem {
    SettingsItem {
        id: s.id.map(|o| o.to_hex()).unwrap_or_default(),
        phone: s.phone,
        workspace_name: s.workspace_name,
        phone_number_id: s.phone_number_id,
        whatsapp_business_account_id: s.whatsapp_business_account_id,
        has_access_token: !s.access_token.is_empty(),
        agents: s.agents,
        active: s.active,
        purposes: s.purposes,
        enable_guardrails: s.enable_guardrails,
        enable_conversation_state: s.enable_conversation_state,
        pre_classifier_enabled: s.pre_classifier_enabled,
        trivial_responses: s.trivial_responses,
        templates_synced_at: s.templates_synced_at.map(iso8601),
        created_at: iso8601(s.created_at),
        updated_at: iso8601(s.updated_at),
    }
}

pub(crate) fn quick_reply_to_item(
    q: WaQuickReply,
    caller: &crate::models::users::User,
    caller_workspaces: &[ObjectId],
) -> QuickReplyItem {
    let can_edit = compute_can_edit(caller.role, caller_workspaces, &q.workspace_ids);
    QuickReplyItem {
        id: q.id.map(|o| o.to_hex()).unwrap_or_default(),
        title: q.title,
        content: q.content,
        workspace_ids: q.workspace_ids.into_iter().map(|o| o.to_hex()).collect(),
        created_by: q.created_by,
        created_by_name: q.created_by_name,
        created_at: iso8601(q.created_at),
        updated_at: iso8601(q.updated_at),
        active: q.active,
        can_edit,
        header: q.header,
        footer: q.footer,
        buttons: q.buttons,
        list: q.list,
        cta_url: q.cta_url,
        use_count: q.use_count,
        last_used_at: q.last_used_at.map(iso8601),
    }
}

fn compute_can_edit(
    caller_role: f32,
    caller_workspaces: &[ObjectId],
    qr_workspace_ids: &[ObjectId],
) -> bool {
    if caller_role == 0.0 {
        return true;
    }
    qr_workspace_ids
        .iter()
        .any(|w| caller_workspaces.contains(w))
}
