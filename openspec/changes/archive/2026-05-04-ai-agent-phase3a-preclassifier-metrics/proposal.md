# AI Agent — Phase 3a: Pre-classifier + Metrics Foundation

**Change ID**: `ai-agent-phase3a-preclassifier-metrics`
**Date**: 2026-05-04
**Status**: Proposed
**Phase**: 3a (3b — explicit Gemini cachedContents — deferred pending metrics data)
**Depends on**: `ai-agent-guardrails-and-hud` (Phase 1), `ai-agent-conversation-state` (Phase 2), commit `c9bbe98` (per-workspace WaSettings toggles)

---

## Why

Sofía (the receptionist agent) currently receives **every** inbound WhatsApp turn — including spam stickers, lone "👍" reactions, throwaway "hola"s, and obvious single-intent messages like "no me anda el internet". Each of those calls ships the full ~13 000-token system instruction to Gemini.

At 8 000 clients in production we project ~1 500 conversations/day × ~8 turns each ≈ 12 000 LLM turns/day. Even with Google's automatic implicit caching on `gemini-2.5-flash` (75 % discount applied transparently since 2025), the Sofía-only path still costs roughly **$200–300 / month**. A pre-classifier that short-circuits ~70 % of trivial turns to a templated reply (or routes the obvious ones straight to the specialized agent, skipping Sofía) drops that bill to roughly **$60–80 / month** — a meaningful win for a Venezuelan team where every $100 saved is a real chunk of someone's salary (locked policy `feedback/cost-conscious-scaling`).

Equally important: **we currently fly blind**. Phase 1 and Phase 2 added meaningful complexity (guardrails, conversation state, in-progress locks, daily limits) without any visibility into whether they're working. There is no per-agent dashboard, no token/cost trend, no cache-hit rate, no way to answer "is Sofía actually expensive, or is the long tail of FAQ agents the real cost driver?". Without those numbers we can't justify or tune anything — including whether explicit Gemini context caching (Phase 3b, deferred) is even worth building.

This change addresses both problems together because they share the same instrumentation surface: the `AiInteractions` collection.

### Why now (and not later)
- Phase 1 + 2 just landed — the dispatch flow is freshly understood; piggy-backing on that working memory is cheaper than re-loading it later.
- The `AiInteractions` collection has been populated since Phase 1 but has **no compound `(agent_id, created_at)` index**. At the current rate of inserts, an aggregate-style metrics query at scale would full-scan. Adding the index now is low risk; doing it after months of growth is a maintenance window.
- Per-workspace policy granularity (locked in `feedback/config-in-db-not-env`) was just established by the recent `WaSettings` refactor — extending it once more with two opt-in fields fits cleanly.

### Why split off explicit Gemini context caching (was Phase 3b in the explore)
The exploration verified that `gemini-2.5-flash` already has **automatic implicit caching with the same 75 % discount**, surfaced via `usageMetadata.cachedContentTokenCount`. Building the explicit `cachedContents` API integration now (TTL tracking in Redis, create/delete/renew lifecycle, relay whitelist updates) is non-trivial and may be **redundant**. The right call is: ship the metrics foundation first, observe the implicit cache hit rate in production for 2–4 weeks, and only build explicit caching if the data shows the implicit hit rate is too low or too unreliable. That's a separate change (`ai-agent-phase3b-explicit-cache`), gated on data this proposal makes available.

---

## What changes

### New files
- `src/modules/ai_agent/pre_classifier.rs` (NEW, ~150 LOC) — pre-classifier module: prompt template, `gemini-2.5-flash-lite` call, `PreClassResult` enum, JSON parser with confidence threshold

### Model extensions
- `src/models/whatsapp.rs`
  - `WaSettings.pre_classifier_enabled: bool` (default `false` — opt-in rollout)
  - `WaSettings.trivial_responses: Vec<TrivialResponse>` (default empty)
  - New struct `TrivialResponse { id, kind, triggers, response, enabled }`
- `src/models/ai_agent.rs`
  - `AiInteraction.thinking_tokens: u32` (already in `RunnerOutput`, just not persisted yet)
  - `AiInteraction.cached_tokens: u32` (NEW — populated from Gemini's `cachedContentTokenCount`)
  - `AiInteraction.pre_classified: bool` (NEW — whether this turn was gated by the pre-classifier)
  - `AiInteraction.pre_class_result: Option<String>` (NEW — variant name when pre-classified)
  - `RunnerOutput.cached_tokens: u32` (NEW — propagated from `UsageMetadata`)
  - All new fields use `#[serde(default)]` for read-back compat with legacy docs

### Gemini client
- `src/modules/ai_agent/gemini.rs`
  - `UsageMetadata.cached_content_token_count: Option<u32>` (Gemini API field `cachedContentTokenCount`, only present when implicit/explicit cache hits)
- `src/modules/ai_agent/runner.rs`
  - Extract `cached_content_token_count` from `UsageMetadata` into `RunnerOutput.cached_tokens`

### Dispatch flow
- `src/modules/ai_agent/dispatch.rs`
  - **Pre-classifier gate** inserted after keyword escalation and before `select_agent`/`run_turn` (~line 464 per explore)
  - Skipped entirely if `wa_settings.pre_classifier_enabled == false`
  - Skipped entirely if `user_text.is_empty()` (media-only turns — pure overhead avoided)
  - Result handling:
    - `Spam` / `GreetingOnly` → look up matching `TrivialResponse` template by `kind`, send via `WhatsAppService`, persist `AiInteraction { pre_classified: true, pre_class_result: Some("Spam"), input_tokens: <preclass>, output_tokens: 0, ... }`, return early
    - `ClearVentas` / `ClearPagos` / `ClearSoporte` → resolve target specialized agent (reuse the existing `transfer_to_agent` routing mechanics), invoke directly with that agent, **skip Sofía**
    - `Ambiguous` → fall through to existing `select_agent` + `run_turn` flow
  - `to_interaction()` extended to populate the four new fields

### Persistence layer
- `src/db/mod.rs` — add trait method:
  ```rust
  async fn get_ai_agent_metrics(
      &self,
      agent_id: ObjectId,
      from: DateTime<Utc>,
      to: DateTime<Utc>,
      granularity: MetricsGranularity, // Summary | Daily
  ) -> Result<AiAgentMetrics>;
  ```
- `src/db/mongo/ai_agent.rs` — MongoDB `$match` + `$group` aggregate over `AiInteractions`
- `scripts/create_indexes.js` — compound index `AiInteractions(agent_id: 1, created_at: -1)` (the project's setup is manual via `mongosh < scripts/create_indexes.js` — documented in the project README)

### HTTP surface
- New endpoint: `GET /v1/auth-user/ai-agent/agents/:id/metrics?from=&to=&granularity=`
  - Default response: aggregate summary `{ ok, data: { summary } }`
  - With `?granularity=daily`: `{ ok, data: { summary, daily_breakdown: [...] } }`
  - Auth: `user_jwt_auth_middleware` (staff/admin only, like the rest of `/v1/auth-user/ai-agent/*`)
- `src/openapi.rs` — register new path + new schemas (`AiAgentMetrics`, `AiAgentMetricsDaily`, `MetricsGranularity`)

### Spec delta
- `openspec/specs/ai-agent/spec.md` — capability delta: pre-classifier behavior, trivial-response handling, metrics surface

**Estimated change size**: ~600 LOC new + ~100 LOC modified.

---

## Out of scope

These are explicitly **not** in this change:

- **Explicit Gemini `cachedContents` API** — deferred to `ai-agent-phase3b-explicit-cache`, gated on metrics data from this change. Implicit caching (already automatic on `gemini-2.5-flash`) is observed via the new `cached_tokens` field.
- **Cross-agent global metrics endpoint** (`/ai-agent/metrics` aggregate across all agents in a workspace) — start per-agent only; aggregate later if there's demand.
- **Backfill script** for legacy `AiInteractions` docs missing the new fields — `#[serde(default)]` handles read-side; metrics over old data simply show zero for new fields. Acceptable for a forward-looking metric.
- **Cost calculation refresh** against Gemini's current pricing — separate ops task. The existing `cost_usd_estimate` field is preserved as-is.
- **A/B testing harness** for pre-classifier accuracy — manual smoke testing + iterating the prompt is enough for v1. Confidence threshold + audit log give us enough signal.
- **Pre-classifier shadow mode** (run but don't act) — would be useful for accuracy validation but adds branching complexity. If we end up wanting it, it's a small follow-up.
- **Regex / advanced trigger matching** for `TrivialResponse.triggers` — substring match only. Keeps the admin UI sane and avoids ReDoS.

---

## Capabilities

- **Modified**: `ai-agent` — adds pre-classifier behavior, trivial-response handling, metrics surface

No new capabilities; this extends the existing `ai-agent` capability.

---

## Approach

The implementation order minimizes intermediate broken states and lets each step compile + `cargo check` pass independently.

### Step 1 — Schema-first (zero-risk additions)
1. Extend `WaSettings` with `pre_classifier_enabled: bool` (default `false`) and `trivial_responses: Vec<TrivialResponse>` (default empty), all `#[serde(default)]`.
2. Define `TrivialResponse { id: String /* UUID */, kind: String /* "spam" | "greeting" | "sticker" */, triggers: Vec<String>, response: String, enabled: bool }`.
3. Extend `AiInteraction` with `thinking_tokens`, `cached_tokens`, `pre_classified`, `pre_class_result` — all `#[serde(default)]`.
4. Extend `UsageMetadata` with `cached_content_token_count: Option<u32>` (Gemini may omit when no cache hit).
5. Extend `RunnerOutput` with `cached_tokens: u32`, populated from the above.

At this point the code compiles with no behavior change. Existing dispatch persists the same data; new fields are zero/None.

### Step 2 — Pre-classifier module (isolated, untriggered)
1. Build `pre_classifier.rs`:
   - Public entry: `pub async fn classify(text: &str, customer_lookup: &CustomerLookupResult, ctx: &PreClassifierContext) -> Result<PreClassResult>`
   - Internal: a focused ~300-token prompt asking Gemini Flash Lite to return `{ "result": "<variant>", "confidence": 0.0..1.0 }`
   - Confidence ≥ 0.85 → use the variant; below → coerce to `Ambiguous`
   - Enum `PreClassResult { Spam, GreetingOnly, ClearVentas, ClearPagos, ClearSoporte, Ambiguous }`
2. Module is wired into `mod.rs` but NOT yet called by `dispatch.rs`. Unit-testable in isolation.

### Step 3 — Dispatch wiring (behind opt-in flag)
1. In `run_dispatch`, after keyword escalation and after `wa_settings` is loaded (per explore: ~line 464):
   - Skip if `!wa_settings.pre_classifier_enabled` → fall through unchanged
   - Skip if `user_text.is_empty()` → fall through unchanged
   - Else call `pre_classifier::classify(...)`
2. Match the result:
   - `Spam` / `GreetingOnly` → find `TrivialResponse { kind, enabled: true, .. }` whose `triggers` contains a substring of `user_text` (case-insensitive, normalized). If found, send `response` via `WhatsAppService`. If no template matches, fall through to normal flow (defensive — empty config shouldn't break dispatch).
   - `ClearVentas` / `ClearPagos` / `ClearSoporte` → reuse the routing mechanics that `transfer_to_agent` already uses to resolve the target specialized agent for that `purpose`, then invoke `run_turn` directly with that agent (skipping Sofía).
   - `Ambiguous` → existing `select_agent` + `run_turn` path.
3. Every pre-classified outcome (including fall-through `Ambiguous`) is logged into `AiInteractions` with `pre_classified: true` and the variant in `pre_class_result`. The Flash Lite call's `input_tokens`/`output_tokens`/`cost_usd_estimate` are also captured.
4. With `pre_classifier_enabled: false` (the default), this entire block is skipped — **regression risk = zero**.

### Step 4 — Metrics surface
1. Add `get_ai_agent_metrics` to the `AiAgentRepository` trait (`db/mod.rs`).
2. Implement in `db/mongo/ai_agent.rs` with a single MongoDB aggregate: `$match { agent_id, created_at: { $gte, $lte } }` → `$group` (summary or by day depending on granularity) → return.
3. Handler `get_ai_agent_metrics_handler` in `modules/ai_agent/handler.rs` — query params parsing, calls trait, returns `{ ok: true, data: ... }`. Errors via `ApiError`.
4. Register the route under the `auth_user_protected` group in the existing AI-agent router builder.
5. Register path + schemas in `openapi.rs`.

### Step 5 — Index
- Add `db.AiInteractions.createIndex({ agent_id: 1, created_at: -1 })` to `scripts/create_indexes.js`.
- The project README already requires manual run of `mongosh < scripts/create_indexes.js` after deploy — call it out in the PR description so ops doesn't miss it.

### Step 6 — Spec delta
- Update `openspec/specs/ai-agent/spec.md` with the pre-classifier behavior, trivial-response model, and metrics endpoint.

### Rationale: why opt-in default
- Pre-classifier adds 200–500 ms of latency on every textual turn. Some workspaces may not want that trade-off.
- Default `false` lets us deploy the code, validate the pipeline on one workspace, then roll out per workspace via the existing UI (locked policy `feedback/config-in-db-not-env`).
- Dark-by-default also makes rollback trivial: revert the toggle in `WaSettings`, no redeploy needed.

### Rationale: substring match for trivial-response triggers
- Q9 in the lock: regex was rejected. Substring keeps the admin UI explainable to non-technical operators ("the message contains this text") and avoids ReDoS.
- Normalization on both sides (lowercase + trim accents) is enough for Spanish greetings ("hola", "buenas", "saludos") and obvious spam patterns.

### Rationale: skip pre-classifier on media-only turns
- Image/audio/sticker-only messages have empty `user_text` after extraction. Calling Flash Lite on empty input wastes tokens and adds latency for no decision quality gain.
- Sticker-as-spam is a real case but is better handled by a separate `kind: "sticker"` `TrivialResponse` matched against the sticker's emoji/metadata — out of scope for v1.

---

## Affected areas

| Area | Change |
|---|---|
| `src/modules/ai_agent/pre_classifier.rs` | NEW (~150 LOC) — pre-classifier module |
| `src/modules/ai_agent/mod.rs` | `pub mod pre_classifier;` |
| `src/modules/ai_agent/dispatch.rs` | Pre-classifier gate (~50 LOC) + `to_interaction()` field wiring |
| `src/modules/ai_agent/handler.rs` | New metrics handler (~80 LOC) |
| `src/modules/ai_agent/runner.rs` | `RunnerOutput.cached_tokens` (~10 LOC) |
| `src/modules/ai_agent/gemini.rs` | `UsageMetadata.cached_content_token_count` parsing (~10 LOC) |
| `src/models/whatsapp.rs` | `WaSettings` extensions + `TrivialResponse` struct |
| `src/models/ai_agent.rs` | `AiInteraction` field extensions, `RunnerOutput.cached_tokens` |
| `src/db/mod.rs` | `get_ai_agent_metrics` trait method |
| `src/db/mongo/ai_agent.rs` | Aggregate impl |
| `src/axum_router.rs` (or AI-agent route group) | Metrics route registration |
| `src/openapi.rs` | Path + schemas |
| `scripts/create_indexes.js` | Compound index `AiInteractions(agent_id, created_at)` |
| `openspec/specs/ai-agent/spec.md` | Capability delta |

**Total**: ~600 LOC new + ~100 LOC modified.

---

## Risks

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| 1 | Pre-classifier latency (200–500 ms) on every textual turn | High | Medium | Skipped on media-only and when `pre_classifier_enabled: false` (default). Latency captured in `AiInteraction.latency_ms` per-turn. SUPERADMIN can disable per-workspace via existing UI. |
| 2 | Pre-classifier false positive routes user to wrong specialist | Medium | Medium | Confidence threshold ≥ 0.85; below → `Ambiguous` → existing flow. Every classification logged in `AiInteractions` for audit. Easy to iterate prompt without redeploy of contracts. |
| 3 | Trivial-response triggers too greedy | Medium | Low | Substring match only (no regex per locked Q9). SUPERADMIN reviews triggers in UI before enabling. Defensive fall-through if no template matches the kind. |
| 4 | `cachedContentTokenCount` absent on some Gemini responses | Low | Low | `Option<u32>` + `#[serde(default)]`. Absent → counted as zero in metrics. |
| 5 | Index migration on prod requires manual `mongosh` run | Low | Medium | Project convention is already manual indexes (per README). Call out in PR description. Without the index, queries still work — they just full-scan; flag in PR for ops to schedule. |
| 6 | Cost-per-token constants drift if Gemini changes pricing | Medium | Low | Document the per-1k-token rates in `gemini.rs` constants. Flag in PR for ops to validate quarterly. The `cost_usd_estimate` field is best-effort — actual billing comes from Google Console. |
| 7 | New `AiInteraction` fields read incorrectly on legacy docs | Low | Low | All fields `#[serde(default)]`. Tested by reading a Phase-1-era doc back. |
| 8 | Pre-classifier fails (Gemini error / timeout) and blocks dispatch | Medium | Medium | On error: log + fall through to existing flow (treat as `Ambiguous`). Pre-classifier is enhancement, not gate. |

---

## Rollback plan

1. **Soft rollback (no redeploy)**: set `pre_classifier_enabled: false` on each affected `WaSettings` doc via the admin UI. The pre-classifier path is fully bypassed; behavior reverts to Phase 2.
2. **Hard rollback (revert commit)**: the change is additive — reverting the commit removes the new endpoint, removes the pre-classifier call, and the new `AiInteraction` fields become unread. No data migration needed.
3. **Index left in place** is harmless even if code is reverted (an unused compound index doesn't hurt reads or writes meaningfully on a low-cardinality collection like `AiInteractions`).

---

## Dependencies

- `ai-agent-guardrails-and-hud` (Phase 1) — deployed
- `ai-agent-conversation-state` (Phase 2) — deployed
- Recent refactor commit `c9bbe98` (per-workspace `WaSettings` toggles) — deployed
- `AiInteractions` collection populated since Phase 1/2 — confirmed in exploration

External:
- Gemini API support for `gemini-2.5-flash-lite` — confirmed available in the relay whitelist (`tools/cf-worker-media-relay/worker.js`)
- `usageMetadata.cachedContentTokenCount` in Gemini responses — documented and verified in exploration

---

## Success criteria

1. `cargo check` passes with zero new warnings.
2. With `pre_classifier_enabled: false` (default) on all workspaces, dispatch behavior is **identical** to pre-change (regression zero).
3. With `pre_classifier_enabled: true`:
   - A "👍" sticker / "hola" message returns a configured `TrivialResponse.greeting` template; no Sofía / specialist invoked.
   - A clear "no me anda el internet" invokes the Soporte agent directly, skipping Sofía.
   - An ambiguous message ("oye, una cosa…") falls through to Sofía as before.
4. `GET /v1/auth-user/ai-agent/agents/:id/metrics?from=&to=` returns aggregate sums; `cached_tokens` is non-zero (validates implicit caching is observable end-to-end).
5. `GET ...?granularity=daily` returns per-day breakdown.
6. New `AiInteraction` docs include `pre_classified`, `pre_class_result`, `cached_tokens`, `thinking_tokens` populated correctly.
7. `mongosh < scripts/create_indexes.js` creates the new compound index `AiInteractions(agent_id_1_created_at_-1)`.
8. OpenAPI `/docs` shows the new endpoint + schemas.

---

## Next phases (informational)

- **`ai-agent-phase3b-explicit-cache`** (deferred, gated on metrics data from this change) — explicit Gemini `cachedContents` API integration. Build only if implicit cache hit rate (visible via this change's metrics) is too low or unreliable.
- **`ai-agent-phase3c-cross-agent-metrics`** (future, if demand) — workspace-level aggregate metrics endpoint.
