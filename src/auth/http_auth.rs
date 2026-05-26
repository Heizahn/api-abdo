use axum::http::HeaderMap;
use chrono::{NaiveDate, Utc};

use crate::config::Config;

pub const STAFF_ACCESS_COOKIE: &str = "abdo_staff_at";
pub const STAFF_REFRESH_COOKIE: &str = "abdo_staff_rt";
pub const STAFF_REDIS_REALM: &str = "staff";

pub fn compat_refresh_body_enabled(cfg: &Config) -> bool {
    cfg.auth_compat_allow_refresh_body && compat_window_is_open(cfg)
}

pub fn compat_ws_query_enabled(cfg: &Config) -> bool {
    cfg.auth_compat_allow_ws_query && compat_window_is_open(cfg)
}

pub fn compat_window_is_open(cfg: &Config) -> bool {
    let Some(until_raw) = cfg.auth_compat_until.as_deref() else {
        return true;
    };

    let until = match NaiveDate::parse_from_str(until_raw, "%Y-%m-%d") {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                "AUTH_COMPAT_UNTIL inválido ('{}'): {}. Se desactiva compatibilidad temporal.",
                until_raw,
                err
            );
            return false;
        }
    };

    Utc::now().date_naive() <= until
}

pub fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookie_header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for chunk in cookie_header.split(';') {
        let mut parts = chunk.trim().splitn(2, '=');
        let key = parts.next()?.trim();
        let value = parts.next()?.trim();
        if key == name && !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

pub fn read_bearer(headers: &HeaderMap) -> Option<String> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .trim();
    let (scheme, token) = auth.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

pub fn read_staff_access_token(headers: &HeaderMap) -> Option<String> {
    read_cookie(headers, STAFF_ACCESS_COOKIE).or_else(|| read_bearer(headers))
}

#[derive(Debug, Clone)]
pub struct AuthInputDebug {
    pub has_authorization_header: bool,
    pub has_cookie_header: bool,
    pub has_access_cookie: bool,
    pub has_bearer_token: bool,
}

pub fn auth_input_debug(headers: &HeaderMap, access_cookie_name: &str) -> AuthInputDebug {
    AuthInputDebug {
        has_authorization_header: headers.contains_key(axum::http::header::AUTHORIZATION),
        has_cookie_header: headers.contains_key(axum::http::header::COOKIE),
        has_access_cookie: read_cookie(headers, access_cookie_name).is_some(),
        has_bearer_token: read_bearer(headers).is_some(),
    }
}

pub fn read_staff_refresh_token(
    headers: &HeaderMap,
    cfg: &Config,
    body_fallback: Option<&str>,
) -> Option<String> {
    if let Some(token) = read_cookie(headers, STAFF_REFRESH_COOKIE) {
        return Some(token);
    }

    if compat_refresh_body_enabled(cfg) {
        if let Some(token) = body_fallback.map(str::trim).filter(|v| !v.is_empty()) {
            return Some(token.to_string());
        }
    }

    None
}

pub fn build_auth_cookie(
    cfg: &Config,
    name: &str,
    value: &str,
    max_age_secs: i64,
    path: &str,
) -> String {
    let same_site = normalize_same_site(&cfg.auth_cookie_same_site);
    let mut cookie = format!(
        "{}={}; Path={}; Max-Age={}; HttpOnly; SameSite={}",
        name, value, path, max_age_secs, same_site
    );

    if cfg.auth_cookie_secure {
        cookie.push_str("; Secure");
    }
    if let Some(domain) = cfg.auth_cookie_domain.as_deref() {
        cookie.push_str("; Domain=");
        cookie.push_str(domain);
    }

    cookie
}

pub fn build_clear_cookie(cfg: &Config, name: &str, path: &str) -> String {
    let same_site = normalize_same_site(&cfg.auth_cookie_same_site);
    let mut cookie = format!(
        "{}=; Path={}; Max-Age=0; HttpOnly; SameSite={}",
        name, path, same_site
    );
    if cfg.auth_cookie_secure {
        cookie.push_str("; Secure");
    }
    if let Some(domain) = cfg.auth_cookie_domain.as_deref() {
        cookie.push_str("; Domain=");
        cookie.push_str(domain);
    }
    cookie
}

fn normalize_same_site(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "strict" => "Strict",
        "none" => "None",
        _ => "Lax",
    }
}
