use std::sync::Arc;

use crate::{
    db::WaTemplateRepository,
    models::whatsapp::{StatusError, WaTemplateStatus, WebhookValue},
    state::AppState,
};

use crate::modules::whatsapp::{
    templates::meta::to_template_item,
    ws::{build_template_updated_event, emit_to_phone_number_agents},
};

#[derive(Debug, Clone)]
pub(crate) struct InboundMediaFailureDetails {
    pub(crate) code: Option<i64>,
    pub(crate) title: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) error_data: Option<serde_json::Value>,
}

impl InboundMediaFailureDetails {
    pub(crate) fn from_status_error(err: &StatusError) -> Self {
        Self {
            code: err.code,
            title: err.title.clone(),
            message: err.message.clone(),
            error_data: err.error_data.clone(),
        }
    }
}

pub(crate) fn log_webhook_top_level_errors(value: &WebhookValue) -> usize {
    match &value.errors {
        Some(errors) => {
            if errors.is_empty() {
                tracing::warn!("[webhook] payload errors recibido sin detalles en change.value");
                0
            } else {
                for err in errors {
                    tracing::warn!(
                        "[webhook] top-level error: code={:?} title={:?} message={:?}",
                        err.code,
                        err.title,
                        err.message
                    );
                }
                errors.len()
            }
        }
        None => {
            tracing::warn!("[webhook] payload errors recibido sin detalles en change.value");
            0
        }
    }
}

pub(crate) fn is_inbound_media_failure_status(
    status: &str,
    errors: Option<&[StatusError]>,
) -> bool {
    status == "failed"
        && errors.is_some_and(|errs| {
            errs.iter()
                .any(|e| matches!(e.code, Some(131052) | Some(131053) | Some(131056)))
        })
}

pub(crate) fn has_meta_throttle_131049(errors: Option<&[StatusError]>) -> bool {
    errors.is_some_and(|errs| errs.iter().any(|e| e.code == Some(131049)))
}

/// Procesa un evento `message_template_status_update` del webhook de Meta.
/// Mapea el `event` a `WaTemplateStatus`, actualiza en DB, emite WS.
/// Siempre retorna sin error — el webhook debe devolver 200.
pub(crate) async fn process_template_status(
    state: &Arc<AppState>,
    meta_template_id: &str,
    event: &str,
    reason: Option<&str>,
) {
    let (new_status, rejection_reason): (WaTemplateStatus, Option<String>) =
        match event.to_uppercase().as_str() {
            "APPROVED" => (WaTemplateStatus::Approved, None),
            "REJECTED" => (WaTemplateStatus::Rejected, reason.map(|s| s.to_string())),
            "FLAGGED" => (
                WaTemplateStatus::Rejected,
                Some("flagged_by_meta_quality".to_string()),
            ),
            "PAUSED" => (WaTemplateStatus::Paused, reason.map(|s| s.to_string())),
            "DISABLED" => (WaTemplateStatus::Disabled, reason.map(|s| s.to_string())),
            "PENDING" | "IN_REVIEW" => (WaTemplateStatus::Pending, None),
            other => {
                tracing::warn!(
                    "[webhook] process_template_status: evento desconocido '{}' para meta_id={}",
                    other,
                    meta_template_id
                );
                return;
            }
        };

    match state
        .db
        .update_template_status(meta_template_id, new_status, rejection_reason)
        .await
    {
        Ok(None) => {
            tracing::warn!(
                "[webhook] process_template_status: template con meta_id={} no encontrado en DB",
                meta_template_id
            );
        }
        Ok(Some((updated_doc, prev_status))) => {
            if prev_status != new_status {
                let item = to_template_item(updated_doc.clone());
                let ws_payload = build_template_updated_event(&item, Some(prev_status));
                emit_to_phone_number_agents(state, &updated_doc.phone_number_id, ws_payload).await;
            }
        }
        Err(e) => {
            tracing::error!(
                "[webhook] process_template_status: DB error para meta_id={}: {}",
                meta_template_id,
                e
            );
        }
    }
}
