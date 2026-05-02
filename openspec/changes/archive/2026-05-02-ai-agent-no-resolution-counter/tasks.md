# Tasks: AI Agent — no_resolution Counter Fix

## Phase 1: Foundation — Cache + Model + Tool Registry

- [x] 1.1 `src/cache/redis_client.rs` — add `reset_ai_no_resolution(&self, conv_id: &str)` method (DEL `ai_agent:no_resolution:{conv_id}` only, async, best-effort). Insert between `incr_ai_no_resolution` (line 487) and `clear_ai_conv_counters` (line 491). Satisfies: Req 4, Scenarios 4.1, 4.2, 4.3.

- [x] 1.2 `src/models/ai_agent.rs` — add `qualification_window_turns: u32` with `#[serde(default)]` to `AiEscalationRules` struct (~line 142). Satisfies: Req 2, Scenarios 2.3, 6.1, 6.2.

- [x] 1.3 `src/models/ai_agent.rs` — add `qualification_window_turns: u32` field to `AiEscalationRulesDto` struct (~line 503) and propagate in `From<AiEscalationRules> for AiEscalationRulesDto` impl (~line 514). Manual propagation required (not a derive). Satisfies: design §2B.

- [x] 1.4 `src/models/ai_agent.rs` — add `qualification_window_turns: Option<u32>` with `#[serde(default, skip_serializing_if = "Option::is_none")]` to `AiEscalationRulesInput` struct (~line 684). Satisfies: Req 7, Scenarios 7.1–7.4.

- [x] 1.5 `src/modules/ai_agent/tools.rs` — add `ToolCategory { InfoLookup, Action }` enum and `tool_category(tool_name: &str) -> ToolCategory` function after `T_*` constants (line 122). Match exhaustive over `T_LOOKUP_CUSTOMER`, `T_GET_INVOICES`, `T_LIST_PLANS`, `T_CHECK_COVERAGE` → InfoLookup; `T_CREATE_TICKET`, `T_REQUEST_HUMAN`, `T_TRANSFER_AGENT` → Action; unknown → `tracing::warn!` + InfoLookup. Satisfies: Req 1, Scenarios 1.1, 1.2, 1.7.

- [x] 1.6 **Verification gate** — run `cargo check` after Phase 1; must compile with 0 errors before proceeding.

## Phase 2: Validator + Handler Wiring

- [x] 2.1 `src/modules/ai_agent/handler.rs` — change `apply_escalation` signature from `fn(...) -> ()` to `fn(...) -> Result<(), ApiError>` (~line 802). Add range check: if `v > 10` return `Err(ApiError::domain_simple(StatusCode::BAD_REQUEST, "qualification_window_turns_out_of_range", ...))` with `tracing::warn!`. Apply `cur.qualification_window_turns = v` on valid input. Satisfies: Req 7, Scenarios 7.1–7.4.

- [x] 2.2 `src/modules/ai_agent/handler.rs` — add `?` at both call sites of `apply_escalation`: line 586 (create_ai_agent_handler) and line 683 (update_ai_agent_handler). Satisfies: design §5 call sites.

- [x] 2.3 `src/modules/ai_agent/handler.rs` — add `qualification_window_turns: 0` to the `AiEscalationRules` struct literal inside `default_agent` function (~line 233). Required because Rust struct literals must be exhaustive. Satisfies: design §2D, backwards compat.

- [x] 2.4 `src/modules/ai_agent/handler.rs` — verify `#[utoipa::path(...)]` responses for create and update handlers. If `(status = 400, ...)` is absent, add `(status = 400, description = "qualification_window_out_of_range")`. Satisfies: design §6 OpenAPI.

- [x] 2.5 **Verification gate** — run `cargo check` after Phase 2; must compile with 0 errors.

## Phase 3: Dispatch Refactor (Core Fix)

- [x] 3.1 `src/modules/ai_agent/dispatch.rs` — refactor lines 871-910 block into 5-branch structure. Replace existing logic with branches: B1 qualification window (`prior_ai_turns < qualification_window_turns` → `tracing::debug!` + skip), B2 Action tool success (`tool_category(&t.tool_name) == Action && t.success` → `state.redis.reset_ai_no_resolution(&conv_hex).await` + `tracing::debug!`), B3 chain_transfer (`had_chain_transfer || cross_workspace_message.is_some()` → `tracing::debug!` + skip), B4 InfoLookup success (any `t.success` → `tracing::debug!` + skip), B5 no useful tool (incr + existing `tracing::info!` + auto_escalate on cap). Preserves the `if cap > 0 {}` outer gate. Preserves `return Ok(())` only after auto_escalate. Satisfies: Req 1, 2, 3, 5, 8; Scenarios 1.1–1.7, 2.1–2.5, 3.1–3.3, 5.1–5.2, 8.1–8.2.

- [x] 3.2 **Verification gate** — run `cargo check` after Phase 3; must compile with 0 errors.

## Phase 4: Unit Tests

- [x] 4.1 `src/modules/ai_agent/dispatch.rs` (or adjacent test module) — write Scenario A (Carla regression): `cap=4`, `window=4`, 4 text-only turns → counter stays at 0 each; turn 5 increments to 1; no escalation fires. Satisfies: Spec Req 2 Scenario 2.4, proposal §Test plan A.

- [x] 4.2 Same file — write Scenario D (sanity): `cap=3`, `window=0`, 3 text-only turns → counter 1→2→3 → auto_escalate fires on turn 3. Confirms B5 path and preserved escalation. Satisfies: Spec Req 3 Scenarios 3.1, 3.3, proposal §Test plan D.

- [x] 4.3 Same file — write Scenario C (InfoLookup does NOT reset): `cap=4`, `window=0`, sequence `[no-tool, no-tool, list_plans ok, no-tool, no-tool]` → counter 1→2→2(skip)→3→4 → escalates on turn 5. Confirms B4, no reset. Satisfies: Spec Req 1 Scenarios 1.1, proposal §Test plan C.

- [x] 4.4 Same file — write Scenario B (Action tool reset): `cap=4`, `window=0`, sequence `[no-tool, no-tool, transfer_to_agent ok, no-tool, no-tool]` → counter 1→2→0→1→2; no escalation. Confirms B2 functional. Satisfies: Spec Req 1 Scenario 1.2, Req 4 Scenarios 4.1–4.2, proposal §Test plan B.

- [x] 4.5 Same file — write edge: unknown tool name with success → treated as InfoLookup (skip, no reset, `tracing::warn!` emitted). Satisfies: Spec Req 1 Scenario 1.7.

- [x] 4.6 Same file — write edge: `cap=0` → no branch evaluates, counter untouched. Satisfies: Spec Req 3 Scenario 3.2.

- [x] 4.7 `src/modules/ai_agent/handler.rs` (or test module) — unit test `apply_escalation` with `qualification_window_turns: 11` → returns `Err(ApiError)` with code `qualification_window_turns_out_of_range`. Satisfies: Spec Req 7 Scenario 7.3.

- [x] 4.8 Same — unit test `apply_escalation` with `qualification_window_turns: 10` → returns `Ok(())` and value is stored. Satisfies: Spec Req 7 Scenarios 7.1, 7.2.

- [x] 4.9 **Verification gate** — run `cargo test` after Phase 4; all tests must pass.

## Phase 5: Final Verification

- [x] 5.1 Run `cargo check` one final time on the complete change; 0 errors required.
- [x] 5.2 Run `cargo test` and confirm all 8+ new tests pass alongside existing suite.
- [x] 5.3 Manual spec review: confirm each of the 22 spec scenarios (Req 1–8) is covered by at least one test or by code inspection of the dispatch branches.
- [x] 5.4 Confirm `apply_escalation` has exactly 2 external call sites (lines 586 and 683) — no hidden third call site that bypasses the validator.
