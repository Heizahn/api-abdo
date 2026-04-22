//! Validación central de payloads de quick-reply (create/update).
//!
//! Se corre antes de tocar la base de datos. Todos los errores se devuelven
//! como `ApiError::ValidationError { field, message }` → HTTP 422 con
//! `{ ok:false, error:"validation_error", field, message }`.
//!
//! El objetivo es que tanto `create` como `update` validen lo mismo sin
//! duplicar código: el handler arma un `ValidatedQuickReply` con los campos
//! finales (mezclando doc existente + patch) y lo pasa acá.

use std::collections::HashSet;

use crate::error::ApiError;
use crate::models::whatsapp::{
    QuickReplyButton, QuickReplyCtaUrl, QuickReplyHeader, QuickReplyList,
};

/// Snapshot de los campos relevantes a validar. El handler lo construye
/// mezclando el doc existente con el patch antes de persistir.
pub struct ValidatedQuickReply<'a> {
    pub title: &'a str,
    pub content: &'a str,
    pub workspace_ids_len: usize,
    pub header: Option<&'a QuickReplyHeader>,
    pub footer: Option<&'a str>,
    pub buttons: Option<&'a [QuickReplyButton]>,
    pub list: Option<&'a QuickReplyList>,
    pub cta_url: Option<&'a QuickReplyCtaUrl>,
}

fn err(field: &str, message: &str) -> ApiError {
    ApiError::ValidationError {
        field: field.to_string(),
        message: message.to_string(),
    }
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

pub fn validate_quick_reply(qr: &ValidatedQuickReply<'_>) -> Result<(), ApiError> {
    // --- title ---
    let title_len = qr.title.chars().count();
    if title_len < 1 || title_len > 100 {
        return Err(err("title", "El título debe tener entre 1 y 100 caracteres"));
    }

    // --- content ---
    let content_len = qr.content.chars().count();
    if content_len < 1 || content_len > 1024 {
        return Err(err("content", "El contenido debe tener entre 1 y 1024 caracteres"));
    }

    // --- workspace_ids ---
    if qr.workspace_ids_len == 0 {
        return Err(err("workspace_ids", "Debe seleccionar al menos un workspace"));
    }

    // --- footer ---
    if let Some(f) = qr.footer {
        if f.chars().count() > 60 {
            return Err(err("footer", "El footer no puede superar 60 caracteres"));
        }
    }

    // --- header ---
    if let Some(h) = qr.header {
        match h {
            QuickReplyHeader::Text { text } => {
                let len = text.chars().count();
                if len < 1 || len > 60 {
                    return Err(err("header.text", "El texto del header debe tener entre 1 y 60 caracteres"));
                }
            }
            QuickReplyHeader::Image { link } => {
                if !is_http_url(link) {
                    return Err(err("header.link", "El link del header debe ser una URL http(s)"));
                }
            }
            QuickReplyHeader::Video { link } => {
                if !is_http_url(link) {
                    return Err(err("header.link", "El link del header debe ser una URL http(s)"));
                }
            }
            QuickReplyHeader::Document { link, filename } => {
                if !is_http_url(link) {
                    return Err(err("header.link", "El link del header debe ser una URL http(s)"));
                }
                if let Some(name) = filename {
                    if name.chars().count() > 255 {
                        return Err(err("header.filename", "El nombre de archivo no puede superar 255 caracteres"));
                    }
                }
            }
        }
    }

    // --- mutual exclusivity: buttons / list / cta_url ---
    let interactive_count = [qr.buttons.is_some(), qr.list.is_some(), qr.cta_url.is_some()]
        .iter()
        .filter(|x| **x)
        .count();
    if interactive_count > 1 {
        return Err(err(
            "interactive",
            "Solo uno de 'buttons', 'list' o 'cta_url' puede estar presente",
        ));
    }

    // --- buttons ---
    if let Some(btns) = qr.buttons {
        if btns.is_empty() || btns.len() > 3 {
            return Err(err("buttons", "Debe haber entre 1 y 3 botones"));
        }
        let mut seen = HashSet::new();
        for (i, b) in btns.iter().enumerate() {
            let id_len = b.id.chars().count();
            if id_len < 1 || id_len > 256 {
                return Err(err(
                    &format!("buttons[{}].id", i),
                    "El id del botón debe tener entre 1 y 256 caracteres",
                ));
            }
            let title_len = b.title.chars().count();
            if title_len < 1 || title_len > 20 {
                return Err(err(
                    &format!("buttons[{}].title", i),
                    "El título del botón debe tener entre 1 y 20 caracteres",
                ));
            }
            if !seen.insert(b.id.clone()) {
                return Err(err(
                    &format!("buttons[{}].id", i),
                    "Los ids de los botones deben ser únicos",
                ));
            }
        }
    }

    // --- list ---
    if let Some(l) = qr.list {
        let btn_len = l.button.chars().count();
        if btn_len < 1 || btn_len > 20 {
            return Err(err("list.button", "El botón de la lista debe tener entre 1 y 20 caracteres"));
        }
        if l.sections.is_empty() || l.sections.len() > 10 {
            return Err(err("list.sections", "Debe haber entre 1 y 10 secciones"));
        }
        let mut total_rows = 0usize;
        let mut seen_row_ids = HashSet::new();
        for (si, section) in l.sections.iter().enumerate() {
            let t_len = section.title.chars().count();
            if t_len < 1 || t_len > 24 {
                return Err(err(
                    &format!("list.sections[{}].title", si),
                    "El título de la sección debe tener entre 1 y 24 caracteres",
                ));
            }
            if section.rows.is_empty() {
                return Err(err(
                    &format!("list.sections[{}].rows", si),
                    "Cada sección debe tener al menos una fila",
                ));
            }
            for (ri, row) in section.rows.iter().enumerate() {
                total_rows += 1;
                let id_len = row.id.chars().count();
                if id_len < 1 || id_len > 200 {
                    return Err(err(
                        &format!("list.sections[{}].rows[{}].id", si, ri),
                        "El id de la fila debe tener entre 1 y 200 caracteres",
                    ));
                }
                let rt_len = row.title.chars().count();
                if rt_len < 1 || rt_len > 24 {
                    return Err(err(
                        &format!("list.sections[{}].rows[{}].title", si, ri),
                        "El título de la fila debe tener entre 1 y 24 caracteres",
                    ));
                }
                if let Some(desc) = &row.description {
                    if desc.chars().count() > 72 {
                        return Err(err(
                            &format!("list.sections[{}].rows[{}].description", si, ri),
                            "La descripción no puede superar 72 caracteres",
                        ));
                    }
                }
                if !seen_row_ids.insert(row.id.clone()) {
                    return Err(err(
                        &format!("list.sections[{}].rows[{}].id", si, ri),
                        "Los ids de las filas deben ser únicos en toda la lista",
                    ));
                }
            }
        }
        if total_rows > 10 {
            return Err(err("list", "La lista no puede tener más de 10 filas en total"));
        }
    }

    // --- cta_url ---
    if let Some(c) = qr.cta_url {
        let dt_len = c.display_text.chars().count();
        if dt_len < 1 || dt_len > 20 {
            return Err(err("cta_url.display_text", "El texto del botón debe tener entre 1 y 20 caracteres"));
        }
        if !is_http_url(&c.url) {
            return Err(err("cta_url.url", "La URL del CTA debe ser http(s)"));
        }
    }

    Ok(())
}
