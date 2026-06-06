use crate::error::ApiError;

/// Valida un access_token de Meta. Un token legítimo es un string continuo
/// base64url-ish sin espacios ni comillas. Cualquier carácter extraño suele
/// indicar copy-paste con varias variables (ej: pegar una línea de `.env`).
pub(crate) fn validate_access_token(raw: &str) -> Result<&str, ApiError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(ApiError::BadRequest("access_token requerido".into()));
    }
    if t.chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    {
        return Err(ApiError::BadRequest(
            "access_token inválido: contiene espacios o comillas".into(),
        ));
    }
    Ok(t)
}

/// Normaliza cualquier formato de número venezolano a E.164 sin "+" (ej: "584141234567")
pub(crate) fn normalize_to_e164(phone: &str) -> String {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.starts_with("58") {
        digits
    } else if let Some(rest) = digits.strip_prefix('0') {
        format!("58{}", rest)
    } else {
        format!("58{}", digits)
    }
}
