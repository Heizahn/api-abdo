# Tasks: AI Agent Phase 3a — Pre-classifier + Metrics

_Spec refs: Requirements 25–32 | Design ADRs 1–9_

---

## Phase 1 — Schema additions (foundation; everything depends on this)

- [x] 1.1 Add `TrivialResponse { id, kind, triggers, response, enabled, priority: i32 }` to `src/models/whatsapp.rs`. `priority` gets `#[serde(default)]`. Derive `ToSchema`. — _Spec 28_
- [x] 1.2 Add `pre_classifier_enabled: bool` (`#[serde(default)]`) and `trivial_responses: Vec<TrivialResponse>` (`#[serde(default)]`) to `WaSettings` in `src/models/whatsapp.rs`. Also add both fields to `UpdateSettingsRequest` (replace-all semantics). — _Spec 25_
- [x] 1.3 Add four fields to `AiInteraction` in `src/models/ai_agent.rs`, all `#[serde(default)]`: `thinking_tokens: u32`, `cached_tokens: u32`, `pre_classified: bool`, `pre_class_result: Option<String>` (also `#[serde(skip_serializing_if = "Option::is_none")]`). — _Spec 29_
- [x] 1.4 Add `AiAgentPurpose` enum (`Recepcionista/Ventas/Pagos/Soporte`, `serde rename_all = "snake_case"`, `ToSchema`) and `purpose: Option<AiAgentPurpose>` (`#[serde(default)]`) to `AiAgent` struct in `src/models/ai_agent.rs`. Doc comment: "Set by SUPERADMIN to enable Clear* routing; legacy agents (`None`) always fall through to Sofía." — _ADR-5, Spec 27.3_
- [x] 1.5 `cargo check` — zero new warnings before continuing. — _Gate_

## Phase 2 — Gemini extensions

- [x] 2.1 Add `cached_content_token_count: u32` (`#[serde(default, rename = "cachedContentTokenCount")]`) to `UsageMetadata` in `src/modules/ai_agent/gemini.rs`. — _Spec 29, ADR §2.4_
- [x] 2.2 Add `response_mime_type: Option<String>` and `response_schema: Option<serde_json::Value>` (both `#[serde(skip_serializing_if = "Option::is_none")]`) to `GenerationConfig` in `gemini.rs`. Existing runner callers set `None` implicitly — no change needed. — _ADR-3, §2.5_
- [x] 2.3 Add `ModelRates { input_per_m, output_per_m, cached_input_per_m }`, constants `RATES_FLASH/RATES_FLASH_LITE/RATES_PRO/RATES_DEFAULT`, and `pub fn rate_for_model(model_id: &str) -> ModelRates` to `gemini.rs`. 2026-05 rates as `const`; quarterly-review comment. Default fallback = `RATES_FLASH`. — _ADR-7/8, Spec 32_
- [x] 2.4 Add `pub fn estimate_cost_usd(model_id, input_tokens, cached_tokens, output_tokens, thinking_tokens) -> f64` to `gemini.rs` using the formula from ADR-7. Keep the old 2-arg overload as a thin shim calling the new function with `cached_tokens=0, thinking_tokens=0`. — _Spec 32, ADR-7_
- [x] 2.5 `cargo check`. — _Gate_

## Phase 3 — Pre-classifier module

- [x] 3.1 Create `src/modules/ai_agent/pre_classifier.rs`. Define: `PreClassResult` enum (6 variants) with `as_str()` + `from_str()`, `PreClassRaw` (Deserialize), `PreClassTokens`, `PreClassResultFull`, `PreClassifierContext`. — _Spec 26_
- [x] 3.2 Implement `pub async fn classify(text, customer_lookup_summary, ctx) -> Result<PreClassResultFull, String>`. Use `gemini-2.5-flash-lite`, `temperature=0.0`, `max_output_tokens=80`, `thinking_budget=0`, `response_mime_type="application/json"`. Confidence gate: raw confidence < 0.85 → `gated_variant = Ambiguous`; preserve `variant` for audit. On parse fail: `tracing::warn!`, return `Ambiguous`. — _Spec 26.1–26.4, ADR-3_
- [x] 3.3 Add `fn build_prompt(text, customer_lookup_summary) -> String` with the Spanish prompt template (from ADR-3) and `fn strip_json_fence(s: &str) -> String`. — _Spec 26.4_
- [x] 3.4 Add `pub mod pre_classifier;` to `src/modules/ai_agent/mod.rs`. — _structural_
- [x] 3.5 `cargo check`. — _Gate_

## Phase 4 — Trivial response matching + RunnerOutput wiring

- [x] 4.1 Add `fn pick_trivial<'a>(responses, kind, text_normalized) -> Option<&'a TrivialResponse>` in `src/modules/ai_agent/dispatch.rs` (private). Filter enabled + kind, trigger via `normalize_zone` substring (empty triggers = catch-all), stable sort by `priority` desc, return first. — _Spec 28.1–28.2, ADR-4_
- [x] 4.2 Add `fn build_customer_summary_short(customer_context: &Option<String>) -> String` in `dispatch.rs`. Extracts `"  - [1]"` line from existing `build_customer_context` output; returns `"sin match en DB"` on no match. — _ADR-3, §2.3_
- [x] 4.3 Add `cached_tokens: u32` to `RunnerOutput` in `src/modules/ai_agent/runner.rs`. Accumulate from `usage.cached_content_token_count` inside the turn loop. Pass to `RunnerOutput` constructor. — _Spec 29, §2.6_
- [x] 4.4 Update `RunnerOutput.cost_usd_estimate` computation in `runner.rs` to call the new 5-arg `gemini::estimate_cost_usd(model_id, input, cached, output, thinking)`. Replace any hardcoded rate. — _Spec 32_
- [x] 4.5 `cargo check`. — _Gate_

## Phase 5 — DB trait + Mongo implementation

- [x] 5.1 In `src/db/mod.rs`, add to `AiAgentRepository` trait: `async fn find_active_agent_by_workspace_and_purpose(workspace_id, purpose: AiAgentPurpose) -> Result<Option<AiAgent>, String>`. — _ADR-5, Spec 27.3_
- [x] 5.2 In `src/db/mod.rs`, add `pub enum MetricsGranularity { Summary, Daily }`. Add `AiAgentMetricsRaw`, `AiAgentMetricsSummary` (Default derive), `AiAgentMetricsDailyBucket` structs. Add `async fn get_ai_agent_metrics(agent_id, from, to, granularity) -> Result<AiAgentMetricsRaw, String>` to `AiAgentRepository`. — _Spec 30, ADR-9_
- [x] 5.3 Implement `find_active_agent_by_workspace_and_purpose` in `src/db/mongo/ai_agent.rs`. Query: `{ workspace_ids: oid, enabled: true, purpose: "<snake_case>" }`, sort `created_at: 1`. — _ADR-5_
- [x] 5.4 Implement `get_ai_agent_metrics` in `src/db/mongo/ai_agent.rs`. Two parallel pipelines via `tokio::join!`: Aggregate A (summary `$group(_id:null)` with `$ifNull` on all new fields), Aggregate B (pre-class breakdown `$group(_id:"$pre_class_result")`). For `Daily` granularity: replace `_id:null` with `$dateToString(timezone:"America/Caracas")` in Aggregate A; sort `_id:1`. Fill missing `pre_classified_breakdown` keys with 0 in handler (not here). — _ADR-9, Spec 30.5–30.6_
- [x] 5.5 `cargo check`. — _Gate_

## Phase 6 — Dispatch wiring (pre-classifier gate)

- [x] 6.1 In `dispatch.rs`, after `build_customer_context(...)` and after keyword escalation, before `build_prompt_variables(...)`: insert the pre-classifier gate. Gate fires only when `wa_settings.pre_classifier_enabled && !user_text.trim().is_empty()`. Wrap in API-key decrypt check; on missing key → skip gate silently. — _ADR-1, Spec 25.1–25.3_
- [x] 6.2 Implement `Spam` match arm: call `pick_trivial(…, "spam", …)`. If match → `send_outbound` template text; if no match → silent drop. Both paths: call `persist_pre_class_only_interaction(...)` helper then `return Ok(())`. — _Spec 27.1, ADR-6_
- [x] 6.3 Implement `GreetingOnly` match arm: call `pick_trivial(…, "greeting", …)`. If match → send + persist + `return Ok(())`. If no match → fall through (do NOT return early). — _Spec 27.2, ADR-6_
- [x] 6.4 Implement `ClearVentas/ClearPagos/ClearSoporte` match arm: map to `AiAgentPurpose`, call `find_active_agent_by_workspace_and_purpose`. `Some(target)` → `active_agent = target`; `None` → `tracing::warn!` + fall through. — _Spec 27.3, ADR-5_
- [x] 6.5 Add `persist_pre_class_only_interaction(...)` private async helper in `dispatch.rs`. Builds `AiInteraction` with `pre_classified=true`, `pre_class_result=Some(variant)`, Flash Lite cost via `estimate_cost_usd("gemini-2.5-flash-lite", ...)`, zero tool_calls. Calls `state.db.insert_ai_interaction(...)`. — _Spec 27.1, 32.1_
- [x] 6.6 Update `RunnerOutput::to_interaction(...)` signature in `runner.rs` to accept `pre_class: Option<&PreClassResultFull>`. Merge pre-class tokens + latency + cost into `AiInteraction` fields as specified in §2.8. Set `pre_classified = pre_class.is_some()`, `pre_class_result = pre_class.map(|p| p.variant.as_str().to_string())`. — _Spec 29.2, §2.8_
- [x] 6.7 Update all callers of `to_interaction(...)` in `dispatch.rs` to pass `pre_class.as_ref()` (or `None` for paths where pre-classifier did not run). — _structural_
- [x] 6.8 `cargo check`. — _Gate_

## Phase 7 — Metrics HTTP handler + OpenAPI

- [x] 7.1 Add `AiAgentMetricsResponse { ok: bool, data: AiAgentMetricsData }` and supporting DTOs (`pre_classified_breakdown: HashMap<String,u64>`, `daily_breakdown: Option<Vec<DailyBucket>>`) to `src/modules/ai_agent/handler.rs`. Implement `From<AiAgentMetricsRaw>` to convert DB shape → response DTO; fill missing breakdown keys with 0; compute `cache_hit_rate = cached/input` (guard div-by-zero). — _Spec 30_
- [x] 7.2 Add `get_ai_agent_metrics_handler` to `handler.rs` with `#[utoipa::path]`. Validate: `ObjectId::parse_str` → 400 `invalid_agent_id`; RFC3339 parse for `from`/`to` → 400 `invalid_date_range`; `from > to` → 400 `invalid_date_range`; unknown granularity → 400 `invalid_granularity`; agent not found → 404 `agent_not_found`. — _Spec 30.2–30.4_
- [x] 7.3 Register route in `src/modules/ai_agent/mod.rs` under `user_routes()`: `GET /v1/auth-user/whatsapp/ai-agent/agents/:id/metrics`. Mirror existing AI Agent route registration pattern. — _Spec 30_
- [x] 7.4 Register path `get_ai_agent_metrics_handler` and schemas `AiAgentMetricsResponse`, `TrivialResponse`, `AiAgentPurpose` in `src/openapi.rs`. — _structural_
- [x] 7.5 `cargo check`. — _Gate_

## Phase 8 — MongoDB index

- [x] 8.1 Add compound index declaration to `scripts/create_indexes.js`: `db.AiInteractions.createIndex({ agent_id: 1, created_at: -1 }, { name: "agent_id_1_created_at_-1" })`. Mirror existing syntax in the file. — _Spec 31_

## Phase 9 — Verification

- [x] 9.1 Final `cargo check` across the full project — zero new warnings.
- [x] 9.2 Unit tests in same files: `strip_json_fence` (fence/no-fence/plain), `PreClassResult::from_str` (known+unknown), `pick_trivial` (empty list, no kind, multi-match with priority, empty-triggers catch-all, accent normalization), `rate_for_model` (flash-lite/flash/pro/unknown), `estimate_cost_usd` (cached subtract, thinking at output rate, zero-input). — _Spec 26.2–26.3, 28.1–28.2, 32.2_
- [ ] 9.3 Smoke tests (manual, post-deploy) per design §5: disabled baseline, greeting fall-through, spam silent drop, ClearSoporte direct routing, no-specialist fall-through, metrics summary, metrics daily, 400s on invalid params, legacy doc deserialization. — _Spec 25.1, 26.3, 27.1–27.4, 30.1–30.6, 31.1_
- [ ] 9.4 Confirm `AiAgent.purpose` migration note in PR description: SUPERADMIN must set field explicitly; legacy `None` agents fall through to Sofía. Note manual `mongosh < scripts/create_indexes.js` post-deploy requirement. — _ADR-5, Spec 31, project CLAUDE.md_

---

## Dependency summary

```
Phase 1 ──► Phase 2 ──► Phase 3 ──► Phase 6
    │                        │
    └─────────────────────► Phase 4 ──► Phase 6
                                        │
Phase 5 (needs 1) ──────────────────────┘
                                        │
Phase 7 (needs 5) ──────────────────────┘──► Phase 9
Phase 8 (independent) ──────────────────────► Phase 9
```
