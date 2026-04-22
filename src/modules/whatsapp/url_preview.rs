//! Fetch de preview de URLs (OG / Twitter Card) que aparecen en el cuerpo de
//! los mensajes de WhatsApp.
//!
//! Se corre en background (via `tokio::spawn`) después de persistir el mensaje.
//! Al terminar, persiste el `UrlPreview` en el doc del mensaje y emite
//! `URL_PREVIEW_READY` por WS con el `MessageItem` actualizado.
//!
//! ## Seguridad (SSRF)
//!
//! - Sólo `http`/`https`.
//! - Resolución DNS manual por cada hop (inicial + cada redirect): si resuelve
//!   a IP privada / loopback / link-local / ULA / multicast, se rechaza.
//! - Redirects manuales hasta 3 hops.
//! - Body limit 2 MB (cortamos el stream).
//! - Timeout total 3 s (timeout de reqwest).
//! - Cliente dedicado forzado a IPv4 (misma razón que el cliente principal).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use regex::Regex;
use reqwest::Url;
use scraper::{Html, Selector};

use crate::db::WhatsAppRepository;
use crate::models::whatsapp::UrlPreview;
use crate::state::AppState;

const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_REDIRECTS: u8 = 3;
const MAX_TITLE_CHARS: usize = 150;
const MAX_DESC_CHARS: usize = 300;

// ============================================
// API PÚBLICA
// ============================================

/// Dispara el fetch de preview en background. No bloquea al caller.
///
/// Si `text` no trae URL, el job termina sin tocar nada. Si lo trae y hay
/// preview, se persiste en DB y se emite `URL_PREVIEW_READY` por WS.
pub fn spawn_preview_job(
    state: Arc<AppState>,
    msg_oid: mongodb::bson::oid::ObjectId,
    conv_oid: mongodb::bson::oid::ObjectId,
    text: String,
) {
    let url = match extract_first_url(&text) {
        Some(u) => u,
        None => return,
    };

    tokio::spawn(async move {
        let preview = match resolve_preview(&state, &url).await {
            Some(p) => p,
            None => return,
        };

        match state.db.set_message_url_preview(&msg_oid, &preview).await {
            Ok(Some(updated)) => {
                let item = super::handler::build_message_item(&state, updated).await;
                let ev = super::ws::WsServerEvent::UrlPreviewReady {
                    conversation_id: conv_oid.to_hex(),
                    message: item,
                };
                super::ws::broadcast_all(&state.ws_registry, &ev).await;
            }
            Ok(None) => {
                tracing::debug!(
                    "[url_preview] mensaje {} desapareció antes de persistir preview",
                    msg_oid.to_hex()
                );
            }
            Err(e) => {
                tracing::warn!("[url_preview] set_message_url_preview error: {}", e);
            }
        }
    });
}

/// Extrae la primera URL http/https del texto. Limpia puntuación final que
/// típicamente pertenece a la oración y no a la URL (".", ",", ")", etc).
pub fn extract_first_url(text: &str) -> Option<String> {
    let re = url_regex();
    let m = re.find(text)?;
    let raw = m.as_str();
    let trimmed = raw.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''));
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ============================================
// RESOLUCIÓN: cache → fetch → cache
// ============================================

async fn resolve_preview(state: &Arc<AppState>, url: &str) -> Option<UrlPreview> {
    // Cache: hit positivo devuelve preview; hit negativo (miss cacheado) devuelve None sin fetchear.
    match state.redis.get_url_preview(url).await {
        Some(Some(v)) => {
            return serde_json::from_value::<UrlPreview>(v).ok();
        }
        Some(None) => {
            tracing::debug!("[url_preview] cache hit negativo para {}", url);
            return None;
        }
        None => {}
    }

    match fetch_preview(url).await {
        Ok(Some(p)) => {
            let v = serde_json::to_value(&p).ok();
            state.redis.set_url_preview(url, v.as_ref()).await;
            Some(p)
        }
        Ok(None) => {
            tracing::debug!("[url_preview] fetch OK sin preview útil: {}", url);
            state.redis.set_url_preview(url, None).await;
            None
        }
        Err(e) => {
            tracing::debug!("[url_preview] fetch falló para {}: {}", url, e);
            state.redis.set_url_preview(url, None).await;
            None
        }
    }
}

// ============================================
// FETCH + SSRF GUARD + REDIRECT MANUAL
// ============================================

async fn fetch_preview(url: &str) -> anyhow::Result<Option<UrlPreview>> {
    let initial = Url::parse(url).map_err(|e| anyhow::anyhow!("URL inválida: {}", e))?;
    if !matches!(initial.scheme(), "http" | "https") {
        return Err(anyhow::anyhow!("scheme no soportado: {}", initial.scheme()));
    }

    let client = preview_client();
    let mut current = initial;
    let mut hops = 0u8;

    loop {
        ensure_public_host(&current).await?;

        let resp = client
            .get(current.clone())
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8",
            )
            .header(reqwest::header::ACCEPT_LANGUAGE, "es,en;q=0.8")
            .send()
            .await?;

        let status = resp.status();
        if status.is_redirection() {
            if hops >= MAX_REDIRECTS {
                return Err(anyhow::anyhow!("demasiados redirects"));
            }
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("redirect sin Location"))?;
            current = current
                .join(loc)
                .map_err(|e| anyhow::anyhow!("Location inválida ({}): {}", loc, e))?;
            hops += 1;
            continue;
        }

        if !status.is_success() {
            return Err(anyhow::anyhow!("HTTP {}", status));
        }

        // Sólo HTML — los demás content types no suelen traer OG tags.
        let ct_lower = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !ct_lower.starts_with("text/html") && !ct_lower.starts_with("application/xhtml") {
            return Err(anyhow::anyhow!("content-type no HTML: {}", ct_lower));
        }

        // Límite de 2 MB leyendo por chunks; cualquier cosa más allá se ignora.
        let body = read_capped_body(resp).await?;
        let html = String::from_utf8_lossy(&body);
        let preview = parse_preview(&html, &current);
        return Ok(Some(preview));
    }
}

/// Lee el body chunk-a-chunk hasta `MAX_BODY_BYTES` y corta. Evita que servers
/// con contenido gigante (o tarpits) agoten memoria.
async fn read_capped_body(mut resp: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::with_capacity(32 * 1024);
    while let Some(chunk) = resp.chunk().await? {
        let remaining = MAX_BODY_BYTES.saturating_sub(buf.len());
        if remaining == 0 {
            break;
        }
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Rechaza la URL si el host resuelve a IP privada / loopback / link-local /
/// ULA / multicast. Chequea cada dirección (un hostname puede resolver a
/// múltiples IPs y basta una privada para rechazar).
async fn ensure_public_host(url: &Url) -> anyhow::Result<()> {
    use std::net::IpAddr;

    let host = url.host_str().ok_or_else(|| anyhow::anyhow!("URL sin host"))?;

    // Host literal (IPv4/IPv6 sin DNS).
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_ip_public(&ip) {
            return Err(anyhow::anyhow!("IP literal no pública: {}", ip));
        }
        return Ok(());
    }

    let port = url.port_or_known_default().unwrap_or(80);
    let hostport = format!("{}:{}", host, port);
    let addrs = tokio::net::lookup_host(hostport.as_str())
        .await
        .map_err(|e| anyhow::anyhow!("DNS lookup falló para {}: {}", host, e))?;

    let mut any = false;
    for sa in addrs {
        any = true;
        if !is_ip_public(&sa.ip()) {
            return Err(anyhow::anyhow!(
                "{} resuelve a IP no pública: {}",
                host,
                sa.ip()
            ));
        }
    }
    if !any {
        return Err(anyhow::anyhow!("DNS sin resultados para {}", host));
    }
    Ok(())
}

/// `true` si la IP es routable en Internet público (no loopback/privada/reservada).
fn is_ip_public(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            if v4.is_loopback() || v4.is_unspecified() || v4.is_broadcast() || v4.is_link_local() || v4.is_multicast() {
                return false;
            }
            // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
            if a == 10 { return false; }
            if a == 172 && (16..32).contains(&b) { return false; }
            if a == 192 && b == 168 { return false; }
            // 100.64.0.0/10 CGNAT
            if a == 100 && (64..128).contains(&b) { return false; }
            // 198.18.0.0/15 benchmarking
            if a == 198 && (b == 18 || b == 19) { return false; }
            // 192.0.0.0/24, 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 documentación
            if a == 192 && b == 0 { return false; }
            if a == 198 && b == 51 { return false; }
            if a == 203 && b == 0 { return false; }
            // 240.0.0.0/4 reservado
            if a >= 240 { return false; }
            true
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            // IPv4-mapped: validar el IPv4 interno.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_ip_public(&IpAddr::V4(mapped));
            }
            let seg0 = v6.segments()[0];
            // fe80::/10 link-local
            if seg0 & 0xffc0 == 0xfe80 { return false; }
            // fc00::/7 ULA
            if seg0 & 0xfe00 == 0xfc00 { return false; }
            // 2001:db8::/32 documentación
            let segs = v6.segments();
            if segs[0] == 0x2001 && segs[1] == 0x0db8 { return false; }
            true
        }
    }
}

// ============================================
// PARSER HTML → UrlPreview
// ============================================

fn parse_preview(html: &str, final_url: &Url) -> UrlPreview {
    let doc = Html::parse_document(html);

    let title = meta_content(&doc, r#"meta[property="og:title"]"#)
        .or_else(|| meta_content(&doc, r#"meta[name="twitter:title"]"#))
        .or_else(|| title_tag(&doc))
        .map(|s| truncate(&s, MAX_TITLE_CHARS));

    let description = meta_content(&doc, r#"meta[property="og:description"]"#)
        .or_else(|| meta_content(&doc, r#"meta[name="twitter:description"]"#))
        .or_else(|| meta_content(&doc, r#"meta[name="description"]"#))
        .map(|s| truncate(&s, MAX_DESC_CHARS));

    let image_url = meta_content(&doc, r#"meta[property="og:image"]"#)
        .or_else(|| meta_content(&doc, r#"meta[property="og:image:url"]"#))
        .or_else(|| meta_content(&doc, r#"meta[property="og:image:secure_url"]"#))
        .or_else(|| meta_content(&doc, r#"meta[name="twitter:image"]"#))
        .or_else(|| meta_content(&doc, r#"meta[name="twitter:image:src"]"#))
        .and_then(|raw| final_url.join(&raw).ok())
        .map(|u| u.to_string());

    let site_name = meta_content(&doc, r#"meta[property="og:site_name"]"#)
        .or_else(|| meta_content(&doc, r#"meta[name="application-name"]"#))
        .or_else(|| final_url.host_str().map(|s| s.to_string()));

    let favicon_raw = link_href(&doc, r#"link[rel="icon"]"#)
        .or_else(|| link_href(&doc, r#"link[rel="shortcut icon"]"#))
        .or_else(|| link_href(&doc, r#"link[rel="apple-touch-icon"]"#))
        .unwrap_or_else(|| "/favicon.ico".to_string());
    let favicon_url = final_url.join(&favicon_raw).ok().map(|u| u.to_string());

    UrlPreview {
        url: final_url.to_string(),
        title,
        description,
        image_url,
        site_name,
        favicon_url,
    }
}

fn meta_content(doc: &Html, selector: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    let el = doc.select(&sel).next()?;
    let v = el.value().attr("content")?.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

fn link_href(doc: &Html, selector: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    let el = doc.select(&sel).next()?;
    let v = el.value().attr("href")?.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

fn title_tag(doc: &Html) -> Option<String> {
    let sel = Selector::parse("title").ok()?;
    let el = doc.select(&sel).next()?;
    let text: String = el.text().collect::<String>().trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

// ============================================
// SINGLETONS
// ============================================

fn url_regex() -> &'static Regex {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    URL_RE.get_or_init(|| {
        // Permisiva pero razonable: http(s)://<host-chars>. Cortamos en whitespace
        // y en los típicos delimitadores de oración.
        Regex::new(r#"https?://[^\s<>\[\]\{\}\(\)\"']+"#).expect("URL regex válida")
    })
}

fn preview_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .connect_timeout(FETCH_TIMEOUT)
            // Redirects manuales — cada hop revalida SSRF.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("Mozilla/5.0 (compatible; api-abdo/1.0; +link-preview)")
            // Misma razón que el client principal: evitar IPv6 en Debian.
            .local_address(std::net::IpAddr::from([0u8, 0, 0, 0]))
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

// ============================================
// TESTS
// ============================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_url_simple() {
        assert_eq!(
            extract_first_url("mira esto https://example.com/foo"),
            Some("https://example.com/foo".to_string())
        );
    }

    #[test]
    fn extract_url_strips_trailing_punctuation() {
        assert_eq!(
            extract_first_url("visita https://example.com/foo, ya mismo."),
            Some("https://example.com/foo".to_string())
        );
    }

    #[test]
    fn extract_url_none_when_no_url() {
        assert_eq!(extract_first_url("hola qué tal"), None);
    }

    #[test]
    fn ssrf_rejects_private_v4() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(!is_ip_public(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_ip_public(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(!is_ip_public(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(!is_ip_public(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!is_ip_public(&IpAddr::V4(Ipv4Addr::new(169, 254, 0, 1))));
        assert!(!is_ip_public(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
    }

    #[test]
    fn ssrf_accepts_public_v4() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(is_ip_public(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(is_ip_public(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn ssrf_rejects_v6_loopback_and_ula() {
        use std::net::{IpAddr, Ipv6Addr};
        assert!(!is_ip_public(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // ULA fc00::/7
        let ula: Ipv6Addr = "fd00::1".parse().unwrap();
        assert!(!is_ip_public(&IpAddr::V6(ula)));
        // link-local fe80::/10
        let ll: Ipv6Addr = "fe80::1".parse().unwrap();
        assert!(!is_ip_public(&IpAddr::V6(ll)));
    }
}
