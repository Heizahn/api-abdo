// Extrae {"phone":"..."} de un JSON pequeño sin usar serde.
// Asume comillas dobles y sin caracteres escapados complejos (solo para demo).
pub fn parse_login_body(json: &str) -> Option<String> {
    // Busca "phone":"<valor>"
    let key = r#""phone""#;
    let idx = json.find(key)?;
    let after = &json[idx + key.len()..];
    let pos_colon = after.find(':')?;
    let after_colon = after[pos_colon + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let after_quote = &after_colon[1..];
    let end = after_quote.find('"')?;
    Some(after_quote[..end].to_string())
}

pub fn login_response_exists(id: &str, name: &str, phone: &str) -> String {
    format!(
        r#"{{"ok":true,"exists":true,"customer":{{"id":"{}","name":"{}","phone":"{}"}}}}"#,
        id, name, phone
    )
}

pub fn login_response_not_found(phone: &str) -> String {
    format!(r#"{{"ok":true,"exists":false,"phone":"{}"}}"#, phone)
}

pub fn bad_request(msg: &str) -> String {
    format!(r#"{{"ok":false,"error":"{}"}}"#, msg)
}
