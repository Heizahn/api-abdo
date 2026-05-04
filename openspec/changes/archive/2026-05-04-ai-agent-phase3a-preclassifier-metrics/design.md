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

## 7. Architectural risks (carried into tasks/apply)

1. **Pre-classifier latency on every text turn.** Mitigated by opt-in default + `pre_classifier_enabled=false` rollback. Worth measuring in production before flipping defaults.
2. **`Clear*` mis-routing on edge cases.** A confidence ≥ 0.85 + clear text could still be wrong (e.g. "no me anda quiero pagar" — both `ClearSoporte` and `ClearPagos` apply). The 0.85 gate + `Ambiguous` fallback should catch most; iterate prompt if production data shows drift.
3. **Combining pre-class tokens into the same `AiInteraction` row.** Means a per-row "this was Sofía-only" filter now requires `pre_classified=false` instead of "row exists". Slight semantic shift — documented in §2.8 and the spec phase.
4. **`AiAgent.purpose` field rollout.** Legacy agents have `None`; SUPERADMIN must explicitly set purpose for routing to take effect. Not enforced in v1 — fall-through to Sofía is the safe default. Communication-only risk; flag in PR description.
5. **`response_mime_type`/`response_schema` may degrade on Gemini Flash Lite.** Defensive parser is the safety net. If parse-failure rate exceeds ~5 % in production logs, drop the schema and rely on the parser alone.
6. **Cost estimate drift.** Documented in ADR-7. Quarterly review noted.
