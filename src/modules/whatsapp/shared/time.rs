use mongodb::bson::DateTime;

pub(crate) fn iso8601(dt: DateTime) -> String {
    dt.try_to_rfc3339_string().unwrap_or_default()
}

/// Ventana de 24h desde `last_inbound_at`. Usado por el gate de envío freeform,
/// por `conv_to_item` y por el WS event `CONVERSACION_ESTADO`.
pub(crate) fn is_within_24h(last_inbound_at: Option<DateTime>) -> bool {
    match last_inbound_at {
        Some(t) => {
            let now = DateTime::now().timestamp_millis();
            let then = t.timestamp_millis();
            (now - then) <= 24 * 60 * 60 * 1000
        }
        None => false,
    }
}

/// Devuelve `(can_send_freeform, freeform_expires_at_iso)`.
pub(crate) fn compute_freeform_state(last_inbound_at: Option<DateTime>) -> (bool, Option<String>) {
    match last_inbound_at {
        Some(t) => {
            let expires = DateTime::from_millis(t.timestamp_millis() + 24 * 60 * 60 * 1000);
            (is_within_24h(Some(t)), Some(iso8601(expires)))
        }
        None => (false, None),
    }
}
