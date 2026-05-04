//! Pre-clasificador (Phase 3a): un único roundtrip a gemini-2.5-flash-lite que
//! decide si el turno es trivial (Spam / GreetingOnly), routeable directo a
//! un especialista (Clear*), o ambiguo (cae al flujo normal).
//!
//! No usa tools, no usa history — solo el último mensaje + un summary corto
//! del cliente. Latencia objetivo: < 500 ms p95.

use serde::Deserialize;
use std::time::Instant;

use super::gemini::{
    self, AiRelay, Content, GenerateContentRequest, GenerationConfig, Part, SystemInstruction,
    ThinkingConfig,
};

// ──────────────────────────────────────────────
// Public types
// ──────────────────────────────────────────────

/// Resultado de la clasificación semántica del mensaje.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreClassResult {
    Spam,
    GreetingOnly,
    ClearVentas,
    ClearPagos,
    ClearSoporte,
    Ambiguous,
}

impl PreClassResult {
    pub fn as_str(&self) -> &'static str {
        match self {
            PreClassResult::Spam => "Spam",
            PreClassResult::GreetingOnly => "GreetingOnly",
            PreClassResult::ClearVentas => "ClearVentas",
            PreClassResult::ClearPagos => "ClearPagos",
            PreClassResult::ClearSoporte => "ClearSoporte",
            PreClassResult::Ambiguous => "Ambiguous",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "Spam" => PreClassResult::Spam,
            "GreetingOnly" => PreClassResult::GreetingOnly,
            "ClearVentas" => PreClassResult::ClearVentas,
            "ClearPagos" => PreClassResult::ClearPagos,
            "ClearSoporte" => PreClassResult::ClearSoporte,
            _ => PreClassResult::Ambiguous,
        }
    }
}

/// Tokens consumidos por el pre-clasificador (roundtrip a flash-lite).
#[derive(Debug, Clone, Copy, Default)]
pub struct PreClassTokens {
    pub input: u32,
    pub output: u32,
}

/// Resultado completo del pre-clasificador.
#[derive(Debug, Clone)]
pub struct PreClassResultFull {
    /// Variante directamente desde Gemini (o `Ambiguous` en error de parse).
    /// Guardada para auditoría en `AiInteraction.pre_class_result`.
    pub variant: PreClassResult,
    /// Igual que `variant`, pero coercida a `Ambiguous` si `confidence < 0.85`.
    /// Este es el valor que `dispatch.rs` consume para tomar decisiones.
    pub gated_variant: PreClassResult,
    pub confidence: f32,
    #[allow(dead_code)]
    pub reasoning: String,
    pub tokens: PreClassTokens,
    pub latency_ms: u32,
}

/// Contexto que el pre-clasificador necesita para llamar a Gemini.
pub struct PreClassifierContext<'a> {
    pub api_key: &'a str,
    pub relay: Option<&'a AiRelay>,
    pub base_url_override: Option<&'a str>,
    pub http: &'a reqwest::Client,
}

// ──────────────────────────────────────────────
// Internal types
// ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PreClassRaw {
    pub result: String,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub reasoning: String,
}

// ──────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────

const PRE_CLASS_MODEL_ID: &str = "gemini-2.5-flash-lite";
const PRE_CLASS_TIMEOUT_SECONDS: u32 = 10;
const PRE_CLASS_CONFIDENCE_THRESHOLD: f32 = 0.85;

// ──────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────

/// Clasifica el texto del mensaje en una de las 6 variantes de `PreClassResult`.
///
/// Usa `gemini-2.5-flash-lite` con `temperature=0.0` y `max_output_tokens=80`
/// para máxima velocidad y costo mínimo. Si la confianza es < 0.85, la
/// variante se coerce a `Ambiguous` (pero el valor original se preserva para
/// auditoría). En caso de error de red o parse, retorna `Err(String)` y el
/// dispatcher lo trata como un skip silencioso del gate.
pub async fn classify(
    text: &str,
    customer_lookup_summary: &str,
    ctx: &PreClassifierContext<'_>,
) -> Result<PreClassResultFull, String> {
    let started = Instant::now();
    let prompt = build_prompt(text, customer_lookup_summary);

    let body = GenerateContentRequest {
        system_instruction: Some(SystemInstruction {
            parts: vec![Part::text(prompt)],
        }),
        contents: vec![Content {
            role: "user".into(),
            parts: vec![Part::text(text)],
        }],
        tools: None,
        generation_config: Some(GenerationConfig {
            temperature: Some(0.0),
            max_output_tokens: Some(80),
            thinking_config: Some(ThinkingConfig { thinking_budget: 0 }),
            response_mime_type: Some("application/json".to_string()),
            response_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "result":     { "type": "string" },
                    "confidence": { "type": "number" },
                    "reasoning":  { "type": "string" }
                },
                "required": ["result", "confidence", "reasoning"]
            })),
        }),
    };

    let resp = gemini::generate_content(
        ctx.http,
        ctx.api_key,
        PRE_CLASS_MODEL_ID,
        PRE_CLASS_TIMEOUT_SECONDS,
        &body,
        ctx.relay,
        ctx.base_url_override,
    )
    .await
    .map_err(|e| format!("{:?}", e))?;

    let usage = resp.usage_metadata.unwrap_or_default();
    let tokens = PreClassTokens {
        input: usage.prompt_token_count,
        output: usage.candidates_token_count,
    };

    let raw_text = resp
        .candidates
        .into_iter()
        .next()
        .and_then(|c| c.content.parts.into_iter().find_map(|p| p.text))
        .unwrap_or_default();

    let cleaned = strip_json_fence(&raw_text);
    let parsed: PreClassRaw = serde_json::from_str(&cleaned).unwrap_or_else(|e| {
        tracing::warn!(
            "[ai_agent.pre_classifier] parse error ({}); coercing to Ambiguous. raw='{}'",
            e,
            cleaned
        );
        PreClassRaw {
            result: "Ambiguous".to_string(),
            confidence: 0.0,
            reasoning: "parse_error".into(),
        }
    });

    let variant = PreClassResult::from_str(&parsed.result);
    let gated_variant = if parsed.confidence < PRE_CLASS_CONFIDENCE_THRESHOLD {
        PreClassResult::Ambiguous
    } else {
        variant
    };

    Ok(PreClassResultFull {
        variant,
        gated_variant,
        confidence: parsed.confidence,
        reasoning: parsed.reasoning,
        tokens,
        latency_ms: started.elapsed().as_millis() as u32,
    })
}

// ──────────────────────────────────────────────
// Private helpers
// ──────────────────────────────────────────────

fn build_prompt(text: &str, customer_lookup_summary: &str) -> String {
    format!(
        r#"Eres un clasificador rápido de mensajes de WhatsApp para un ISP venezolano.

Mensaje del cliente: "{text}"
Cliente: {customer_lookup_summary}

Clasificá la INTENCIÓN en UNA sola etiqueta:
- Spam: cadenas, publicidad ajena, basura
- GreetingOnly: solo saludo, emoji, sticker text, "hola", "👍"
- ClearVentas: pregunta de planes/precios/contratar
- ClearPagos: pago, factura, deuda, comprobante
- ClearSoporte: problema técnico, no anda, lento
- Ambiguous: mezcla de temas O intención no clara

Responde SOLO con JSON estricto, sin markdown:
{{"result":"<etiqueta>","confidence":<0.0-1.0>,"reasoning":"<≤50 chars>"}}
"#
    )
}

fn strip_json_fence(s: &str) -> String {
    let t = s.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}

// ──────────────────────────────────────────────
// Unit tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_json_fence_with_fence() {
        let input = "```json\n{\"result\":\"Spam\"}\n```";
        assert_eq!(strip_json_fence(input), "{\"result\":\"Spam\"}");
    }

    #[test]
    fn strip_json_fence_no_fence() {
        let input = "{\"result\":\"Ambiguous\"}";
        assert_eq!(strip_json_fence(input), "{\"result\":\"Ambiguous\"}");
    }

    #[test]
    fn strip_json_fence_plain_fence() {
        let input = "```\n{\"result\":\"Spam\"}\n```";
        assert_eq!(strip_json_fence(input), "{\"result\":\"Spam\"}");
    }

    #[test]
    fn from_str_known_variants() {
        assert_eq!(PreClassResult::from_str("Spam"), PreClassResult::Spam);
        assert_eq!(PreClassResult::from_str("GreetingOnly"), PreClassResult::GreetingOnly);
        assert_eq!(PreClassResult::from_str("ClearVentas"), PreClassResult::ClearVentas);
        assert_eq!(PreClassResult::from_str("ClearPagos"), PreClassResult::ClearPagos);
        assert_eq!(PreClassResult::from_str("ClearSoporte"), PreClassResult::ClearSoporte);
        assert_eq!(PreClassResult::from_str("Ambiguous"), PreClassResult::Ambiguous);
    }

    #[test]
    fn from_str_unknown_variant() {
        assert_eq!(PreClassResult::from_str("Unknown"), PreClassResult::Ambiguous);
        assert_eq!(PreClassResult::from_str(""), PreClassResult::Ambiguous);
        assert_eq!(PreClassResult::from_str("spam"), PreClassResult::Ambiguous); // case-sensitive
    }
}
