# Tasks: AI Agent Guardrails + Turn-State HUD (Phase 1)

## Phase 1: Config Kill Switch
<!-- Spec ref: Req 14 (Sc 14.1, 14.2) -->

- [x] 1.1 In `src/config.rs` `Config` struct, add `pub enable_ai_guardrails: bool` field after `gemini_base_url`. Document env var `ENABLE_AI_GUARDRAILS`.
- [x] 1.2 In `Config::from_env()`, parse `ENABLE_AI_GUARDRAILS` env var; use `!matches!(v.trim().to_lowercase().as_str(), "false" | "0" | "no")`, default `true` when unset (per ADR-4 — Config is not serde-derived, use `env::var` pattern only).
- [x] 1.3 `cargo check` — confirm no new errors or warnings.

## Phase 2: guardrails.rs Module
<!-- Spec ref: Req 9, 10, 11, 12, 13 -->

- [x] 2.1 Create `src/modules/ai_agent/guardrails.rs`. Add `INTENT_KEYWORDS` static table with 9 groups from ADR-5 design (already-normalized trigger substrings).
- [x] 2.2 Implement `extract_customer_explicit_zones(messages: &[WaMessage]) -> Vec<String>` — filter `direction == "in"`, map `body` through `normalize_zone`, skip empty. (Spec ref: Sc 9.1–9.4)
- [x] 2.3 Implement `extract_recent_media_ids(messages: &[WaMessage]) -> Vec<String>` — filter `direction == "in"`, collect `media_id`, dedupe preserving order. (Spec ref: Sc 10.1–10.2)
- [x] 2.4 Implement `validate_zone_mentioned(claimed: &str, customer_zones: &[String]) -> bool` — normalize claimed, bidirectional `contains` over each customer zone (reuse `tools::normalize_zone` via `super::tools::normalize_zone`). (Spec ref: Sc 9.1–9.4, ADR-1)
- [x] 2.5 Implement `extract_customer_explicit_intents(messages: &[WaMessage]) -> Vec<String>` — join all normalized inbound bodies into one buffer; iterate `INTENT_KEYWORDS`, push group key on first substring hit, de-dup in declaration order. (Spec ref: Sc 13.1–13.3)
- [x] 2.6 Implement `build_turn_state(history: &[ConvTurn], zones: &[String], intents: &[String]) -> Option<String>` — return `None` when `turn_number == 1 && zones.is_empty() && intents.is_empty()`; otherwise emit `turn_number:`, optional `customer_explicit_zones:`, optional `customer_explicit_intents:` lines. (Spec ref: Sc 11.1–11.3, ADR-6)
- [x] 2.7 Add `pub mod guardrails;` to `src/modules/ai_agent/mod.rs` alphabetically between `escalation` and `gemini`.
- [x] 2.8 `cargo check` — confirm `guardrails.rs` compiles standalone with no warnings.

## Phase 3: ToolContext Extension
<!-- Spec ref: Req 12 (Sc 12.1, 12.2) -->

- [x] 3.1 In `src/modules/ai_agent/tools.rs`, add `pub customer_explicit_zones: Vec<String>` and `pub recent_media_ids: Vec<String>` to `ToolContext` struct (after `default_ticket_category_id`).
- [x] 3.2 In `src/modules/ai_agent/sandbox.rs`, add `customer_explicit_zones: Vec::new()` and `recent_media_ids: Vec::new()` to the `ToolContext` construction. Add `None` for `turn_state` in the `run_turn` call between `agent_state` and `Some(&active_prompt_vars)`. (Spec ref: Sc 12.1)
- [x] 3.3 `cargo check` — `sandbox.rs` must compile; struct exhaustiveness check catches any missing fields.

## Phase 4: dispatch.rs Precompute
<!-- Spec ref: Req 9, 10, 11 (ADR-2) -->

- [x] 4.1 Add `use super::guardrails;` near the existing `use super::escalation;` imports in `src/modules/ai_agent/dispatch.rs`.
- [x] 4.2 After `recent` is loaded (around line 298), compute `customer_explicit_zones`, `recent_media_ids`, and `customer_explicit_intents` via the three new guardrails functions.
- [x] 4.3 After `full_history` is built, compute `turn_state_owned: Option<String>` via `guardrails::build_turn_state(&full_history, &customer_explicit_zones, &customer_explicit_intents)`.
- [x] 4.4 In the `ToolContext` construction, add `customer_explicit_zones: customer_explicit_zones.clone()` and `recent_media_ids: recent_media_ids.clone()`.
- [x] 4.5 `cargo check` — confirm all precompute bindings and `ToolContext` fields align.

## Phase 5: runner.rs HUD Injection
<!-- Spec ref: Req 11 (Sc 11.1–11.3, ADR-3, ADR-6) -->

- [x] 5.1 Add `turn_state: Option<&str>` parameter to `run_turn` signature in `src/modules/ai_agent/runner.rs` (after `agent_state`, before `prompt_vars`).
- [x] 5.2 Add `turn_state: Option<&str>` parameter to `build_system_instruction`; inject `format!("[turn_state]\n{}", ts.trim())` chunk after the `[agent_state]` chunk and before `[faqs]`.
- [x] 5.3 Forward `turn_state` from `run_turn` body down to `build_system_instruction` call.
- [x] 5.4 In `src/modules/ai_agent/dispatch.rs` `run_turn` call site, pass `turn_state_owned.as_deref()` in the new position (between `agent_state_owned.as_deref()` and `Some(&active_prompt_vars)`).
- [x] 5.5 `cargo check` — both `dispatch.rs` and `sandbox.rs` call sites must pass correct argument count.

## Phase 6: Tool Guardrails
<!-- Spec ref: Req 9 (Sc 9.1–9.5), Req 10 (Sc 10.1–10.3) -->

- [x] 6.1 In `src/modules/ai_agent/tools.rs` `exec_check_coverage`, after `raw` is trimmed and non-empty check passes, insert guardrail block: `if ctx.state.config.enable_ai_guardrails && !ctx.is_sandbox` → call `guardrails::validate_zone_mentioned(raw, &ctx.customer_explicit_zones)` → return `ToolResult::err("zone_not_mentioned_by_customer", started)` on false. (Spec ref: Sc 9.2, 9.4, 9.5)
- [x] 6.2 In `exec_report_payment`, after the `media_id.trim().is_empty()` check, insert guardrail block: same `enable_ai_guardrails && !is_sandbox` gate → check `ctx.recent_media_ids.iter().any(|m| m == mid)` → return `ToolResult::err("media_id_not_in_conversation", started)` on miss. (Spec ref: Sc 10.2, 10.3)
- [x] 6.3 `cargo check` — zero new warnings.

## Phase 7: Verification
<!-- Spec ref: All requirements -->

- [x] 7.1 Final `cargo check` — zero new errors, zero new warnings across all modified files.
- [ ] 7.2 Manual smoke test: send "quiero info del internet" → tool must return `zone_not_mentioned_by_customer`. (Sc 9.2 — Naguanagua repro)
- [ ] 7.3 Manual smoke test: send "vivo en Valencia, ¿hay cobertura?" → `check_coverage("valencia")` guardrail passes. (Sc 9.1)
- [ ] 7.4 Manual smoke test: send payment image → AI calls `report_payment` with real `media_id` → guardrail passes. (Sc 10.1)
- [ ] 7.5 Manual smoke test: no image + AI hallucinates `media_id` → returns `media_id_not_in_conversation`. (Sc 10.2)
- [ ] 7.6 Manual smoke test: set `ENABLE_AI_GUARDRAILS=false`, restart → guardrails bypassed on both tools. (Sc 9.5, 10.3, 14.2)
- [ ] 7.7 Manual smoke test: inspect logs on turn 3 with zones+intents → `[turn_state]` block appears with correct fields. (Sc 11.1)
- [ ] 7.8 Manual smoke test: sandbox run (Shadow mode) → `check_coverage` and `report_payment` work normally (`is_sandbox=true` bypasses guardrails). (Sc 12.1)
- [ ] 7.9 Manual smoke test: customer first message is "hola" → `[turn_state]` block is omitted from system instruction. (Sc 11.2)
