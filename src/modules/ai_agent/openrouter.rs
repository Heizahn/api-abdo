//! Cliente HTTP para OpenRouter (API compatible con OpenAI).
//!
//! Endpoint: `POST {base_url}/chat/completions`
//!
//! OpenRouter acepta el formato OpenAI estándar:
//! - `messages` — array con roles `system` | `user` | `assistant` | `tool`.
//! - `tools` — array con schemas JSON de los tools.
//! - `tool_choice` — `auto` | `none` | `required`.
//! - Content puede ser string o array de bloques (multimodal).
//!
//! El loop de runner (en `runner.rs`) llama `complete` en N iteraciones:
//! cada vez que la respuesta trae `tool_calls`, se ejecutan los tools,
//! se appendean al history con su `tool_call_id`, y se vuelve a llamar.
//! Termina cuando la respuesta es texto puro (sin tool_calls).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::error::ApiError;

/// URL por defecto — OpenRouter API v1. Hardcoded.
///
/// No exponemos esto como env var: si OpenRouter cambia su URL, hay que
/// rebuild igual (no es algo que cambie sin redeploy de la app), así que
/// el indirection sólo agrega complejidad. El SUPERADMIN tiene un override
/// per-agente desde la UI por si necesita apuntar a un proxy.
const OPENROUTER_DEFAULT_BASE: &str = "https://openrouter.ai/api/v1";

/// Resuelve la base URL efectiva. Solo respeta el override per-agent (UI);
/// el resto cae al default hardcoded.
pub fn resolve_base_url(per_agent: Option<&str>) -> String {
    if let Some(s) = per_agent.filter(|s| !s.trim().is_empty()) {
        return s.trim_end_matches('/').to_string();
    }
    OPENROUTER_DEFAULT_BASE.to_string()
}

// ─── Relay ───────────────────────────────────────────────────────────────────

/// Configuración del relay AI (Cloudflare Worker). Cuando ambos campos
/// están presentes, `complete` enruta el POST por el worker:
///
/// ```text
/// POST {relay_url}/?url={encoded openrouter url}
/// Headers:
///   x-relay-secret: {relay_secret}
///   authorization:  Bearer {api_key}
///   content-type:   application/json
/// ```
#[derive(Debug, Clone)]
pub struct AiRelay {
    pub url: String,
    pub secret: String,
}

impl AiRelay {
    /// Construye desde la config si ambas vars están seteadas.
    pub fn from_config(cfg: &crate::config::Config) -> Option<Self> {
        match (cfg.relay_url.as_ref(), cfg.relay_secret.as_ref()) {
            (Some(u), Some(s)) => Some(AiRelay {
                url: u.clone(),
                secret: s.clone(),
            }),
            _ => None,
        }
    }
}

// ─── Request types ───────────────────────────────────────────────────────────

/// Request body para `POST /chat/completions`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Siempre `None` en este módulo (no usamos streaming).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

/// Discriminante para el campo `tool_choice`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
}

/// Controla el formato de respuesta del modelo.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
}

/// Mensaje en el historial de conversación.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatMessage {
    /// `"system"` | `"user"` | `"assistant"` | `"tool"`
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Solo en mensajes `role: "tool"` — referencia al ID del `ToolCall` que responde.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Nombre de la tool que generó el resultado (informativo para logs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Contenido de un mensaje: string plano o array de bloques (multimodal).
/// `#[serde(untagged)]` permite serializar/deserializar ambas formas exactamente
/// como el wire format de OpenAI (string OR array, sin wrapper).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Bloque de contenido multimodal (imagen, audio, texto inline).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ImageUrl { image_url: ImageUrlInner },
    InputAudio { input_audio: InputAudioInner },
}

/// URL (o data URI base64) para una imagen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrlInner {
    /// `"data:<mime>;base64,<b64>"` para imágenes inline, o URL absoluta.
    pub url: String,
}

/// Audio inline (base64).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputAudioInner {
    /// Base64 estándar (NO url-safe).
    pub data: String,
    /// `"wav"` | `"mp3"` | `"ogg"`
    pub format: String,
}

// ─── Tool definitions (request side) ────────────────────────────────────────

/// Definición de un tool que el modelo puede invocar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Siempre `"function"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

/// Metadata de la función del tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    /// JSON Schema de los parámetros (subset OpenAPI 3.0 que OpenRouter acepta).
    pub parameters: Value,
}

// ─── Tool calls (response side) ──────────────────────────────────────────────

/// Tool call emitida por el modelo en su respuesta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// ID único por call — DEBE echarse back en `tool_call_id` del mensaje tool.
    pub id: String,
    /// Siempre `"function"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

/// Función llamada por el modelo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Args del tool — JSON-encoded STRING (no un Value directo). El runner
    /// debe hacer `serde_json::from_str::<Value>(&arguments)` antes de dispatch.
    pub arguments: String,
}

// ─── Response types ───────────────────────────────────────────────────────────

/// Response de `POST /chat/completions`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionResponse {
    /// Preservados para audit/logging aunque el runner no los use ahora.
    #[allow(dead_code)]
    pub id: Option<String>,
    #[allow(dead_code)]
    pub model: Option<String>,
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<UsageMetadata>,
}

/// Una "opción" en el array de choices.
#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    #[allow(dead_code)]
    pub index: u32,
    pub message: ChatMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Tokens consumidos por el request.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_tokens: i64,
    #[serde(default)]
    pub completion_tokens: i64,
    /// Mantenido para compatibilidad de logging; `prompt_tokens` +
    /// `completion_tokens` son los que usa el runner.
    #[allow(dead_code)]
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// Detalle del breakdown de prompt tokens.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: i64,
}

/// Shape del error de OpenRouter (HTTP 4xx / 5xx).
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

/// Body del error.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorBody {
    #[serde(default)]
    pub code: Option<String>,
    pub message: String,
}

// ─── Cliente ──────────────────────────────────────────────────────────────────

/// Total de intentos (incluye el primero). 3 = original + 2 retries.
const RETRY_MAX_ATTEMPTS: u32 = 3;
/// Backoff entre intentos (ms). Index 0 = espera antes del 2do intento.
const RETRY_BACKOFF_MS: &[u64] = &[1_000, 2_500];

fn is_retryable_status(s: u16) -> bool {
    matches!(s, 408 | 429 | 500 | 502 | 503 | 504)
}

/// Cliente para la API de OpenRouter.
pub struct OpenRouterClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    relay: Option<AiRelay>,
}

impl OpenRouterClient {
    pub fn new(
        http: reqwest::Client,
        base_url: String,
        api_key: String,
        relay: Option<AiRelay>,
    ) -> Self {
        Self { http, base_url, api_key, relay }
    }

    /// Llama `POST {base_url}/chat/completions`. Maneja retries para errores
    /// transitorios (429 + 5xx) con backoff exponencial.
    pub async fn complete(
        &self,
        req: &ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, ApiError> {
        if self.api_key.is_empty() {
            return Err(ApiError::domain_simple(
                axum::http::StatusCode::BAD_GATEWAY,
                "ai_auth_failed",
                "OpenRouter API key no configurada",
            ));
        }

        let target_url = format!("{}/chat/completions", self.base_url);
        let mut last_err: Option<ApiError> = None;

        for attempt in 0..RETRY_MAX_ATTEMPTS {
            if attempt > 0 {
                let idx = (attempt as usize - 1).min(RETRY_BACKOFF_MS.len() - 1);
                let backoff_ms = RETRY_BACKOFF_MS[idx];
                tracing::warn!(
                    "[openrouter] retry {}/{} tras {}ms",
                    attempt + 1,
                    RETRY_MAX_ATTEMPTS,
                    backoff_ms
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }

            let request_builder = match &self.relay {
                Some(r) => self
                    .http
                    .post(&r.url)
                    .query(&[("url", target_url.as_str())])
                    .header("x-relay-secret", &r.secret)
                    .header("authorization", format!("Bearer {}", self.api_key)),
                None => self
                    .http
                    .post(&target_url)
                    .header("authorization", format!("Bearer {}", self.api_key)),
            };

            let resp = match request_builder
                .header("HTTP-Referer", "https://api.abdo.local")
                .header("X-OpenRouter-Title", "api-abdo")
                .json(req)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("[openrouter] request failed: {}", e);
                    last_err = Some(ApiError::domain_simple(
                        axum::http::StatusCode::BAD_GATEWAY,
                        "ai_upstream_error",
                        "Error de red contactando OpenRouter",
                    ));
                    continue;
                }
            };

            let status = resp.status();

            if status.is_success() {
                let body_text = match resp.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("[openrouter] read body failed: {}", e);
                        last_err = Some(ApiError::domain_simple(
                            axum::http::StatusCode::BAD_GATEWAY,
                            "ai_upstream_error",
                            "No se pudo leer respuesta de OpenRouter",
                        ));
                        continue;
                    }
                };
                return match serde_json::from_str::<ChatCompletionResponse>(&body_text) {
                    Ok(r) => Ok(r),
                    Err(e) => {
                        let preview = if body_text.len() > 1000 {
                            format!("{}…(truncated)", &body_text[..1000])
                        } else {
                            body_text.clone()
                        };
                        tracing::warn!(
                            "[openrouter] decode response failed: {} | body: {}",
                            e, preview
                        );
                        Err(ApiError::domain_simple(
                            axum::http::StatusCode::BAD_GATEWAY,
                            "ai_invalid_response",
                            "Respuesta inválida de OpenRouter",
                        ))
                    }
                };
            }

            let body_text = resp.text().await.unwrap_or_default();
            tracing::error!("[openrouter] non-2xx {}: {}", status, body_text);

            let (code, msg) = match status.as_u16() {
                400 => ("ai_invalid_request", "Petición inválida hacia OpenRouter"),
                401 | 403 => ("ai_auth_failed", "OpenRouter respondió 401/403 — clave inválida"),
                402 => ("ai_payment_required", "OpenRouter requiere saldo/pago"),
                404 => ("ai_model_not_found", "Modelo no encontrado en OpenRouter"),
                429 => ("ai_rate_limit", "Límite de tasa de OpenRouter alcanzado"),
                500..=599 => ("ai_upstream_error", "Error temporal de OpenRouter"),
                _ => ("ai_upstream_error", "Error inesperado de OpenRouter"),
            };

            let err = ApiError::domain_simple(
                axum::http::StatusCode::BAD_GATEWAY,
                code,
                msg,
            );

            if !is_retryable_status(status.as_u16()) {
                return Err(err);
            }
            last_err = Some(err);
        }

        Err(last_err.unwrap_or_else(|| {
            ApiError::domain_simple(
                axum::http::StatusCode::BAD_GATEWAY,
                "ai_upstream_error",
                "OpenRouter falló tras varios intentos",
            )
        }))
    }

    /// Verifica conectividad enviando un request mínimo (1 token). No registra
    /// `AiInteraction`. Retorna `Ok(())` si el modelo responde HTTP 200.
    pub async fn test_connection(&self, model: &str) -> Result<(), ApiError> {
        if self.api_key.is_empty() {
            return Err(ApiError::domain_simple(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "ai_api_key_missing",
                "api_key requerida",
            ));
        }
        let req = ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: Some(MessageContent::Text("ping".into())),
                ..Default::default()
            }],
            tools: None,
            tool_choice: None,
            response_format: None,
            temperature: Some(0.0),
            max_tokens: Some(1),
            stream: None,
        };
        self.complete(&req).await.map(|_| ())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Serialización de requests ─────────────────────────────────────────────

    #[test]
    fn test_request_serializes_text_only() {
        let req = ChatCompletionRequest {
            model: "openai/gpt-4o-mini".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: Some(MessageContent::Text("Hola".into())),
                ..Default::default()
            }],
            tools: None,
            tool_choice: None,
            response_format: None,
            temperature: Some(0.7),
            max_tokens: Some(500),
            stream: None,
        };

        let json_str = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        // content debe ser string, no array
        assert_eq!(v["messages"][0]["content"], "Hola");
        assert!(v.get("tools").is_none() || v["tools"].is_null());
    }

    #[test]
    fn test_request_serializes_with_image_block() {
        let req = ChatCompletionRequest {
            model: "openai/gpt-4o-mini".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: Some(MessageContent::Blocks(vec![
                    ContentBlock::Text { text: "¿Qué es esto?".into() },
                    ContentBlock::ImageUrl {
                        image_url: ImageUrlInner {
                            url: "data:image/jpeg;base64,AABB==".into(),
                        },
                    },
                ])),
                ..Default::default()
            }],
            tools: None,
            tool_choice: None,
            response_format: None,
            temperature: None,
            max_tokens: None,
            stream: None,
        };

        let json_str = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        // content debe ser array
        assert!(v["messages"][0]["content"].is_array());
        let blocks = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image_url");
        assert_eq!(blocks[1]["image_url"]["url"], "data:image/jpeg;base64,AABB==");
    }

    // ── Deserialización de responses ──────────────────────────────────────────

    #[test]
    fn test_response_parses_text_only() {
        let raw = r#"{
            "id": "gen-123",
            "model": "openai/gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hola, ¿en qué puedo ayudarte?"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 10,
                "total_tokens": 30
            }
        }"#;

        let resp: ChatCompletionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.choices.len(), 1);
        match &resp.choices[0].message.content {
            Some(MessageContent::Text(s)) => assert_eq!(s, "Hola, ¿en qué puedo ayudarte?"),
            other => panic!("Expected Text content, got {:?}", other),
        }
        assert!(resp.choices[0].message.tool_calls.is_none());
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 20);
        assert_eq!(usage.completion_tokens, 10);
    }

    #[test]
    fn test_response_parses_tool_calls() {
        let raw = r#"{
            "id": "gen-456",
            "model": "openai/gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_c1",
                            "type": "function",
                            "function": {
                                "name": "lookup_customer",
                                "arguments": "{\"phone\":\"04140000000\"}"
                            }
                        },
                        {
                            "id": "call_c2",
                            "type": "function",
                            "function": {
                                "name": "get_invoices",
                                "arguments": "{\"limit\":5}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        }"#;

        let resp: ChatCompletionResponse = serde_json::from_str(raw).unwrap();
        let tool_calls = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].id, "call_c1");
        assert_eq!(tool_calls[0].function.name, "lookup_customer");
        assert_eq!(tool_calls[0].function.arguments, "{\"phone\":\"04140000000\"}");
        assert_eq!(tool_calls[1].id, "call_c2");
        assert_eq!(tool_calls[1].function.name, "get_invoices");
    }

    #[test]
    fn test_response_parses_missing_usage() {
        // Spec 4.3: usage ausente no debe hacer panic; todos los campos a 0.
        let raw = r#"{
            "id": "gen-789",
            "model": "openai/gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "OK" },
                "finish_reason": "stop"
            }]
        }"#;

        let resp: ChatCompletionResponse = serde_json::from_str(raw).unwrap();
        let usage = resp.usage.unwrap_or_default();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
        // No panic — test passed.
    }

    // ── resolve_base_url ──────────────────────────────────────────────────────

    #[test]
    fn resolve_base_url_default() {
        assert_eq!(resolve_base_url(None), OPENROUTER_DEFAULT_BASE);
        assert_eq!(resolve_base_url(Some("")), OPENROUTER_DEFAULT_BASE);
        assert_eq!(resolve_base_url(Some("   ")), OPENROUTER_DEFAULT_BASE);
    }

    #[test]
    fn resolve_base_url_per_agent_override() {
        assert_eq!(
            resolve_base_url(Some("https://proxy.local/api/v1")),
            "https://proxy.local/api/v1"
        );
        // Trailing slash trimmed.
        assert_eq!(
            resolve_base_url(Some("https://proxy.local/api/v1/")),
            "https://proxy.local/api/v1"
        );
    }
}
