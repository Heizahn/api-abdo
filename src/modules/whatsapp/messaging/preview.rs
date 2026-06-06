use crate::models::whatsapp::SendTemplatePayload;

pub(crate) fn interactive_preview(payload: &serde_json::Value) -> String {
    // Preferimos el texto del body, luego del header, luego un fallback.
    if let Some(b) = payload
        .get("body")
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
    {
        let t = b.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(h) = payload
        .get("header")
        .and_then(|h| h.get("text"))
        .and_then(|t| t.as_str())
    {
        let t = h.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "[mensaje interactivo]".to_string()
}

pub(crate) fn template_preview(tpl: &SendTemplatePayload) -> String {
    if let Some(rendered) = tpl.rendered_text.as_deref() {
        let t = rendered.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    format!("[plantilla: {}]", tpl.name)
}
