# Design — AI Agent Phase 3a (Pre-classifier + Metrics)

**Change ID**: `ai-agent-phase3a-preclassifier-metrics`
**Date**: 2026-05-04
**Status**: Designed
**Author**: SDD design phase

This document is the **HOW** at architecture level for the proposal. Tasks (the WHAT-to-do steps) are produced in a separate phase.

---

## 1. Architectural Decision Records (ADRs)

### ADR-1 — Pre-classifier insertion point in `dispatch.rs`

**Context.** `dispatch.rs::run_dispatch` already runs several gates before the LLM call:
1. Conversation lookup + workspace resolution.
2. `wa_settings` load (already in scope at the relevant point).
3. Schedule check + agent enabled check (`select_agent` / per-agent gating).
4. Keyword escalation (line ~447): substring match over `agent.escalation.keywords`. **Free** (string compare).
5. Customer pre-lookup by phone (line ~470). **Cheap** (one Mongo round-trip).
6. `select_agent` + `run_turn` against Sofía or specialist.

**Decision.** The pre-classifier gate goes **after step 5 (customer pre-lookup), AFTER keyword escalation, BEFORE `select_agent`/`run_turn`**.

Concrete location: after `build_customer_context(...)` returns (line ~471), before `build_prompt_variables(...)` (line ~473). This is what the proposal called "~line 464" — the keyword escalation lives at 447–464, so the gate goes immediately after the customer pre-lookup at 470–471.

**Rationale.**
- Keyword escalation is **free and deterministic** (string compare). It MUST run first — paying for an LLM call when a deterministic gate can already decide is wasteful.
- Customer pre-lookup must run first too: the pre-classifier's prompt benefits from `customer_summary_short` (knowing whether the speaker is an existing client changes how `Spam` vs `ClearVentas` are interpreted). One Mongo round-trip costs ~2 ms; an LLM call without that context degrades classification quality.
- Pre-classifier runs **after** schedule/active-agent gates because if no agent answers anyway, classifying is wasted spend.
- The gate runs **before** `select_agent` so a `Clear*` result can REPLACE the active agent before invoking `run_turn`.

**Rejected alternatives.**
- *Before keyword escalation.* Rejected — would always pay the LLM cost even when a free gate would resolve.
- *Inside `select_agent`.* Rejected — couples pre-classification with agent resolution. Pre-classifier may decide to short-circuit entirely (Spam/Greeting) without needing an agent at all.
- *After `select_agent` but before `run_turn`.* Rejected — wastes the chance to skip Sofía. The whole point of `Clear*` is to bypass the receptionist, so the gate must run before the receptionist is selected.

---

### ADR-2 — Pre-classifier model: `gemini-2.5-flash-lite` (hardcoded)

**Decision.** Hardcode `gemini-2.5-flash-lite` as the pre-classifier model in v1. No per-workspace override.

**Rationale.**
- Smallest, fastest, cheapest model in the Gemini family with structured output support.
- Per-workspace model override is YAGNI. Cost differential between flash-lite and flash for a 250-token prompt is two orders of magnitude — no real-world workspace would want a different model here.
- If a workspace needs to disable the pre-classifier entirely, `WaSettings.pre_classifier_enabled = false` already covers that.

**Rejected.**
- *Per-workspace `pre_classifier_model_id` field.* Rejected — adds config surface, UI complexity, validation cost. Defer until someone asks. None will.

---

### ADR-3 — Pre-classifier prompt + structured-JSON enforcement

**Decision.** Use a fixed Spanish prompt (~250 tokens) and request strict JSON via Gemini's `response_mime_type: "application/json"` + `response_schema`. Parse defensively; on parse failure or missing fields, fall back to `Ambiguous`.

**Prompt template (final):**
```
Eres un clasificador rápido de mensajes de WhatsApp para un ISP venezolano.

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
{"result":"<etiqueta>","confidence":<0.0-1.0>,"reasoning":"<≤50 chars>"}
```

**Customer summary truncation.** `customer_lookup_summary` is built from `build_customer_context`'s output but truncated to one line (`name | status | balance`) to keep prompt cost predictable. If no match: `"sin match en DB"`.

**Confidence gate.** Inside `pre_classifier::classify`, after parsing:
- `raw.confidence < 0.85` → `gated_variant = Ambiguous` regardless of `raw.result`
- `raw.result` not in the known enum → `Ambiguous`

The raw value is preserved separately in `PreClassResultFull.variant` for audit/logging; `gated_variant` is the one dispatch consumes.

**Rationale.**
- Structured output via `response_mime_type` reduces parse failures by ~95 % (Gemini docs). Defensive parser is the safety net.
- 0.85 threshold matches the proposal's success criteria. Conservative on purpose: false positives are costlier than fall-through.
- Spanish prompt because the project audience (Venezuelan ISP) writes in Spanish; English instructions on Spanish input are documented to perform worse on Gemini Flash Lite.

**Rejected.**
- *Asking Gemini to return free-form text + parsing it.* Rejected — fragile; a single rephrase from Gemini breaks dispatch.
- *Tools/function-calling for the pre-classifier.* Rejected — overkill for a single boolean-style decision; latency penalty.

---

### ADR-4 — Trivial-response template matching

**Decision.** Substring match on `normalize_zone(text)` (existing helper in `tools.rs`) against `normalize_zone(trigger)` for each trigger. Selection rule:
1. Filter `responses` by `enabled == true` AND `kind == requested_kind`.
2. Filter further: keep template if `triggers.is_empty()` (= fallback for that kind) OR any trigger normalized substring-matches the normalized text.
3. Sort by `priority` descending.
4. On equal priority, preserve declaration order (Rust's `sort_by` is stable).
5. Return first.

**Rationale.**
- `normalize_zone` already strips Spanish accents + lowercases — the right primitive (proposal locks substring + no regex per Q9).
- Empty `triggers` = fallback for that `kind` — handy for "always send X if pre-classifier says Spam and nothing more specific matched".
- `priority` lets SUPERADMIN order overlapping templates (e.g. "hola buenas tardes" should beat "hola" if both are present).
- Stable sort = predictable behavior for SUPERADMIN.

**Rejected.**
- *Regex.* Rejected per locked Q9 + ReDoS risk.
- *First-match-wins (no priority).* Rejected — order in DB is not a UI concept; admins shouldn't have to reorder rows.

---

### ADR-5 — `Clear*` → target-agent resolution

**Investigation result.** `AiAgent` (in `src/models/ai_agent.rs`) has these relevant fields today:
- `label: String` — human-readable name ("Soporte", "Pagos", "Recepcionista").
- `description: String` — informational text.
- `is_receptionist: bool` — already in the schema.
- `workspace_ids: Vec<ObjectId>` — agents this agent serves.
- `enabled: bool`, `mode: AiAgentMode`.

There is **no** `purpose` / specialty enum field today. `is_receptionist` covers Sofía but doesn't enumerate Ventas/Pagos/Soporte.

**Decision.** Add a new optional field `purpose: Option<AiAgentPurpose>` on `AiAgent` and use it for Clear-result routing.

```rust
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AiAgentPurpose {
    Recepcionista,
    Ventas,
    Pagos,
    Soporte,
}
```

- Optional (`Option<...>`) so legacy agents (no `purpose` set) continue to work — they just never get auto-selected by `Clear*`.
- `is_receptionist` stays as-is (do NOT remove it, do NOT migrate). It's used by `find_receptionist_for_workspace` and is orthogonal — a Recepcionista agent can have `is_receptionist=true` AND `purpose=Recepcionista`. Existing `find_receptionist_for_workspace` keeps using `is_receptionist`; this change doesn't touch that path.
- Mapping (consumed by dispatch only):
  - `ClearVentas` → `purpose = Ventas`
  - `ClearPagos`  → `purpose = Pagos`
  - `ClearSoporte` → `purpose = Soporte`

**New repository method** (in `AiAgentRepository`):

```rust
async fn find_active_agent_by_workspace_and_purpose(
    &self,
    workspace_id: &ObjectId,
    purpose: AiAgentPurpose,
) -> Result<Option<AiAgent>, String>;
```

Mongo query:
```js
{ workspace_ids: <oid>, enabled: true, purpose: "<snake_case>" }
```
Sort: `created_at: 1` (oldest wins, consistent with `find_active_agent_for_workspace`). Index on `(workspace_ids, enabled, purpose)` is **NOT** needed in v1 — `AiAgents` is a tiny collection (one doc per agent, dozens of docs project-wide). Existing `workspace_ids` multikey index covers it.

**Fall-through behavior.**
- Not found → log at `warn` level (`[ai_agent.dispatch] no specialist agent found for purpose=...; falling through to select_agent`) and continue with the existing `select_agent` flow (Sofía or fallback).
- **Important**: the `AiInteraction` for the pre-classifier call is STILL persisted (we paid for it), but `pre_class_result = "ClearVentas"` (or whichever) and dispatch continues. No double-billing concern: the subsequent `run_turn` writes its own separate `AiInteraction`.

**Rejected alternatives.**
- *Re-use `is_receptionist`.* Doesn't carry enough info (only one specialty marker).
- *Hardcode mapping by `label` substring match (e.g. label contains "Ventas").* Rejected — fragile, locale-dependent, breaks when label is renamed.
- *Add a `tags: Vec<String>` field.* Rejected — overgeneralized for this use case; we want explicit enum semantics.

---

### ADR-6 — Trivial-response when pre-classifier matches but no template found

**Decision.** Differentiated fall-through per variant:

| Variant | No template → action |
|---|---|
| `Spam` | Silent drop. Persist `AiInteraction { pre_classified: true, pre_class_result: "Spam", response_text: None, ... }`. Return early. |
| `GreetingOnly` | **Fall through** to `select_agent` + `run_turn`. Persist `AiInteraction { pre_classified: true, pre_class_result: "GreetingOnly" }` and let the normal flow handle it. |
| `ClearVentas` / `ClearPagos` / `ClearSoporte` | Fall through to `select_agent` (no specialist found) — no trivial template lookup needed for Clear* (they go to the agent, not a template). |
| `Ambiguous` | Fall through. (Same as today's behavior.) |

**Rationale.**
- Spam silent-drop is the whole point: if SUPERADMIN didn't configure a spam template, dropping is the safer default (responding to spam encourages more spam).
- Greetings deserve a human-feeling reply. If the SUPERADMIN didn't configure a greeting template, falling through to Sofía is better than ghosting the customer.
- Clear* never consumes trivial templates by design — the value is routing, not templating.

---

### ADR-7 — Cost-rate constants (single source of truth)

**Decision.** Define per-1M-token rates as `const` blocks in `src/modules/ai_agent/gemini.rs`, organized by model family. Replace the existing `estimate_cost_usd` with a richer signature that takes `cached_tokens` + `thinking_tokens`.

```rust
// Rates per 1M tokens (USD), as of 2026-05.
// Source: https://ai.google.dev/pricing
pub struct ModelRates {
    pub input_per_m: f64,
    pub output_per_m: f64,
    /// Implicit + explicit cache hit rate (typically 25 % of input rate).
    pub cached_input_per_m: f64,
}

const RATES_FLASH:       ModelRates = ModelRates { input_per_m: 0.30,  output_per_m: 2.50, cached_input_per_m: 0.075 };
const RATES_FLASH_LITE:  ModelRates = ModelRates { input_per_m: 0.10,  output_per_m: 0.40, cached_input_per_m: 0.025 };
const RATES_PRO:         ModelRates = ModelRates { input_per_m: 1.25,  output_per_m: 10.00, cached_input_per_m: 0.3125 };
const RATES_DEFAULT:     ModelRates = RATES_FLASH; // safety fallback for unknown models

pub fn rate_for_model(model_id: &str) -> ModelRates {
    let m = model_id.to_lowercase();
    if m.contains("flash-lite") { RATES_FLASH_LITE }
    else if m.contains("flash")  { RATES_FLASH }
    else if m.contains("pro")    { RATES_PRO }
    else                         { RATES_DEFAULT }
}
```

**Cost formula.**
```rust
pub fn estimate_cost_usd(
    model_id: &str,
    input_tokens: u32,
    cached_tokens: u32,
    output_tokens: u32,
    thinking_tokens: u32,
) -> f64 {
    let r = rate_for_model(model_id);
    let billable_input = input_tokens.saturating_sub(cached_tokens) as f64;
    let cached         = cached_tokens as f64;
    let output         = output_tokens as f64;
    let thinking       = thinking_tokens as f64;
    (billable_input * r.input_per_m
        + cached       * r.cached_input_per_m
        + output       * r.output_per_m
        + thinking     * r.output_per_m)
        / 1_000_000.0
}
```

**Compatibility shim.** Keep the old 2-arg `estimate_cost_usd(model, input, output)` as a thin wrapper that calls the new function with `cached_tokens = 0` and `thinking_tokens = 0`. Avoids ripple in `runner.rs` until `RunnerOutput.cached_tokens` lands.

**Rationale.**
- Cached input is billed at ~25 % of the regular input rate (Google pricing 2025+). Implicit cache surfaces via `cachedContentTokenCount`; we have to subtract from the billable input or we'd double-count.
- Thinking tokens are billed at the output rate (per Google pricing for thinking models — confirmed in the existing `gemini.rs` comment around `thinking_budget`).
- One source of truth means the metrics endpoint and the per-turn `cost_usd_estimate` always agree.

**Rejected.**
- *Per-model exact `match` arm.* Rejected — fragile when Google adds new revisions ("gemini-2.5-flash-001"); substring family detection covers more cases.
- *Read rates from DB or env.* Rejected — operational complexity for a slow-changing constant; PR description flags quarterly review.

---

### ADR-8 — Per-model rate-table fallback

**Decision.** Unknown `model_id` → return `RATES_DEFAULT` (= `RATES_FLASH`). Log at `debug` level, not `warn` (we expect the table to lag Google's launches, no need to spam logs).

**Rationale.**
- Cost is a best-effort estimate — billing comes from Google Console, never from this number.
- Defaulting to `flash` overestimates `flash-lite` cost (safe direction) and underestimates `pro` (acceptable; pro shouldn't be the pre-classifier).
- The handler exposes `cost_usd_estimate` as documented "estimate", not authoritative billing.

---

### ADR-9 — Metrics aggregate strategy

**Decision.** Single MongoDB aggregate per request, parameterized by `granularity: Summary | Daily`.

#### Summary pipeline
```js
[
  { $match: { agent_id: <oid>, created_at: { $gte: from, $lte: to } } },
  { $group: {
      _id: null,
      total_turns:           { $sum: 1 },
      total_input_tokens:    { $sum: { $ifNull: ["$input_tokens", 0] } },
      total_output_tokens:   { $sum: { $ifNull: ["$output_tokens", 0] } },
      total_thinking_tokens: { $sum: { $ifNull: ["$thinking_tokens", 0] } },
      total_cached_tokens:   { $sum: { $ifNull: ["$cached_tokens", 0] } },
      total_cost_usd:        { $sum: { $ifNull: ["$cost_usd_estimate", 0.0] } },
      avg_latency_ms:        { $avg: { $ifNull: ["$latency_ms", 0] } },
      pre_classified_count:  { $sum: { $cond: [{ $eq: ["$pre_classified", true] }, 1, 0] } },
      escalated_count:       { $sum: { $cond: [{ $eq: ["$escalated", true] }, 1, 0] } },
      tool_calls_count:      { $sum: { $size: { $ifNull: ["$tool_calls", []] } } },
  } },
]
```

`$ifNull` is required because legacy `AiInteractions` docs (Phase 1 / 2 era) don't have the new fields. Without `$ifNull`, `$sum` returns `null` and breaks downstream parsing.

#### Pre-class breakdown (sub-aggregate or `$facet`)
Run a second pipeline in parallel (`tokio::join!`):
```js
[
  { $match: { agent_id: <oid>, created_at: { $gte: from, $lte: to }, pre_classified: true } },
  { $group: { _id: "$pre_class_result", count: { $sum: 1 } } },
]
```
Result shape: `{ "Spam": 12, "GreetingOnly": 45, ... }`. Missing variants = 0 in the response (handler fills the keys).

**Why two parallel pipelines instead of `$facet`.** `$facet` runs in a single document context and forces all stages to fit in 100 MB of RAM. For metrics over months of data this can OOM. Two parallel `$match` queries each hit the `(agent_id, created_at)` compound index independently — cheaper and bounded.

#### Daily pipeline (when `granularity == Daily`)
```js
[
  { $match: { agent_id: <oid>, created_at: { $gte: from, $lte: to } } },
  { $group: {
      _id: { $dateToString: { format: "%Y-%m-%d", date: "$created_at", timezone: "America/Caracas" } },
      total_turns: { $sum: 1 },
      total_input_tokens:    { $sum: { $ifNull: ["$input_tokens", 0] } },
      total_output_tokens:   { $sum: { $ifNull: ["$output_tokens", 0] } },
      total_thinking_tokens: { $sum: { $ifNull: ["$thinking_tokens", 0] } },
      total_cached_tokens:   { $sum: { $ifNull: ["$cached_tokens", 0] } },
      total_cost_usd:        { $sum: { $ifNull: ["$cost_usd_estimate", 0.0] } },
      pre_classified_count:  { $sum: { $cond: [{ $eq: ["$pre_classified", true] }, 1, 0] } },
      escalated_count:       { $sum: { $cond: [{ $eq: ["$escalated", true] }, 1, 0] } },
  } },
  { $sort: { _id: 1 } },
]
```

**Timezone note.** `America/Caracas` per project locale. Same timezone used elsewhere (`utils::timezone::VenezuelaDateTime`).

**Rationale.**
- Single `$match` + `$group` is the standard MongoDB metric pattern; the existing `audit_messages_by_day` in `whatsapp.rs` (line 1550) is the precedent.
- Per-day is bounded by the input range (caller specifies `from`/`to`); no risk of unbounded result set.
- Compound index `(agent_id: 1, created_at: -1)` from `scripts/create_indexes.js` covers both filters.

**Rejected.**
- *Per-hour granularity.* YAGNI. If demand emerges, add `Hourly` later — same shape.
- *Caching aggregate results in Redis.* Rejected — metrics queries are expected to be infrequent (admin UI), and stale data is worse than slow data here.

---

## 2. Code-level design

### 2.1 New module — `src/modules/ai_agent/pre_classifier.rs` (~150 LOC)

```rust
//! Pre-classifier (Phase 3a): un único roundtrip a gemini-2.5-flash-lite que
//! decide si el turno es trivial (Spam / GreetingOnly), routeable directo a
//! un especialista (Clear*), o ambiguo (cae al flujo normal).
//!
//! No usa tools, no usa history — solo el último mensaje + un summary corto
//! del cliente. Latencia objetivo: < 500 ms p95.

use serde::{Deserialize, Serialize};
use std::time::Instant;

use super::gemini::{
    self, AiRelay, Content, GenerateContentRequest, GenerationConfig, Part, SystemInstruction,
};

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
    pub fn as_str(&self) -> &'static str { /* match */ }
    pub fn from_str(s: &str) -> Self { /* match → Ambiguous on unknown */ }
}

#[derive(Debug, Deserialize)]
struct PreClassRaw {
    pub result: String,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub reasoning: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PreClassTokens {
    pub input: u32,
    pub output: u32,
}

#[derive(Debug, Clone)]
pub struct PreClassResultFull {
    /// Variant directly from Gemini (or `Ambiguous` on parse failure).
    pub variant: PreClassResult,
    /// Same as `variant`, but coerced to `Ambiguous` if `confidence < 0.85`.
    /// This is the value `dispatch.rs` consumes for routing decisions.
    pub gated_variant: PreClassResult,
    pub confidence: f32,
    #[allow(dead_code)]
    pub reasoning: String,
    pub tokens: PreClassTokens,
    pub latency_ms: u32,
}

pub struct PreClassifierContext<'a> {
    pub api_key: &'a str,
    pub relay: Option<&'a AiRelay>,
    pub base_url_override: Option<&'a str>,
    pub http: &'a reqwest::Client,
}

const PRE_CLASS_MODEL_ID: &str = "gemini-2.5-flash-lite";
const PRE_CLASS_TIMEOUT_SECONDS: u32 = 10;
const PRE_CLASS_CONFIDENCE_THRESHOLD: f32 = 0.85;

pub async fn classify(
    text: &str,
    customer_lookup_summary: &str,
    ctx: &PreClassifierContext<'_>,
) -> Result<PreClassResultFull, String> {
    let started = Instant::now();
    let prompt = build_prompt(text, customer_lookup_summary);

    let body = GenerateContentRequest {
        system_instruction: Some(SystemInstruction { parts: vec![Part::text(prompt)] }),
        contents: vec![Content {
            role: "user".into(),
            parts: vec![Part::text(text)],
        }],
        tools: None,
        generation_config: Some(GenerationConfig {
            temperature: Some(0.0),
            max_output_tokens: Some(80),
            thinking_config: Some(super::gemini::ThinkingConfig { thinking_budget: 0 }),
        }),
    };
    // NOTE: response_mime_type / response_schema are added inline as serde-skipped
    // optional fields on GenerationConfig in this same change (see §2.5).

    let resp = gemini::generate_content(
        ctx.http, ctx.api_key, PRE_CLASS_MODEL_ID,
        PRE_CLASS_TIMEOUT_SECONDS, &body, ctx.relay, ctx.base_url_override,
    ).await.map_err(|e| format!("{:?}", e))?;

    let usage = resp.usage_metadata.unwrap_or_default();
    let tokens = PreClassTokens {
        input: usage.prompt_token_count,
        output: usage.candidates_token_count,
    };

    let raw_text = resp.candidates.into_iter().next()
        .and_then(|c| c.content.parts.into_iter().find_map(|p| p.text))
        .unwrap_or_default();

    // Defensive: strip leading/trailing whitespace and any markdown fence.
    let cleaned = strip_json_fence(&raw_text);
    let parsed: PreClassRaw = serde_json::from_str(&cleaned)
        .unwrap_or(PreClassRaw {
            result: "Ambiguous".to_string(),
            confidence: 0.0,
            reasoning: "parse_error".into(),
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
"#)
}

fn strip_json_fence(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}
```

### 2.2 Dispatch wiring (`dispatch.rs`)

Insertion point: between line 471 (`build_customer_context`) and line 473 (`build_prompt_variables`).

```rust
let (customer_context, customer_first_name) =
    build_customer_context(&state, &conv.phone).await;

// ── Pre-classifier gate (Phase 3a) ────────────────────────────────────
let user_text_trimmed = user_text.trim();
let pre_class = if wa_settings.pre_classifier_enabled && !user_text_trimmed.is_empty() {
    let summary_short = build_customer_summary_short(&customer_context);
    let relay_owned_pc = AiRelay::from_config(&state.config);
    let api_key_pc = decrypt_api_key(&agent, &ai_agent_secret()).ok();
    if let Some(key) = api_key_pc.as_deref() {
        let ctx = pre_classifier::PreClassifierContext {
            api_key: key,
            relay: relay_owned_pc.as_ref(),
            base_url_override: agent.model.endpoint_override.as_deref(),
            http: &state.reqwest_client,
        };
        match pre_classifier::classify(user_text_trimmed, &summary_short, &ctx).await {
            Ok(out) => Some(out),
            Err(e) => {
                tracing::warn!("[ai_agent.dispatch] pre_classifier error (conv={}): {}", conv_hex, e);
                None
            }
        }
    } else { None }
} else { None };

let mut active_agent: AiAgent = agent;       // mutable so Clear* can replace
let mut pre_class_consumed_turn = false;     // true if we short-circuit

if let Some(out) = pre_class.as_ref() {
    use pre_classifier::PreClassResult::*;
    match out.gated_variant {
        Spam => {
            let template = pick_trivial(&wa_settings.trivial_responses, "spam", &normalize_zone(user_text_trimmed));
            if let Some(t) = template {
                send_outbound(&state, &conv, &t.response, /*assigned ai_user*/).await?;
            } // else: silent drop
            persist_pre_class_only_interaction(&state, &conv, &active_agent, out, "Spam").await;
            return Ok(());
        }
        GreetingOnly => {
            let template = pick_trivial(&wa_settings.trivial_responses, "greeting", &normalize_zone(user_text_trimmed));
            if let Some(t) = template {
                send_outbound(&state, &conv, &t.response, /*ai*/).await?;
                persist_pre_class_only_interaction(&state, &conv, &active_agent, out, "GreetingOnly").await;
                return Ok(());
            }
            // No greeting template → fall through. Persist after run_turn merges.
        }
        ClearVentas | ClearPagos | ClearSoporte => {
            let purpose = match out.gated_variant {
                ClearVentas => AiAgentPurpose::Ventas,
                ClearPagos => AiAgentPurpose::Pagos,
                ClearSoporte => AiAgentPurpose::Soporte,
                _ => unreachable!(),
            };
            match state.db.find_active_agent_by_workspace_and_purpose(&workspace_id, purpose).await {
                Ok(Some(target)) => {
                    tracing::info!(
                        "[ai_agent.dispatch] pre_classifier routed {} → agent {} (purpose={:?})",
                        out.gated_variant.as_str(), target.id.unwrap().to_hex(), purpose,
                    );
                    active_agent = target;
                }
                Ok(None) => tracing::warn!(
                    "[ai_agent.dispatch] no specialist agent for purpose={:?}, falling through",
                    purpose,
                ),
                Err(e) => tracing::warn!("[ai_agent.dispatch] specialist lookup error: {}", e),
            }
        }
        Ambiguous => { /* fall through */ }
    }
}

// ── (existing flow continues) ─────────────────────────────────────────
let prompt_vars = build_prompt_variables(&active_agent, &wa_settings, &conv, customer_first_name.as_deref());
// ...
```

**`pre_class_consumed_turn` field-passing.** When the pre-classifier did NOT short-circuit but DID run, the existing `to_interaction()` call must include the pre-class data. Two integration points:

1. **Short-circuit (Spam silent drop, Spam template, Greeting template).** Build the `AiInteraction` directly via `persist_pre_class_only_interaction(...)` — a small helper that creates an `AiInteraction` with `pre_classified=true`, `pre_class_result=Some(variant)`, `input_tokens=out.tokens.input`, `output_tokens=out.tokens.output`, `cost_usd_estimate=estimate_cost_usd("gemini-2.5-flash-lite", ...)`, no tool calls, no response_text (or the template text). Persisted via `state.db.insert_ai_interaction(...)` (existing method).

2. **Fall-through (Greeting no-template, Clear* with target found, Clear* no target, Ambiguous).** Carry `pre_class.as_ref()` into the same scope as `runner_output.to_interaction(...)`. Extend `to_interaction()` to take an optional `pre_class: Option<&PreClassResultFull>` parameter and merge:
   - `pre_classified = pre_class.is_some()`
   - `pre_class_result = pre_class.map(|p| p.variant.as_str().to_string())`
   - `input_tokens += pre_class.map_or(0, |p| p.tokens.input)`  (combined with run_turn input)
   - `output_tokens += pre_class.map_or(0, |p| p.tokens.output)` (combined)
   - `cost_usd_estimate += pre_classifier_cost`

**Rationale for combining tokens vs separate row.** A single `AiInteraction` row per inbound turn keeps metrics aggregates simple (count = turns, sum tokens = total spend). Separate rows would force a "type" discriminator in every aggregate. The `pre_classified=true` flag + `pre_class_result` give us the breakdown we need.

### 2.3 Helpers added in `dispatch.rs`

```rust
/// One-line summary for the pre-classifier prompt. Called only when
/// pre_classifier is enabled — keeps dispatch path lean otherwise.
fn build_customer_summary_short(customer_context: &Option<String>) -> String {
    match customer_context {
        Some(ctx) if ctx.contains("matches: 0") || !ctx.contains("matches:") => "sin match en DB".into(),
        Some(ctx) => {
            // Extract first match line: "  - [1] client_id: ... | name: X | ... | status: Y | balance: Z"
            ctx.lines().find(|l| l.starts_with("  - [1]"))
               .map(|l| l.trim().to_string())
               .unwrap_or_else(|| "sin match en DB".into())
        }
        None => "sin match en DB".into(),
    }
}

fn pick_trivial<'a>(
    responses: &'a [TrivialResponse],
    kind: &str,
    text_normalized: &str,
) -> Option<&'a TrivialResponse> {
    let mut candidates: Vec<&TrivialResponse> = responses.iter()
        .filter(|t| t.enabled && t.kind == kind)
        .filter(|t| t.triggers.is_empty() || t.triggers.iter().any(|tr| {
            text_normalized.contains(&normalize_zone(tr))
        }))
        .collect();
    candidates.sort_by(|a, b| b.priority.cmp(&a.priority));
    candidates.first().copied()
}
```

`pick_trivial` lives in `dispatch.rs` (close to its only caller) — extracting to a separate module would be premature.

### 2.4 `UsageMetadata` extension (`gemini.rs`)

```rust
#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    #[serde(default)]
    #[allow(dead_code)]
    pub total_token_count: u32,
    #[serde(default)]
    pub thoughts_token_count: u32,
    /// NEW — Phase 3a. Tokens served from Gemini's implicit/explicit context cache.
    /// Absent when no cache hit; `#[serde(default)]` → 0.
    #[serde(default)]
    pub cached_content_token_count: u32,
}
```

### 2.5 `GenerationConfig` structured-output additions (`gemini.rs`)

```rust
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
    /// NEW — Phase 3a. When set to "application/json", Gemini coerces output
    /// to valid JSON. Used by pre-classifier; runner doesn't set it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    /// NEW — Phase 3a. Optional JSON schema enforcing the output shape.
    /// Pre-classifier provides a 3-field schema; runner leaves it `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<serde_json::Value>,
}
```

The runner constructs `GenerationConfig` with the new fields as `None`; no behavior change.

### 2.6 `RunnerOutput` extension (`runner.rs`)

```rust
pub struct RunnerOutput {
    // ...existing fields...
    /// NEW — Phase 3a. Tokens served from Gemini's implicit/explicit cache.
    pub cached_tokens: u32,
}
```

Populated inside `run_turn`'s loop:
```rust
let mut total_cached: u32 = 0;
// ...
let usage = resp.usage_metadata.unwrap_or(UsageMetadata::default());
total_in       = total_in.saturating_add(usage.prompt_token_count);
total_out      = total_out.saturating_add(usage.candidates_token_count);
total_thinking = total_thinking.saturating_add(usage.thoughts_token_count);
total_cached   = total_cached.saturating_add(usage.cached_content_token_count); // NEW
```

`RunnerOutput` constructor at the bottom adds `cached_tokens: total_cached`. `cost_usd_estimate` switches to the new 5-arg formula.

### 2.7 `AiInteraction` extensions (`models/ai_agent.rs`)

```rust
pub struct AiInteraction {
    // ...existing fields...
    #[serde(default)]
    pub thinking_tokens: u32,
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub pre_classified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_class_result: Option<String>,
}
```

`#[serde(default)]` on every new field is non-negotiable: legacy docs from Phase 1/2 have none of these.

### 2.8 `RunnerOutput::to_interaction(...)` signature update

```rust
pub fn to_interaction(
    &self,
    conversation_id: ObjectId,
    message_id: ObjectId,
    workspace_id: ObjectId,
    agent_id: ObjectId,
    turn_index: u32,
    model_id: &str,
    pre_class: Option<&PreClassResultFull>,   // NEW — Phase 3a
) -> AiInteraction {
    let now = mongodb::bson::DateTime::now();
    let pc_in = pre_class.map_or(0, |p| p.tokens.input);
    let pc_out = pre_class.map_or(0, |p| p.tokens.output);
    let pc_cost = pre_class.map_or(0.0, |p| {
        gemini::estimate_cost_usd("gemini-2.5-flash-lite", p.tokens.input, 0, p.tokens.output, 0)
    });

    AiInteraction {
        id: None,
        conversation_id,
        message_id,
        workspace_id,
        agent_id,
        turn_index,
        model_id: model_id.to_string(),
        input_tokens: self.input_tokens.saturating_add(pc_in),
        output_tokens: self.output_tokens.saturating_add(pc_out),
        thinking_tokens: self.thinking_tokens,
        cached_tokens: self.cached_tokens,
        cost_usd_estimate: self.cost_usd_estimate + pc_cost,
        latency_ms: self.latency_ms.saturating_add(pre_class.map_or(0, |p| p.latency_ms)),
        tool_calls: self.tool_calls.clone(),
        response_text: self.response_text.clone(),
        escalated: self.escalated,
        escalation_reason: self.escalation_reason.clone(),
        pre_classified: pre_class.is_some(),
        pre_class_result: pre_class.map(|p| p.variant.as_str().to_string()),
        created_at: now,
    }
}
```

### 2.9 `WaSettings` extensions (`models/whatsapp.rs`)

```rust
pub struct WaSettings {
    // ...existing fields including enable_guardrails, enable_conversation_state...
    /// Phase 3a. Opt-in pre-classifier (gemini-2.5-flash-lite) before Sofía
    /// gets the turn. Default `false` — admin enables per-workspace from UI.
    #[serde(default)]
    pub pre_classifier_enabled: bool,
    /// Phase 3a. Templates for trivial-response replies (spam, greeting).
    /// Empty = pre-classifier still runs, but Spam silent-drops and
    /// GreetingOnly falls through to Sofía.
    #[serde(default)]
    pub trivial_responses: Vec<TrivialResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TrivialResponse {
    /// UUID. Stable handle for SUPERADMIN UI edits.
    pub id: String,
    /// "spam" | "greeting" | "sticker" — matches the variant the pre-classifier emits.
    pub kind: String,
    /// Substring patterns (case-insensitive, accent-insensitive). Empty = match any text of this kind.
    pub triggers: Vec<String>,
    /// Body sent to the customer via WhatsAppService.
    pub response: String,
    /// Disable without deleting.
    pub enabled: bool,
    /// Higher wins. Default 0. Stable sort preserves declaration order on ties.
    #[serde(default)]
    pub priority: i32,
}
```

`UpdateSettingsRequest` (PATCH body) gains the same two optional fields plus `trivial_responses_set` semantics (replace-all, not merge — keeps the admin UI unambiguous: "your saved list IS the list you submitted").

### 2.10 DB metrics method

```rust
// src/db/mod.rs — inside AiAgentRepository trait
async fn get_ai_agent_metrics(
    &self,
    agent_id: &ObjectId,
    from: bson::DateTime,
    to: bson::DateTime,
    granularity: MetricsGranularity,
) -> Result<AiAgentMetricsRaw, String>;
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsGranularity { Summary, Daily }

#[derive(Debug, Clone)]
pub struct AiAgentMetricsRaw {
    pub summary: AiAgentMetricsSummary,
    pub pre_class_breakdown: HashMap<String, u64>,
    pub daily: Option<Vec<AiAgentMetricsDailyBucket>>,
}

#[derive(Debug, Clone, Default)]
pub struct AiAgentMetricsSummary {
    pub total_turns: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_thinking_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_cost_usd: f64,
    pub avg_latency_ms: f64,
    pub pre_classified_count: u64,
    pub escalated_count: u64,
    pub tool_calls_count: u64,
}
```

The trait returns the raw shape; the handler converts to the response DTO (`AiAgentMetricsResponse`) and stringifies dates.

### 2.11 HTTP handler

```rust
// src/modules/ai_agent/handler.rs

#[utoipa::path(
    get,
    path = "/v1/auth-user/ai-agent/agents/{id}/metrics",
    tag = "AI Agent",
    security(("bearerAuth" = [])),
    params(
        ("id" = String, Path, description = "AiAgent ObjectId hex"),
        ("from" = String, Query, description = "ISO-8601 UTC timestamp inclusive"),
        ("to" = String, Query, description = "ISO-8601 UTC timestamp inclusive"),
        ("granularity" = Option<String>, Query, description = "summary | daily (default summary)"),
    ),
    responses(
        (status = 200, body = AiAgentMetricsResponse),
        (status = 400, description = "Bad request — invalid id, from, to, or granularity"),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "Agent not found"),
    )
)]
pub async fn get_ai_agent_metrics_handler(
    State(state): State<Arc<AppState>>,
    Path(agent_id_hex): Path<String>,
    Query(params): Query<MetricsQueryParams>,
) -> Result<Json<AiAgentMetricsResponse>, ApiError> {
    let agent_id = ObjectId::parse_str(&agent_id_hex).map_err(|_| ApiError::bad_request("invalid_id"))?;
    let from_dt = parse_iso(&params.from).map_err(|_| ApiError::bad_request("invalid_from"))?;
    let to_dt = parse_iso(&params.to).map_err(|_| ApiError::bad_request("invalid_to"))?;
    if to_dt < from_dt { return Err(ApiError::bad_request("range_inverted")); }

    let granularity = match params.granularity.as_deref() {
        Some("daily") => MetricsGranularity::Daily,
        Some("summary") | None => MetricsGranularity::Summary,
        Some(_) => return Err(ApiError::bad_request("invalid_granularity")),
    };

    // 404 if agent doesn't exist
    let _agent = state.db.find_ai_agent_by_id(&agent_id).await
        .map_err(|e| ApiError::internal(format!("agent_lookup: {e}")))?
        .ok_or_else(|| ApiError::not_found("agent_not_found"))?;

    let raw = state.db.get_ai_agent_metrics(&agent_id, from_dt.into(), to_dt.into(), granularity).await
        .map_err(|e| ApiError::internal(format!("metrics: {e}")))?;

    Ok(Json(AiAgentMetricsResponse { ok: true, data: raw.into() }))
}

#[derive(Debug, Deserialize)]
pub struct MetricsQueryParams {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub granularity: Option<String>,
}
```

Route registration: under the existing `auth_user_protected` group in `axum_router.rs` (the AI-agent route block):

```rust
.route("/v1/auth-user/ai-agent/agents/:id/metrics",
       get(crate::modules::ai_agent::handler::get_ai_agent_metrics_handler))
```

### 2.12 Index in `scripts/create_indexes.js`

```js
// Phase 3a — metrics aggregate over (agent_id, created_at) range
db.AiInteractions.createIndex(
  { agent_id: 1, created_at: -1 },
  { name: "agent_id_1_created_at_-1" }
);
```

PR description must call out the manual `mongosh < scripts/create_indexes.js` requirement (per CLAUDE.md project convention).

---

## 3. Sequence diagram (text)

```
WhatsApp inbound → POST /v1/webhook/whatsapp
        │
        ▼
debounce / queue (existing)
        │
        ▼
dispatch::run_dispatch
        │
        ├─ load wa_conversation, wa_settings, agent
        ├─ schedule check, agent.enabled check
        ├─ keyword_escalation_check (FREE)            ──┐  hit → escalate, return
        ├─ build_customer_context (cheap, 1 Mongo)     ─┤
        │
        ▼
   ┌────────────────────────────────────────────────┐
   │  PRE-CLASSIFIER GATE (Phase 3a)                │
   │  if wa_settings.pre_classifier_enabled         │
   │     && !user_text.is_empty()                   │
   │     && api_key decrypts                        │
   │  then call pre_classifier::classify(...)       │
   │     model: gemini-2.5-flash-lite               │
   │     timeout: 10s                               │
   │     temperature: 0.0, max_out: 80              │
   └────────────────────────────────────────────────┘
        │
        ▼
   match pre_class.gated_variant {
     Spam ──────────────► pick_trivial(spam):
                            Some(t) → send t.response
                            None    → silent drop
                          persist AiInteraction(pre_classified)
                          return Ok(())
     GreetingOnly ──────► pick_trivial(greeting):
                            Some(t) → send + persist + return
                            None    → fall through (Sofía will reply)
     ClearVentas        ┐
     ClearPagos         ├► find_active_agent_by_workspace_and_purpose:
     ClearSoporte       │     Some(target) → active_agent = target  (skip Sofía)
                        │     None         → keep current active_agent
     Ambiguous ─────────┴► fall through (Sofía answers as today)
   }
        │
        ▼
build_prompt_variables(active_agent, ...)
        │
        ▼
run_turn(active_agent, ...)            ◄── may transfer agents within the chain
        │
        ▼
to_interaction(pre_class.as_ref())     ◄── merges pre-class tokens + cost
        │
        ▼
db.insert_ai_interaction(...)
```

---

## 4. Open questions resolved here

| # | Question | Resolution |
|---|---|---|
| Q1 | Where exactly does the gate go? | Line 471–473 (after customer pre-lookup, before `build_prompt_variables`) — ADR-1 |
| Q2 | Which Gemini model? | `gemini-2.5-flash-lite` hardcoded — ADR-2 |
| Q3 | Structured output enforcement? | `response_mime_type: application/json` + defensive parser — ADR-3 |
| Q4 | Confidence threshold? | 0.85; below → coerce to `Ambiguous` — ADR-3 |
| Q5 | Trigger matching algorithm? | Substring match on normalized text via existing `normalize_zone` — ADR-4 |
| Q6 | Tiebreaker on multiple matching templates? | Higher `priority` wins; ties resolved by stable insertion order — ADR-4 |
| Q7 | How does `Clear*` find the specialist? | New `AiAgent.purpose` enum field + `find_active_agent_by_workspace_and_purpose` repo method — ADR-5 |
| Q8 | What if no specialist exists for that purpose? | Fall through to existing `select_agent`; warn log — ADR-5 |
| Q9 | What if no trivial template matches Spam? | Silent drop, log only — ADR-6 |
| Q10 | What if no trivial template matches GreetingOnly? | Fall through to Sofía — ADR-6 |
| Q11 | Where do cost rates live? | `gemini.rs` constants, family-detected via substring — ADR-7 |
| Q12 | How is cached input billed? | Subtract from billable input, add at 25 % rate — ADR-7 |
| Q13 | One `AiInteraction` per turn or two (pre-class + run)? | One. Combine tokens; `pre_classified=true` + `pre_class_result` discriminate — §2.8 |
| Q14 | Aggregate strategy for breakdown? | Two parallel `$match`+`$group` (avoids `$facet` 100 MB limit) — ADR-9 |
| Q15 | Daily timezone? | `America/Caracas` matching existing `VenezuelaDateTime` — ADR-9 |

---

## 5. Testing approach

Per project policy (`cargo check` clean is the bar; no enforced TDD):

1. **`cargo check` zero warnings** across all touched files.
2. **Unit tests** (small, focused; in same files):
   - `pre_classifier::strip_json_fence` — handles `\`\`\`json ... \`\`\``, no fence, plain JSON.
   - `pre_classifier::PreClassResult::from_str` — known + unknown variants.
   - `pick_trivial` — empty list, no kind match, multiple matches with priority, fallback (empty triggers), case/accent normalization.
   - `gemini::rate_for_model` — flash-lite, flash, pro, unknown.
   - `gemini::estimate_cost_usd` — cached subtract, thinking added at output rate, zero-input.
3. **Integration smoke** (post-deploy, manual):
   1. `pre_classifier_enabled=true` on one workspace; no `trivial_responses` configured.
      - Send "👍" → pre-classifier returns `GreetingOnly`, no template, falls through to Sofía. `AiInteraction.pre_classified=true`, `pre_class_result="GreetingOnly"`.
   2. Same workspace, add `TrivialResponse { kind:"spam", response:"...", triggers:[], enabled:true }`.
      - Send a known-spam message → silent reply with `response`. No Sofía.
   3. Workspace has `purpose=Soporte` agent enabled.
      - Send "no me anda el internet" → pre-classifier `ClearSoporte`, dispatch picks Soporte agent directly. Sofía never invoked. `AiInteraction.agent_id` = Soporte agent.
   4. Same workspace, no `purpose=Ventas` agent.
      - Send "qué planes tienen?" → pre-classifier `ClearVentas`, lookup returns None, falls through to Sofía. Warn log emitted.
   5. Send ambiguous "hola necesito algo" → pre-classifier `Ambiguous`, falls through to Sofía. `pre_classified=true`, `pre_class_result="Ambiguous"`.
   6. Set `pre_classifier_enabled=false` → confirm zero `gemini-2.5-flash-lite` calls in logs across multiple inbounds.
   7. `GET /v1/auth-user/ai-agent/agents/:id/metrics?from=2026-05-04T00:00:00Z&to=2026-05-04T23:59:59Z` → returns aggregate with non-zero `cached_tokens` (validates implicit cache observability).
   8. Same with `?granularity=daily` → returns daily breakdown sorted by date.
   9. `GET .../metrics?granularity=garbage` → 400 `invalid_granularity`.
   10. Read a Phase-1-era `AiInteraction` doc back via the metrics endpoint → no panic, missing fields default to 0.

---

## 6. Affected files (recap)

| File | Change | Approx LOC |
|---|---|---|
| `src/modules/ai_agent/pre_classifier.rs` | NEW | ~150 |
| `src/modules/ai_agent/mod.rs` | `pub mod pre_classifier;` | 1 |
| `src/modules/ai_agent/dispatch.rs` | gate insert, helpers, `to_interaction` wiring | ~80 |
| `src/modules/ai_agent/runner.rs` | `RunnerOutput.cached_tokens`, `to_interaction` signature | ~15 |
| `src/modules/ai_agent/gemini.rs` | `UsageMetadata.cached_content_token_count`, `GenerationConfig.response_mime_type/schema`, rate table, new `estimate_cost_usd` (back-compat shim) | ~70 |
| `src/modules/ai_agent/handler.rs` | metrics handler + DTO conversion | ~80 |
| `src/models/ai_agent.rs` | `AiInteraction` extensions, `AiAgentPurpose`, `AiAgent.purpose` (+DTO) | ~40 |
| `src/models/whatsapp.rs` | `WaSettings.pre_classifier_enabled/trivial_responses`, `TrivialResponse`, PATCH body fields | ~50 |
| `src/db/mod.rs` | `MetricsGranularity`, `AiAgentMetricsRaw`, `get_ai_agent_metrics`, `find_active_agent_by_workspace_and_purpose` | ~30 |
| `src/db/mongo/ai_agent.rs` | aggregate impls + new lookup | ~120 |
| `src/axum_router.rs` | route registration | ~3 |
| `src/openapi.rs` | path + schemas (AiAgentMetricsResponse, TrivialResponse, AiAgentPurpose) | ~20 |
| `scripts/create_indexes.js` | compound index | ~3 |
| `openspec/specs/ai-agent/spec.md` | capability delta (separate spec phase) | n/a |
| **Total** | | **~660 LOC** |

---

## 7. Architectural risks (carried into tasks/apply)

1. **Pre-classifier latency on every text turn.** Mitigated by opt-in default + `pre_classifier_enabled=false` rollback. Worth measuring in production before flipping defaults.
2. **`Clear*` mis-routing on edge cases.** A confidence ≥ 0.85 + clear text could still be wrong (e.g. "no me anda quiero pagar" — both `ClearSoporte` and `ClearPagos` apply). The 0.85 gate + `Ambiguous` fallback should catch most; iterate prompt if production data shows drift.
3. **Combining pre-class tokens into the same `AiInteraction` row.** Means a per-row "this was Sofía-only" filter now requires `pre_classified=false` instead of "row exists". Slight semantic shift — documented in §2.8 and the spec phase.
4. **`AiAgent.purpose` field rollout.** Legacy agents have `None`; SUPERADMIN must explicitly set purpose for routing to take effect. Not enforced in v1 — fall-through to Sofía is the safe default. Communication-only risk; flag in PR description.
5. **`response_mime_type`/`response_schema` may degrade on Gemini Flash Lite.** Defensive parser is the safety net. If parse-failure rate exceeds ~5 % in production logs, drop the schema and rely on the parser alone.
6. **Cost estimate drift.** Documented in ADR-7. Quarterly review noted.

---

## 8. What this design intentionally does NOT decide

These are tasks-phase or apply-phase concerns, not architecture:

- Exact `WaSettings` PATCH validation rules (e.g. dedup `TrivialResponse.id`, max length of `response`).
- OpenAPI schema field-by-field enumeration.
- Spec text in `openspec/specs/ai-agent/spec.md` (separate spec phase).
- Implementation order across the listed files (tasks phase).
- Concrete prompt iteration after first production sample (post-launch tuning).
