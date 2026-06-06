use crate::models::whatsapp::{StatusError, WebhookValue};

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
