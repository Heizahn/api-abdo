//! Cliente HTTP de Gemini (Google Generative Language API) con function calling.
//!
//! Endpoint: `POST https://generativelanguage.googleapis.com/v1/models/{model}:generateContent?key={api_key}`
//!
//! Shape del payload (relevante a function calling):
//! - `system_instruction` — el system prompt como bloque separado del history.
//! - `contents` — historial de mensajes con `role: user|model` y `parts`. Cada part
//!   es texto, function call (modelo pide tool), o function response (back devuelve
//!   resultado de tool).
//! - `tools.functionDeclarations` — schemas JSON de los tools habilitados.
//! - `generationConfig` — temperature, maxOutputTokens.
//!
//! El loop (en `runner.rs`) llama `generate_content` en N iteraciones: cada vez que
//! la respuesta trae `functionCall`, se ejecuta el tool, se appendea el call y la
//! response al `contents`, y se vuelve a llamar. Termina cuando la respuesta es
//! texto (`finishReason: STOP`).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::error::ApiError;

/// Base URL default — Google AI Studio (`generativelanguage.googleapis.com/v1beta`).
/// Soporta familias `gemini-1.5-*`, `gemini-2.x-*`, `gemini-3-*` (preview).
/// Para Vertex AI Express (mismo shape de request/response, otro endpoint),
/// el SUPERADMIN setea `GEMINI_BASE_URL` en `.env` con:
///   https://aiplatform.googleapis.com/v1/publishers/google/models
/// El cliente le concatena `/{model}:generateContent` (o `/{model}` para
/// `test_connection` / `list_models`).
const GEMINI_BASE_DEFAULT: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Devuelve la base URL a usar — override del env si está, default si no.
pub fn resolve_base_url<'a>(override_url: Option<&'a str>) -> &'a str {
    override_url
        .map(|s| s.trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .unwrap_or(GEMINI_BASE_DEFAULT)
}

/// Configuración del relay AI (Cloudflare Worker). Cuando ambos campos
/// están presentes, `generate_content` enruta el POST por el worker:
///
/// ```text
/// POST {relay_url}/?url={encoded gemini url}
/// Headers:
///   x-relay-secret: {relay_secret}
///   x-goog-api-key: {api_key}
///   content-type:   application/json
/// ```
///
/// Sin relay, el cliente conecta directo a Gemini (puede fallar desde la
/// VM si el ISP bloquea `googleapis.com`).
#[derive(Debug, Clone)]
pub struct AiRelay {
    pub url: String,
    pub secret: String,
}

impl AiRelay {
    /// Construye desde la config si ambas vars están seteadas.
    pub fn from_config(cfg: &crate::config::Config) -> Option<Self> {
        match (cfg.ai_relay_url.as_ref(), cfg.ai_relay_secret.as_ref()) {
            (Some(u), Some(s)) => Some(AiRelay {
                url: u.clone(),
                secret: s.clone(),
            }),
            _ => None,
        }
    }
}

// ─── Request ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct GenerateContentRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<SystemInstruction>,
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDeclaration>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
}

#[derive(Debug, Serialize)]
pub struct SystemInstruction {
    pub parts: Vec<Part>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Content {
    /// `user` o `model`. Las function responses van con `role: user` (semántica de Gemini).
    /// Modelos "thinking" (gemini-3-*) a veces devuelven `content: {}` cuando
    /// gastan tokens razonando sin emitir texto/parts (handoff silencioso post
    /// tool_call, por ejemplo). Default a "model" para que decode no falle.
    #[serde(default = "default_content_role")]
    pub role: String,
    #[serde(default)]
    pub parts: Vec<Part>,
}

fn default_content_role() -> String {
    "model".to_string()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<FunctionResponse>,
    /// Multimedia inline (imagen, audio, etc). Gemini 1.5+ procesa nativo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<InlineData>,
    /// Blob opaco que Gemini emite junto a `function_call` en modelos con
    /// reasoning ("thinking"). DEBE re-enviarse intacto en el siguiente
    /// roundtrip (cuando devolvemos el `function_response`); de lo contrario
    /// Gemini rebota 400 INVALID_ARGUMENT con "missing thought_signature".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    /// Marca este part como razonamiento interno del modelo (no se renderiza
    /// como texto al usuario). Lo emiten algunos modelos junto al texto final.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
}

/// Adjunto multimedia (imagen, audio, video, PDF). El `data` va en base64
/// estándar (no URL-safe). `mime_type` debe ser uno que Gemini soporte:
/// image/jpeg, image/png, image/webp, audio/mp3, audio/wav, audio/ogg,
/// video/mp4, application/pdf, text/plain.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InlineData {
    pub mime_type: String,
    pub data: String,
}

impl Part {
    pub fn text(s: impl Into<String>) -> Self {
        Part {
            text: Some(s.into()),
            function_call: None,
            function_response: None,
            inline_data: None,
            thought_signature: None,
            thought: None,
        }
    }

    pub fn inline(mime_type: impl Into<String>, data_base64: impl Into<String>) -> Self {
        Part {
            text: None,
            function_call: None,
            function_response: None,
            inline_data: Some(InlineData {
                mime_type: mime_type.into(),
                data: data_base64.into(),
            }),
            thought_signature: None,
            thought: None,
        }
    }

    #[allow(dead_code)]
    pub fn function_response(name: impl Into<String>, response: Value) -> Self {
        Part {
            text: None,
            function_call: None,
            function_response: Some(FunctionResponse {
                name: name.into(),
                response,
            }),
            inline_data: None,
            thought_signature: None,
            thought: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    /// Args del tool — JSON arbitrario que respeta el `parameters.schema`.
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionResponse {
    pub name: String,
    /// Resultado del tool — Gemini espera un objeto JSON. Si el tool devuelve
    /// una lista, se envuelve en `{ "items": [...] }`.
    pub response: Value,
}

#[derive(Debug, Serialize, Clone)]
pub struct ToolDeclaration {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Serialize, Clone)]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    /// Schema JSON estilo OpenAPI 3.0 reducido (subset que Gemini acepta).
    pub parameters: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Control del "thinking budget" para modelos thinking (gemini-2.5-flash,
    /// gemini-2.5-pro, gemini-3-*). Valor `0` = desactiva el razonamiento
    /// interno; todos los `max_output_tokens` van al texto visible. Modelos
    /// no-thinking (gemini-2.5-flash-lite, gemini-2.0-flash-001) ignoran el
    /// campo. Sin esto, gemini-2.5-flash típico gasta 400+ tokens en thoughts
    /// y deja al cliente con `out_tokens=0` (silencio).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    /// `0` = thinking disabled. `-1` = dinámico (modelo decide). `N>0` = cap.
    pub thinking_budget: i32,
}

// ─── Response ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub usage_metadata: Option<UsageMetadata>,
    /// Cuando Gemini rebota el request por filtros de seguridad o input inválido,
    /// `candidates` viene vacío y `prompt_feedback` lleva el motivo.
    #[serde(default)]
    pub prompt_feedback: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub content: Content,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    /// Eco del total reportado por Gemini. No lo usamos hoy (preferimos la
    /// suma de input+output para que cuadre con el cost estimate), pero lo
    /// mantenemos en el shape por si en el futuro hace falta para auditar
    /// discrepancias entre el reporte de Gemini y nuestro cálculo.
    #[serde(default)]
    #[allow(dead_code)]
    pub total_token_count: u32,
}

// ─── Cliente ────────────────────────────────────────────────────────────────

/// Llama `generateContent`. El caller construye el request completo (incluyendo
/// system_instruction, contents acumulados y tools) y procesa la respuesta para
/// decidir si itera (function call) o termina (texto).
///
/// Si `relay` está presente, el POST se enruta vía Cloudflare Worker; si no, va
/// directo a `generativelanguage.googleapis.com`. La api_key viaja como header
/// `x-goog-api-key` en ambos casos (no como query param) — más seguro y
/// transparente para el worker, que solo la pasa upstream.
///
/// **Retry**: ante errores transitorios (5xx upstream, 429 rate-limit, fallo
/// de conexión) reintenta hasta `RETRY_MAX_ATTEMPTS` veces con backoff
/// exponencial (1s, 2s). 4xx no transitorios (400, 401, 403, 404) no se
/// reintentan — fallan rápido.
pub async fn generate_content(
    http: &reqwest::Client,
    api_key: &str,
    model_id: &str,
    timeout_seconds: u32,
    body: &GenerateContentRequest,
    relay: Option<&AiRelay>,
    base_url_override: Option<&str>,
) -> Result<GenerateContentResponse, ApiError> {
    if api_key.is_empty() {
        return Err(ApiError::domain_simple(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "ai_api_key_missing",
            "El workspace no tiene api_key de Gemini configurada",
        ));
    }
    let base = resolve_base_url(base_url_override);
    // AI Studio: /models/{id}:generateContent
    // Vertex Express: /publishers/google/models es la base, así que la
    // concatenación queda /publishers/google/models/{id}:generateContent.
    // Detectamos si el override ya tiene `/models` al final.
    let target_path = if base.ends_with("/models") {
        format!("{}/{}:generateContent", base, model_id)
    } else {
        format!("{}/models/{}:generateContent", base, model_id)
    };
    // Mandamos la api_key como query param (`?key=`) ADEMÁS del header
    // `x-goog-api-key`. AI Studio acepta cualquiera; Vertex AI Express
    // documenta query param. Dejar ambos cubre los dos productos sin
    // ambigüedad — si uno falla, el otro pasa.
    let target_url = format!("{}?key={}", target_path, api_key);

    let mut last_err: Option<ApiError> = None;
    for attempt in 0..RETRY_MAX_ATTEMPTS {
        if attempt > 0 {
            let idx = (attempt as usize - 1).min(RETRY_BACKOFF_MS.len() - 1);
            let backoff_ms = RETRY_BACKOFF_MS[idx];
            tracing::warn!(
                "[gemini] retry {}/{} tras {}ms",
                attempt + 1,
                RETRY_MAX_ATTEMPTS,
                backoff_ms
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }

        let req = match relay {
            Some(r) => http
                .post(&r.url)
                .query(&[("url", target_url.as_str())])
                .header("x-relay-secret", &r.secret)
                .header("x-goog-api-key", api_key),
            None => http
                .post(&target_url)
                .header("x-goog-api-key", api_key),
        };

        let resp = match req
            .json(body)
            .timeout(Duration::from_secs(timeout_seconds.max(1) as u64))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("[gemini] request failed: {}", e);
                // Errores de red son transitorios — reintentamos.
                last_err = Some(ApiError::domain_simple(
                    axum::http::StatusCode::BAD_GATEWAY,
                    "ai_upstream_unreachable",
                    "No se pudo contactar a Gemini",
                ));
                continue;
            }
        };

        let status = resp.status();
        if status.is_success() {
            // Leemos el body como texto primero — así si el decode falla
            // tenemos el cuerpo crudo para diagnosticar (Gemini a veces
            // devuelve 200 con JSON truncado o un shape inesperado).
            let body_text = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("[gemini] read body failed: {}", e);
                    last_err = Some(ApiError::domain_simple(
                        axum::http::StatusCode::BAD_GATEWAY,
                        "ai_decode_error",
                        "No se pudo leer respuesta de Gemini",
                    ));
                    continue;
                }
            };
            match serde_json::from_str::<GenerateContentResponse>(&body_text) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    let preview = if body_text.len() > 1000 {
                        format!("{}…(truncated, total {} chars)", &body_text[..1000], body_text.len())
                    } else {
                        body_text.clone()
                    };
                    tracing::error!(
                        "[gemini] decode response failed: {} | body: {}",
                        e, preview
                    );
                    // Decode error de un 200 OK suele ser body truncado en
                    // transit o un response degradado de Google. Lo tratamos
                    // como transient y reintentamos.
                    last_err = Some(ApiError::domain_simple(
                        axum::http::StatusCode::BAD_GATEWAY,
                        "ai_decode_error",
                        "Respuesta de Gemini no decodificable",
                    ));
                    continue;
                }
            }
        }

        let body_text = resp.text().await.unwrap_or_default();
        tracing::error!("[gemini] non-2xx {}: {}", status, body_text);
        let code = match status.as_u16() {
            400 => "ai_invalid_request",
            401 | 403 => "ai_auth_failed",
            429 => "ai_rate_limited",
            500..=599 => "ai_upstream_error",
            _ => "ai_unexpected",
        };
        let err = ApiError::domain_with_details(
            axum::http::StatusCode::BAD_GATEWAY,
            code,
            format!("Gemini respondió {}", status.as_u16()),
            serde_json::json!({ "upstream_status": status.as_u16(), "body": body_text }),
        );

        // Solo reintentamos transitorios (429 + 5xx). 4xx no recoverable.
        if !is_retryable_status(status.as_u16()) {
            return Err(err);
        }
        last_err = Some(err);
    }

    Err(last_err.unwrap_or_else(|| {
        ApiError::domain_simple(
            axum::http::StatusCode::BAD_GATEWAY,
            "ai_upstream_unreachable",
            "Gemini falló tras varios intentos",
        )
    }))
}

/// Total de intentos (incluye el primero). 3 = original + 2 retries.
const RETRY_MAX_ATTEMPTS: u32 = 3;

/// Backoff entre intentos (ms). Index 0 = espera antes del 2do intento, etc.
const RETRY_BACKOFF_MS: &[u64] = &[1_000, 2_500];

fn is_retryable_status(s: u16) -> bool {
    matches!(s, 408 | 429 | 500 | 502 | 503 | 504)
}

// ─── Test de conexión ───────────────────────────────────────────────────────

/// Verifica que `api_key` + `model_id` sean válidos. Usa
/// `GET /v1/models/{model_id}` que devuelve metadata del modelo y NO consume
/// cuota de generación. Si todo bien devuelve `Ok(())`; cualquier error sube
/// como `ApiError::Domain` con código diagnóstico (mismo mapping que
/// `generate_content`).
pub async fn test_connection(
    http: &reqwest::Client,
    api_key: &str,
    model_id: &str,
    timeout_seconds: u32,
    relay: Option<&AiRelay>,
    base_url_override: Option<&str>,
) -> Result<(), ApiError> {
    if api_key.is_empty() {
        return Err(ApiError::domain_simple(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "ai_api_key_missing",
            "api_key requerida",
        ));
    }
    if model_id.is_empty() {
        return Err(ApiError::domain_simple(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "missing_field",
            "model_id requerido",
        ));
    }

    let base = resolve_base_url(base_url_override);
    let target_path = if base.ends_with("/models") {
        format!("{}/{}", base, model_id)
    } else {
        format!("{}/models/{}", base, model_id)
    };
    let target_url = format!("{}?key={}", target_path, api_key);

    let req = match relay {
        Some(r) => http
            .get(&r.url)
            .query(&[("url", target_url.as_str())])
            .header("x-relay-secret", &r.secret)
            .header("x-goog-api-key", api_key),
        None => http
            .get(&target_url)
            .header("x-goog-api-key", api_key),
    };

    let resp = req
        .timeout(Duration::from_secs(timeout_seconds.max(1) as u64))
        .send()
        .await
        .map_err(|e| {
            tracing::error!("[gemini] test_connection request failed: {}", e);
            ApiError::domain_simple(
                axum::http::StatusCode::BAD_GATEWAY,
                "ai_upstream_unreachable",
                "No se pudo contactar a Gemini",
            )
        })?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }

    let body_text = resp.text().await.unwrap_or_default();
    tracing::warn!("[gemini] test_connection non-2xx {}: {}", status, body_text);
    let code = match status.as_u16() {
        400 => "ai_invalid_request",
        401 | 403 => "ai_auth_failed",
        404 => "ai_model_not_found",
        429 => "ai_rate_limited",
        500..=599 => "ai_upstream_error",
        _ => "ai_unexpected",
    };
    Err(ApiError::domain_with_details(
        axum::http::StatusCode::BAD_GATEWAY,
        code,
        format!("Gemini respondió {}", status.as_u16()),
        serde_json::json!({ "upstream_status": status.as_u16(), "body": body_text }),
    ))
}

// ─── Listar modelos disponibles ─────────────────────────────────────────────

/// Item crudo del response de `GET /v1beta/models`. Sólo deserializamos los
/// campos que consumimos — Gemini agrega cosas (input_modalities, etc.) que
/// hoy no usamos.
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GeminiModelEntry {
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub input_token_limit: Option<u32>,
    #[serde(default)]
    pub output_token_limit: Option<u32>,
    #[serde(default)]
    pub supported_generation_methods: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ListModelsResponse {
    #[serde(default)]
    models: Vec<GeminiModelEntry>,
}

/// `GET /v1beta/models` — devuelve TODOS los modelos visibles para la api_key
/// (sin filtrar). El handler los filtra por familia `gemini-*` y por
/// `generateContent` antes de devolver al FE.
pub async fn list_models(
    http: &reqwest::Client,
    api_key: &str,
    timeout_seconds: u32,
    relay: Option<&AiRelay>,
    base_url_override: Option<&str>,
) -> Result<Vec<GeminiModelEntry>, ApiError> {
    if api_key.is_empty() {
        return Err(ApiError::domain_simple(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "ai_api_key_missing",
            "api_key requerida",
        ));
    }

    let base = resolve_base_url(base_url_override);
    // Para Vertex Express, listar modelos no aplica del mismo modo —
    // el endpoint termina en `/publishers/google/models` directamente,
    // que SÍ devuelve la lista. Para AI Studio le agregamos `/models`.
    let target_path = if base.ends_with("/models") {
        base.to_string()
    } else {
        format!("{}/models", base)
    };
    let target_url = format!("{}?key={}", target_path, api_key);

    let req = match relay {
        Some(r) => http
            .get(&r.url)
            .query(&[("url", target_url.as_str())])
            .header("x-relay-secret", &r.secret)
            .header("x-goog-api-key", api_key),
        None => http
            .get(&target_url)
            .header("x-goog-api-key", api_key),
    };

    let resp = req
        .timeout(Duration::from_secs(timeout_seconds.max(1) as u64))
        .send()
        .await
        .map_err(|e| {
            tracing::error!("[gemini] list_models request failed: {}", e);
            ApiError::domain_simple(
                axum::http::StatusCode::BAD_GATEWAY,
                "gemini_unreachable",
                "No se pudo contactar a Gemini",
            )
        })?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        tracing::warn!("[gemini] list_models non-2xx {}: {}", status, body_text);
        let (http_status, code) = match status.as_u16() {
            400 => (axum::http::StatusCode::BAD_GATEWAY, "ai_invalid_request"),
            401 | 403 => (axum::http::StatusCode::UNAUTHORIZED, "invalid_api_key"),
            429 => (axum::http::StatusCode::TOO_MANY_REQUESTS, "gemini_rate_limited"),
            500..=599 => (axum::http::StatusCode::BAD_GATEWAY, "gemini_unreachable"),
            _ => (axum::http::StatusCode::BAD_GATEWAY, "ai_unexpected"),
        };
        return Err(ApiError::domain_with_details(
            http_status,
            code,
            format!("Gemini respondió {}", status.as_u16()),
            serde_json::json!({ "upstream_status": status.as_u16(), "body": body_text }),
        ));
    }

    let parsed = resp.json::<ListModelsResponse>().await.map_err(|e| {
        tracing::error!("[gemini] list_models decode response: {}", e);
        ApiError::domain_simple(
            axum::http::StatusCode::BAD_GATEWAY,
            "ai_decode_error",
            "Respuesta de Gemini no decodificable",
        )
    })?;
    Ok(parsed.models)
}

// ─── Helpers de costos (estimación) ─────────────────────────────────────────

/// Estimación grosera de costo USD basada en tarifas Gemini 1.5 Flash (2025-Q1):
/// - input: ~$0.075 / 1M tokens
/// - output: ~$0.30 / 1M tokens
///
/// Para Pro/2.0 los multiplicadores cambian — el cálculo sirve como referencia
/// en la UI del sandbox y métricas, no para billing real.
pub fn estimate_cost_usd(model_id: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    let (in_per_m, out_per_m) = match model_id {
        m if m.contains("flash") => (0.075, 0.30),
        m if m.contains("pro") => (1.25, 5.00),
        _ => (0.075, 0.30),
    };
    let input = (input_tokens as f64) * in_per_m / 1_000_000.0;
    let output = (output_tokens as f64) * out_per_m / 1_000_000.0;
    input + output
}
