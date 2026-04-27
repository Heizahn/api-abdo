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

/// Base URL de la Generative Language API. Usamos `v1beta` porque ahí viven
/// las familias `gemini-1.5-*` y `gemini-2.x-*`. La `v1` "stable" sólo
/// expone modelos legacy (`gemini-pro` 1.0) — si el SUPERADMIN configurara
/// uno de esos, Gemini igual responde porque ambos endpoints coexisten.
const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

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
    pub role: String,
    pub parts: Vec<Part>,
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
}

impl Part {
    pub fn text(s: impl Into<String>) -> Self {
        Part {
            text: Some(s.into()),
            function_call: None,
            function_response: None,
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
}

// ─── Response ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
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
pub async fn generate_content(
    http: &reqwest::Client,
    api_key: &str,
    model_id: &str,
    timeout_seconds: u32,
    body: &GenerateContentRequest,
    relay: Option<&AiRelay>,
) -> Result<GenerateContentResponse, ApiError> {
    if api_key.is_empty() {
        return Err(ApiError::domain_simple(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "ai_api_key_missing",
            "El workspace no tiene api_key de Gemini configurada",
        ));
    }
    let target_url = format!("{}/models/{}:generateContent", GEMINI_BASE, model_id);

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

    let resp = req
        .json(body)
        .timeout(Duration::from_secs(timeout_seconds.max(1) as u64))
        .send()
        .await
        .map_err(|e| {
            tracing::error!("[gemini] request failed: {}", e);
            ApiError::domain_simple(
                axum::http::StatusCode::BAD_GATEWAY,
                "ai_upstream_unreachable",
                "No se pudo contactar a Gemini",
            )
        })?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        tracing::error!("[gemini] non-2xx {}: {}", status, body_text);
        // Errores de Gemini típicos:
        // - 400 INVALID_ARGUMENT (body mal formado)
        // - 403 PERMISSION_DENIED (api_key inválida o sin permisos)
        // - 429 RESOURCE_EXHAUSTED (rate limit)
        // - 500/503 (transitorios upstream)
        let code = match status.as_u16() {
            400 => "ai_invalid_request",
            401 | 403 => "ai_auth_failed",
            429 => "ai_rate_limited",
            500..=599 => "ai_upstream_error",
            _ => "ai_unexpected",
        };
        return Err(ApiError::domain_with_details(
            axum::http::StatusCode::BAD_GATEWAY,
            code,
            format!("Gemini respondió {}", status.as_u16()),
            serde_json::json!({ "upstream_status": status.as_u16(), "body": body_text }),
        ));
    }

    resp.json::<GenerateContentResponse>().await.map_err(|e| {
        tracing::error!("[gemini] decode response: {}", e);
        ApiError::domain_simple(
            axum::http::StatusCode::BAD_GATEWAY,
            "ai_decode_error",
            "Respuesta de Gemini no decodificable",
        )
    })
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

    let target_url = format!("{}/models/{}", GEMINI_BASE, model_id);

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
