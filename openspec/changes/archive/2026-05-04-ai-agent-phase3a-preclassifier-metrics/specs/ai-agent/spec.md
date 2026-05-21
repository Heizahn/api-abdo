# Delta for AI Agent — Phase 3a: Pre-classifier + Metrics

## ADDED Requirements

### Requirement 25: Pre-classifier — opt-in per workspace

`WaSettings` MUST expose `pre_classifier_enabled: bool` (default `false`). When
`false`, dispatch MUST skip the pre-classifier entirely — no Gemini call, no
behavioral change. When `true`, dispatch MUST invoke the pre-classifier AFTER
keyword escalation and BEFORE `select_agent`.

#### Scenario 25.1: Disabled by default — no LLM call

- GIVEN `WaSettings.pre_classifier_enabled = false`
- WHEN dispatch processes any inbound message
- THEN it MUST skip the pre-classifier AND continue with existing flow
- AND no Gemini Flash Lite call MUST be made

#### Scenario 25.2: Opt-in activation — pre-classifier runs first

- GIVEN `WaSettings.pre_classifier_enabled = true` AND the inbound has non-empty text
- WHEN dispatch runs
- THEN it MUST invoke the pre-classifier BEFORE `select_agent` and BEFORE keyword escalation routing

#### Scenario 25.3: Skipped on media-only messages

- GIVEN `pre_classifier_enabled = true` AND the inbound message text is empty or whitespace
- WHEN dispatch runs
- THEN it MUST skip the pre-classifier AND fall through to existing flow

---

### Requirement 26: Pre-classifier output contract

The pre-classifier MUST return a `PreClassResult` enum and an associated
confidence score. Results with `confidence < 0.85` MUST be treated as `Ambiguous`
by dispatch (confidence gate), but the original variant MUST still be persisted
to `AiInteraction.pre_class_result` for audit. The pre-classifier MUST use
`gemini-2.5-flash-lite` (not the main agent model). On Gemini error or
JSON-parse failure, dispatch MUST fall through to existing flow (treat as
`Ambiguous`) and log a warning.

`PreClassResult` variants:

| Variant | Meaning |
|---|---|
| `Spam` | Noise, forwarded chains, advertising |
| `GreetingOnly` | Single-word greeting, lone emoji |
| `ClearVentas` | Explicit contracting / pricing intent |
| `ClearPagos` | Explicit payment intent |
| `ClearSoporte` | Explicit technical support issue |
| `Ambiguous` | Uncertain or mixed intent |

#### Scenario 26.1: High-confidence result used directly

- GIVEN the pre-classifier returns `result = ClearSoporte, confidence = 0.92`
- WHEN dispatch evaluates the result
- THEN it MUST treat the result as `ClearSoporte` and route accordingly

#### Scenario 26.2: Low-confidence forces Ambiguous routing

- GIVEN the pre-classifier returns `result = ClearVentas, confidence = 0.72`
- WHEN dispatch evaluates the result
- THEN it MUST route as `Ambiguous` (fall through to existing flow)
- AND `AiInteraction.pre_class_result` MUST record `"ClearVentas"` (the original variant)

#### Scenario 26.3: JSON parse failure treated as Ambiguous

- GIVEN Gemini responds with malformed JSON or missing fields
- WHEN the pre-classifier parses the response
- THEN dispatch MUST treat the result as `Ambiguous` AND emit a `tracing::warn!`
- AND no agent MUST be short-circuited

#### Scenario 26.4: Required JSON output schema

The pre-classifier prompt MUST instruct Gemini to respond with:
```json
{ "result": "<variant>", "confidence": <0.0–1.0>, "reasoning": "<≤50 chars Spanish>" }
```
- `reasoning` is for audit logs only; it MUST NOT be sent to the customer

---

### Requirement 27: Pre-classifier action mapping

#### Scenario 27.1: Spam → trivial response or silent drop

- GIVEN `result = Spam AND confidence >= 0.85`
- WHEN dispatch evaluates
- THEN it MUST look up an enabled `TrivialResponse` with `kind = "spam"` matching `user_text`
- AND IF a match is found: send the template's `response` text; persist `AiInteraction { pre_classified: true, pre_class_result: "Spam" }`; return early without invoking any agent
- AND IF no match: silently drop (no response); still persist `AiInteraction { pre_classified: true }`

#### Scenario 27.2: GreetingOnly → trivial response, fallback if no match

- GIVEN `result = GreetingOnly AND confidence >= 0.85`
- WHEN dispatch evaluates
- THEN it MUST look up an enabled `TrivialResponse` with `kind = "greeting"`
- AND IF a match is found: send the template; persist `AiInteraction { pre_classified: true }`
- AND IF no match: fall through to existing flow (MUST NOT silently drop greetings)

#### Scenario 27.3: Clear* → bypass receptionist, direct to specialist

- GIVEN `result in [ClearVentas, ClearPagos, ClearSoporte] AND confidence >= 0.85`
- WHEN dispatch evaluates
- THEN it MUST resolve the corresponding specialized agent using the same mechanism as `transfer_to_agent`
- AND IF a target agent is found: invoke `run_turn` directly (skip Sofía); persist `AiInteraction { pre_classified: true, pre_class_result: <variant> }`
- AND IF no target agent found: fall through to existing `select_agent` flow; persist `AiInteraction { pre_classified: false }`

#### Scenario 27.4: Ambiguous → existing flow unchanged

- GIVEN `result = Ambiguous` (directly, via confidence gate, or via parse failure)
- WHEN dispatch evaluates
- THEN it MUST fall through to existing `select_agent` flow
- AND `AiInteraction.pre_classified` MUST be `false`

---

### Requirement 28: TrivialResponse data shape

`WaSettings` MUST embed `trivial_responses: Vec<TrivialResponse>` (default empty,
`#[serde(default)]`). `TrivialResponse` MUST contain:

| Field | Type | Notes |
|---|---|---|
| `id` | `String` | UUID v4 |
| `kind` | `String` | `"spam"` \| `"greeting"` \| `"sticker"` |
| `triggers` | `Vec<String>` | Substring triggers; case-insensitive after `normalize_zone` |
| `response` | `String` | Text sent verbatim to customer |
| `enabled` | `bool` | `false` = soft-disable without deletion |
| `priority` | `i32` | Default `0`; higher wins on tiebreaker |

#### Scenario 28.1: Empty triggers acts as catch-all fallback

- GIVEN a `TrivialResponse` with `enabled = true AND triggers = []`
- WHEN dispatch looks up a trivial for `kind = "X"`
- THEN this entry MUST be selected only if no entry with non-empty triggers matched first

#### Scenario 28.2: Tiebreaker — highest priority, then insertion order

- GIVEN multiple enabled entries with `kind = "X"` whose triggers all match
- WHEN dispatch picks one
- THEN it MUST select the entry with the HIGHEST `priority` value
- AND on priority tie: the FIRST entry in the list (insertion order) wins

---

### Requirement 29: AiInteraction schema extensions

`AiInteraction` MUST add four new fields, all with `#[serde(default)]`:

| Field | Type | Default | Source |
|---|---|---|---|
| `thinking_tokens` | `u32` | `0` | `thoughtsTokenCount` from Gemini `UsageMetadata` |
| `cached_tokens` | `u32` | `0` | `cachedContentTokenCount` from Gemini `UsageMetadata` |
| `pre_classified` | `bool` | `false` | `true` iff pre-classifier short-circuited dispatch |
| `pre_class_result` | `Option<String>` | `None` | Pre-classifier variant name when it ran |

#### Scenario 29.1: Backward compatibility — legacy documents deserialize cleanly

- GIVEN an `AiInteraction` document without the four new fields
- WHEN deserialized
- THEN all four fields MUST take their default values without error
- AND no migration script SHALL be required

#### Scenario 29.2: New document persists all four fields

- GIVEN a turn where the pre-classifier ran and returned `ClearSoporte` with `confidence = 0.90`
- WHEN the `AiInteraction` is persisted
- THEN `pre_classified = true`, `pre_class_result = Some("ClearSoporte")`, `cached_tokens >= 0`, `thinking_tokens >= 0` MUST all be present in the document

---

### Requirement 30: Metrics endpoint

`GET /v1/auth-user/whatsapp/ai-agent/agents/:id/metrics?from=&to=&granularity=` MUST
exist under the `user_jwt_auth_middleware` group (consistent with all other AI Agent
routes which live under `/v1/auth-user/whatsapp/ai-agent/...`).

Query params: `from` and `to` (RFC3339, required); `granularity` (`summary` default | `daily`).

**Summary response shape:**
```json
{
  "ok": true,
  "data": {
    "agent_id": "<hex>", "from": "<RFC3339>", "to": "<RFC3339>",
    "summary": {
      "total_turns": <u64>, "total_input_tokens": <u64>,
      "total_output_tokens": <u64>, "total_thinking_tokens": <u64>,
      "total_cached_tokens": <u64>, "total_cost_usd": <f64>,
      "avg_latency_ms": <f64>, "pre_classified_count": <u64>,
      "pre_classified_breakdown": { "Spam": <u64>, "GreetingOnly": <u64>,
        "ClearVentas": <u64>, "ClearPagos": <u64>,
        "ClearSoporte": <u64>, "Ambiguous": <u64> },
      "escalated_count": <u64>, "tool_calls_count": <u64>,
      "cache_hit_rate": <f64>
    }
  }
}
```
`cache_hit_rate = total_cached_tokens / total_input_tokens` (0 when input is 0).

#### Scenario 30.1: Valid request returns aggregate summary

- GIVEN a valid `agent_id`, parseable `from`/`to`, and at least one matching `AiInteraction`
- WHEN the endpoint is called
- THEN it MUST return HTTP 200 with the summary shape above

#### Scenario 30.2: Invalid agent_id → 400

- GIVEN `id` is not a valid hex ObjectId
- WHEN the endpoint is called
- THEN it MUST return `{ "ok": false, "error": "invalid_agent_id" }`

#### Scenario 30.3: Invalid or missing date params → 400

- GIVEN `from` or `to` cannot be parsed, OR `from > to`
- WHEN the endpoint is called
- THEN it MUST return `{ "ok": false, "error": "invalid_date_range" }`

#### Scenario 30.4: Agent not found → 404

- GIVEN `id` is a valid ObjectId but no `AiAgent` document exists
- WHEN the endpoint is called
- THEN it MUST return `{ "ok": false, "error": "agent_not_found" }`

#### Scenario 30.5: Empty window returns zero summary

- GIVEN no `AiInteraction` documents fall in `[from, to]` for this agent
- WHEN the endpoint is called
- THEN it MUST return HTTP 200 with all `total_*` fields at `0`

#### Scenario 30.6: granularity=daily adds per-day breakdown

- GIVEN `?granularity=daily` is provided
- WHEN the endpoint responds
- THEN the response MUST include `daily_breakdown: Vec<DailyBucket>` at the same level as `summary`
- AND each bucket MUST contain `date: "YYYY-MM-DD"` (Venezuela tz) plus a subset of summary fields
- AND buckets MUST be sorted ascending by date; missing days MAY be omitted

---

### Requirement 31: MongoDB index for AiInteractions

`scripts/create_indexes.js` MUST add a compound index on
`AiInteractions { agent_id: 1, created_at: -1 }`.
The metrics aggregate MUST use this index; a full-collection scan is not acceptable
at production scale (~12 000 interactions/day).

#### Scenario 31.1: Index created by setup script

- GIVEN `mongosh <URI> < scripts/create_indexes.js` is run
- WHEN the command completes
- THEN the collection `AiInteractions` MUST have the compound index `agent_id_1_created_at_-1`
- AND the index MUST NOT already exist check (idempotent `createIndex` call)

---

### Requirement 32: cost_usd_estimate consistency

Per-model cost rate constants MUST live in a single table in `gemini.rs`; they
MUST NOT be duplicated across modules. Pre-classifier interactions (short-circuited
turns) MUST record `cost_usd_estimate` reflecting only the Flash Lite tokens (not
the main agent model). Implicit cache hits (`cached_tokens > 0`) MUST be billed at
25% of the standard input rate (75% discount) in the estimate.

#### Scenario 32.1: Pre-classifier turn cost reflects Flash Lite only

- GIVEN a turn where the pre-classifier ran and returned `Spam` (no main-agent call)
- WHEN `AiInteraction.cost_usd_estimate` is computed
- THEN it MUST reflect Flash Lite's input + output tokens only
- AND the main-agent model's rate MUST NOT be applied

#### Scenario 32.2: Cached tokens billed at 25% input rate

- GIVEN a turn where `cached_tokens = 1000` and `input_tokens = 5000` (4000 non-cached)
- WHEN `cost_usd_estimate` is computed
- THEN cached_tokens MUST be charged at 25% of the standard input rate
- AND non-cached input tokens MUST be charged at the standard input rate
