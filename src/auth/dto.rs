use serde::Deserialize;

pub fn parse_login_body(json: &str) -> Option<String> {
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

pub fn login_response_not_found(phone: &str) -> String {
    format!(r#"{{"ok":true,"exists":false,"phone":"{}"}}"#, phone)
}

pub fn bad_request(msg: &str) -> String {
    format!(r#"{{"ok":false,"error":"{}"}}"#, msg)
}

// ✅ versión definitiva solo snake_case
#[derive(Deserialize)]
struct RefreshBody {
    refresh_token: String,
}

pub fn parse_refresh_body(json: &str) -> Option<String> {
    serde_json::from_str::<RefreshBody>(json)
        .ok()
        .map(|b| b.refresh_token)
}
