use axum::http::StatusCode;

use crate::{
    error::ApiError,
    models::whatsapp::{
        WaTemplate, WaTemplateButtonInput, WaTemplateCategory, WaTemplateHeaderInput,
        WaTemplateItem,
    },
};

use super::super::shared::time::iso8601;

fn count_placeholders(text: &str) -> u32 {
    let bytes = text.as_bytes();
    let mut max_idx: u32 = 0;
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let mut j = i + 2;
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start && j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                if let Ok(n) = std::str::from_utf8(&bytes[start..j])
                    .unwrap_or("")
                    .parse::<u32>()
                {
                    if n > max_idx {
                        max_idx = n;
                    }
                }
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    max_idx
}

fn slugify(s: &str) -> String {
    let ascii_only: String = s.chars().filter(|c| c.is_ascii()).collect();
    let lower = ascii_only.to_lowercase();
    let replaced: String = lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();

    let mut collapsed = String::with_capacity(replaced.len());
    let mut prev_underscore = false;
    for c in replaced.chars() {
        if c == '_' {
            if !prev_underscore {
                collapsed.push(c);
            }
            prev_underscore = true;
        } else {
            collapsed.push(c);
            prev_underscore = false;
        }
    }

    let trimmed = collapsed.trim_end_matches('_');
    if trimmed.len() > 512 {
        &trimmed[..512]
    } else {
        trimmed
    }
    .to_string()
}

pub(in crate::modules::whatsapp) fn generate_template_name(
    name_input: &str,
    is_system: bool,
) -> String {
    let slug = slugify(name_input);
    if is_system {
        let today = chrono::Utc::now().format("%Y%m%d").to_string();
        format!("sistema_abdo_{}_{}", slug, today)
    } else {
        slug
    }
}

pub(in crate::modules::whatsapp) fn flat_to_components(
    header: Option<&WaTemplateHeaderInput>,
    body: &str,
    body_samples: Option<&Vec<String>>,
    footer: Option<&str>,
    buttons: Option<&Vec<WaTemplateButtonInput>>,
) -> Vec<serde_json::Value> {
    let mut comps: Vec<serde_json::Value> = Vec::new();

    if let Some(h) = header {
        let mut comp = serde_json::json!({
            "type": "HEADER",
            "format": h.kind.to_uppercase(),
        });
        if let Some(t) = &h.text {
            comp["text"] = serde_json::json!(t);
        }
        if let Some(ex) = &h.example {
            comp["example"] = ex.clone();
        }
        comps.push(comp);
    }

    let mut body_comp = serde_json::json!({ "type": "BODY", "text": body });
    if let Some(samples) = body_samples {
        if !samples.is_empty() {
            body_comp["example"] = serde_json::json!({ "body_text": [samples] });
        }
    }
    comps.push(body_comp);

    if let Some(f) = footer {
        if !f.trim().is_empty() {
            comps.push(serde_json::json!({ "type": "FOOTER", "text": f }));
        }
    }

    if let Some(btns) = buttons {
        if !btns.is_empty() {
            let mut button_arr: Vec<serde_json::Value> = Vec::new();
            for b in btns {
                let mut bobj = serde_json::json!({
                    "type": b.kind.to_uppercase(),
                    "text": b.text,
                });
                if let Some(u) = &b.url {
                    bobj["url"] = serde_json::json!(u);
                }
                if let Some(p) = &b.phone_number {
                    bobj["phone_number"] = serde_json::json!(p);
                }
                if let Some(ex) = &b.example {
                    bobj["example"] = serde_json::json!(ex);
                }
                button_arr.push(bobj);
            }
            comps.push(serde_json::json!({ "type": "BUTTONS", "buttons": button_arr }));
        }
    }

    comps
}

pub(in crate::modules::whatsapp) fn validate_components(
    comps: &[serde_json::Value],
) -> Result<u32, ApiError> {
    let has_body = comps.iter().any(|c| {
        c.get("type")
            .and_then(|v| v.as_str())
            .map(|t| t.eq_ignore_ascii_case("BODY"))
            .unwrap_or(false)
    });
    if !has_body {
        return Err(ApiError::domain_with_details(
            StatusCode::BAD_REQUEST,
            "invalid_component",
            "Se requiere componente BODY",
            serde_json::json!({ "component_index": null, "reason": "body_required" }),
        ));
    }

    let mut body_placeholders: u32 = 0;

    for (idx, comp) in comps.iter().enumerate() {
        let comp_type = comp
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();

        match comp_type.as_str() {
            "BODY" => {
                let text = comp.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "BODY.text no puede estar vacío",
                        serde_json::json!({ "component_index": idx, "reason": "body_text_required" }),
                    ));
                }
                if text.len() > 1024 {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "BODY.text excede 1024 caracteres",
                        serde_json::json!({ "component_index": idx, "reason": "body_text_too_long" }),
                    ));
                }
                body_placeholders = count_placeholders(text);
            }
            "FOOTER" => {
                if let Some(text) = comp.get("text").and_then(|v| v.as_str()) {
                    if text.len() > 60 {
                        return Err(ApiError::domain_with_details(
                            StatusCode::BAD_REQUEST,
                            "invalid_component",
                            "FOOTER.text excede 60 caracteres",
                            serde_json::json!({ "component_index": idx, "reason": "footer_text_too_long" }),
                        ));
                    }
                }
            }
            "HEADER" => {
                let format = comp
                    .get("format")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_uppercase();
                let valid_formats = ["NONE", "TEXT", "IMAGE", "VIDEO", "DOCUMENT"];
                if !valid_formats.contains(&format.as_str()) {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        format!("HEADER.format inválido: {}", format),
                        serde_json::json!({ "component_index": idx, "reason": "header_format_invalid" }),
                    ));
                }
                if format == "TEXT" {
                    if let Some(text) = comp.get("text").and_then(|v| v.as_str()) {
                        if text.len() > 60 {
                            return Err(ApiError::domain_with_details(
                                StatusCode::BAD_REQUEST,
                                "invalid_component",
                                "HEADER.text excede 60 caracteres",
                                serde_json::json!({ "component_index": idx, "reason": "header_text_too_long" }),
                            ));
                        }
                    }
                }
            }
            "BUTTONS" => {
                let buttons = match comp.get("buttons").and_then(|v| v.as_array()) {
                    Some(b) => b,
                    None => continue,
                };
                let types: Vec<String> = buttons
                    .iter()
                    .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
                    .map(|s| s.to_uppercase())
                    .collect();

                let all_qr = types.iter().all(|t| t == "QUICK_REPLY");
                let all_url = types.iter().all(|t| t == "URL");
                let all_phone = types.iter().all(|t| t == "PHONE_NUMBER");

                if !all_qr && !all_url && !all_phone {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "No se pueden mezclar tipos de botones",
                        serde_json::json!({ "component_index": idx, "reason": "buttons_mixed_types" }),
                    ));
                }
                if all_qr && buttons.len() > 3 {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "Máximo 3 botones QUICK_REPLY",
                        serde_json::json!({ "component_index": idx, "reason": "buttons_too_many" }),
                    ));
                }
                if (all_url || all_phone) && buttons.len() > 1 {
                    return Err(ApiError::domain_with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_component",
                        "Máximo 1 botón de tipo URL o PHONE_NUMBER",
                        serde_json::json!({ "component_index": idx, "reason": "buttons_too_many" }),
                    ));
                }
            }
            _ => {}
        }
    }

    Ok(body_placeholders)
}

pub(in crate::modules::whatsapp) fn to_template_item(t: WaTemplate) -> WaTemplateItem {
    WaTemplateItem {
        id: t.id.map(|o| o.to_hex()).unwrap_or_default(),
        phone_number_id: t.phone_number_id,
        name: t.name,
        display_name: t.display_name,
        name_input: t.name_input,
        language: t.language,
        category: t.category,
        components: t.components,
        body_placeholders: t.body_placeholders,
        status: t.status,
        rejection_reason: t.rejection_reason,
        meta_template_id: t.meta_template_id,
        is_system: t.is_system,
        submit_to_meta: t.submit_to_meta,
        created_by: t.created_by,
        created_by_name: t.created_by_name,
        created_at: iso8601(t.created_at),
        updated_at: iso8601(t.updated_at),
    }
}

pub(in crate::modules::whatsapp) fn template_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "template_not_found",
        "Plantilla no encontrada",
    )
}

pub(in crate::modules::whatsapp) fn parse_meta_template_category(
    raw: Option<&str>,
) -> Option<WaTemplateCategory> {
    match raw?.trim().to_uppercase().as_str() {
        "MARKETING" => Some(WaTemplateCategory::Marketing),
        "UTILITY" | "SERVICE" => Some(WaTemplateCategory::Utility),
        "AUTHENTICATION" => Some(WaTemplateCategory::Authentication),
        _ => None,
    }
}

pub(crate) fn map_meta_error(err: &anyhow::Error, default_msg: &str) -> ApiError {
    use super::super::service::MetaApiError;

    if let Some(me) = err.downcast_ref::<MetaApiError>() {
        if me.code == 429 {
            return ApiError::domain_with_details(
                StatusCode::TOO_MANY_REQUESTS,
                "meta_edit_rate_limited",
                "Meta limita las ediciones a 1 por día y 10 por mes. Intenta más tarde",
                serde_json::json!({}),
            );
        }
        let user_msg = me.error_user_msg.clone();
        return ApiError::domain_with_details(
            StatusCode::BAD_GATEWAY,
            "meta_rejected",
            default_msg,
            serde_json::json!({
                "meta_error_code": me.code.to_string(),
                "meta_error_message": me.message,
                "rejection_reason": user_msg,
            }),
        );
    }

    ApiError::domain_with_details(
        StatusCode::BAD_GATEWAY,
        "meta_rejected",
        default_msg,
        serde_json::json!({
            "meta_error_code": "0",
            "meta_error_message": err.to_string(),
            "rejection_reason": null,
        }),
    )
}
