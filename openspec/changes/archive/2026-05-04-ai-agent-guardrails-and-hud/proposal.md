# Proposal: AI Agent Guardrails (server-side) + Turn-State HUD — Phase 1

## Intent

Eliminate server-side hallucinations on tool calls and give the model a deterministic per-turn state HUD.

Real prod bug: customer says "quiero info del internet", AI calls `check_coverage(zone="Naguanagua")` (zone never mentioned), tool says "not covered", AI tells customer "no llegamos a Naguanagua". Mitigated at prompt level (commit 62a77ad) but fragile: SUPERADMIN can override descriptions, prompts get long and the LLM ignores rules under pressure, and at 8000+ clients each hallucinated `check_coverage` call costs ~$0.30. The HUD also reduces redundant questions ("Sofía vuelve a preguntar lo mismo") because the model has explicit state instead of inferring from history.

## Scope

### In Scope
- New `src/modules/ai_agent/guardrails.rs` with pure helpers (no I/O):
  - `extract_customer_explicit_zones(&[WaMessage]) -> Vec<String>`
  - `extract_recent_media_ids(&[WaMessage]) -> Vec<String>`
  - `extract_customer_explicit_intents(&[WaMessage]) -> Vec<String>`
  - `validate_zone_mentioned(claimed: &str, customer_zones: &[String]) -> bool` (bidirectional substring via `normalize_zone`)
  - `build_turn_state(&[ConvTurn], &[String], &[String]) -> Option<String>`
- `ToolContext` gains 2 fields: `customer_explicit_zones`, `recent_media_ids`.
- Guardrail in `exec_check_coverage`: if zone not mentioned by customer → `ToolResult::err("zone_not_mentioned_by_customer")`.
- Guardrail in `exec_report_payment`: if `media_id` not in recent inbound media → `ToolResult::err("media_id_not_in_conversation")`.
- `[turn_state]` HUD block injected by `build_system_instruction` (same pattern as `[agent_state]`, no duplication of `already_greeted`).
- Optional kill switch `Config.enable_ai_guardrails: bool` (default `true`).

### Out of Scope
- Reason-text validation in `exec_transfer_to_agent` and `exec_create_ticket` (Phase 2 — false-positive risk).
- `prior_tool_calls` field in HUD (no `list_by_conversation` query exists for `AiInteractions`).
- Refactoring `run_turn`'s parameter list (already 14 params; cosmetic, separate change).
- Pre-classifier and Gemini context cache (Phase 3).

## Capabilities

### New Capabilities
- None.

### Modified Capabilities
- `ai-agent`: adds 2 server-side guardrail rules (`zone_not_mentioned_by_customer`, `media_id_not_in_conversation`) and a deterministic `[turn_state]` HUD block contract (turn_number, customer_explicit_zones, customer_explicit_intents).

## Approach

1. Create `guardrails.rs` with pure functions (trivially testable).
2. Extend `ToolContext` additively (existing tools unaffected — they just don't read the new fields).
3. In `dispatch.rs`, precompute `customer_explicit_zones` + `recent_media_ids` + `turn_state_owned` ONCE per turn from the `recent` slice already loaded at line 297 (zero new DB queries).
4. Add `turn_state: Option<&str>` parameter to `run_turn` and `build_system_instruction`; inject `[turn_state]` chunk after `[agent_state]`.
5. Add fail-fast guardrails at the top of `exec_check_coverage` and `exec_report_payment`.
6. Mirror `ToolContext` field additions in `sandbox.rs` (synthetic empty Vecs).

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `src/modules/ai_agent/guardrails.rs` | New | ~120 LOC of pure helpers |
| `src/modules/ai_agent/mod.rs` | Modified | `pub mod guardrails;` |
| `src/modules/ai_agent/tools.rs` | Modified | +2 `ToolContext` fields, +2 guardrail blocks (~35 LOC) |
| `src/modules/ai_agent/dispatch.rs` | Modified | precompute zones/media_ids/turn_state, pass through (~45 LOC) |
| `src/modules/ai_agent/runner.rs` | Modified | new `turn_state` param + chunk injection (~18 LOC) |
| `src/modules/ai_agent/sandbox.rs` | Modified | mirrored ToolContext additions |
| `openspec/specs/ai-agent/spec.md` | Modified | delta spec: error codes + HUD contract |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Zone extraction false positive | Low | Multi-char Spanish names; worst case = false-pass (tool runs) |
| Zone extraction false NEGATIVE (synonym/abbrev) | Med | Bidirectional substring match; on miss, AI asks customer — acceptable |
| Sandbox tests break (ToolContext schema changed) | Med | Mirror update in `sandbox.rs` flagged in affected files |
| `media_id_not_in_conversation` blocks legitimate retries | Low | Retries within same turn keep media_id in same `recent` slice |
| `run_turn` signature growth | Low | Cosmetic; out-of-scope here |

## Rollback Plan

Pure additive change. Revert the commit. Existing tools without guardrails keep working unchanged. Emergency kill switch: set `enable_ai_guardrails=false` in config — short-circuits guardrail checks without revert.

## Dependencies

None. Self-contained within the `ai_agent` module.

## Success Criteria

- [ ] Reproduce the Naguanagua bug pre-change; post-change tool returns `zone_not_mentioned_by_customer` and AI asks for the zone.
- [ ] `[turn_state]` block appears in `system_instruction` logs with correct `turn_number`, zones, intents.
- [ ] Existing tools (`lookup_customer`, `calculate_amount_bs`, `get_invoices`, `list_plans`) unaffected.
- [ ] `cargo check` passes with zero new warnings.
