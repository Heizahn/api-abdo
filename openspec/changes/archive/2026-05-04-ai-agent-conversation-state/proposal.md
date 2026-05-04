# Proposal: AI Agent — Persisted Conversation State — Phase 2

## Intent

Phase 1's `[turn_state]` HUD is recomputed from raw history every turn — ephemeral by design. Sofía therefore re-derives "where am I in this flow" on each call. Real prod bug: the customer says "quiero info del internet", but next turn Sofía still asks "¿eres cliente o quieres contratar?". Intent was already classified — there's nowhere to record it.

Phase 2 introduces structured, persisted conversation state that the model READS (as `[conversation_state]`) and tools WRITE (via `state_patch`). State survives turns, is visible to staff in the UI, and lets a human takeover continue without starting from scratch.

## Scope

### In Scope
- `WaConversationAiState` + `FailedAttempt` + `StatePatch` structs in `src/models/whatsapp.rs`.
- `ai_conv_state: Option<WaConversationAiState>` embedded in `WaConversation` (`#[serde(default)]`, no migration).
- `ToolResult.state_patch: Option<StatePatch>`. Each `exec_*` returns its patch on success.
- `RunnerOutput.state_patches: Vec<StatePatch>` — runner accumulates per turn.
- `run_turn` + `build_system_instruction` gain `conversation_state: Option<&str>` (block injected after `[turn_state]`, before `[faqs]`).
- Dispatch: read `conv.ai_conv_state` → format block → after chain loop fold patches → 1 atomic `update_conversation_ai_conv_state`.
- New trait method `WhatsAppRepository::update_conversation_ai_conv_state` + Mongo impl (`$set`/`$push`).
- `ConversationItem.ai_conv_state` propagated via `conv_to_item` (UI gets it for free).
- `POST /v1/auth-user/whatsapp/conversations/:id/ai-state/reset` (under `user_protected`).
- Reset semantics: transfer same-workspace clears intent/step (preserves collected_data); ticket sets `current_step="ticket_created"`; reopen clears state.
- Caps: `failed_attempts` ≤ 5 (FIFO); `collected_data` ≤ 20 keys × 500 chars.
- Optional kill switch `Config.enable_ai_conversation_state` (default `true`).

### Out of Scope
- LLM-scored `intent_confidence` (v1 = binary 1.0 from keyword guardrails).
- Typed schema for `collected_data` (stays freeform `HashMap<String,String>` / BSON).
- State analytics dashboard.
- Backfill from history; existing convs start `None`.
- Multi-agent state competition (today serialized by dispatch lock).

## Capabilities

### New Capabilities
- None.

### Modified Capabilities
- `ai-agent`: adds (a) state persistence rules, (b) tool-driven state mutation contract via `state_patch`, (c) `[conversation_state]` injection into `system_instruction`, (d) reset semantics on transfer/ticket/reopen.

## Approach

1. Define `WaConversationAiState`, `FailedAttempt`, `StatePatch` in `models/whatsapp.rs`.
2. Embed `ai_conv_state` in `WaConversation` and `ConversationItem`.
3. Add `update_conversation_ai_conv_state` to `WhatsAppRepository` trait + Mongo impl.
4. Extend `ToolResult.state_patch`; populate in each `exec_*` on success.
5. Extend `RunnerOutput.state_patches`; accumulate as chain loop runs `execute_tool`.
6. Add `conversation_state` param to `run_turn` + `build_system_instruction`; inject block.
7. Dispatch: read state pre-runner → format → pass; post-runner fold patches → 1 DB write.
8. Wire reset semantics in `exec_transfer_to_agent`, `exec_create_ticket`, conv-reopen flow.
9. Expose `POST .../ai-state/reset` endpoint (acquires `try_lock_ai_dispatch`).
10. Document delta in `openspec/specs/ai-agent/spec.md`; register schemas in `openapi.rs`.

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `src/models/whatsapp.rs` | Modified | new structs + embed in `WaConversation` + `ConversationItem` |
| `src/modules/ai_agent/tools.rs` | Modified | `ToolResult.state_patch` + per-tool patches |
| `src/modules/ai_agent/runner.rs` | Modified | `conversation_state` param + `RunnerOutput.state_patches` |
| `src/modules/ai_agent/dispatch.rs` | Modified | read state, format block, fold patches, write |
| `src/modules/ai_agent/sandbox.rs` | Modified | mirror tool/runner signature additions |
| `src/db/mod.rs` | Modified | new trait method |
| `src/db/mongo/whatsapp.rs` | Modified | `$set`/`$push` impl |
| `src/modules/whatsapp/handler.rs` | Modified | `conv_to_item` propagation + reset endpoint |
| `src/axum_router.rs` | Modified | register reset route |
| `src/openapi.rs` | Modified | register new endpoint + schemas |
| `openspec/specs/ai-agent/spec.md` | Modified | delta spec |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| State drift on dispatch crash mid-turn | Low | Missed update, not stale. Tolerable. |
| `collected_data` unbounded growth | Med | Cap 20 keys × 500 chars/value. |
| `failed_attempts` unbounded growth | Med | FIFO trim to last 5. |
| New tool added without `state_patch` | Med | Convention: every `exec_*` returns `Some`/`None` explicitly; lint comment near dispatcher. |
| LLM ignores `[conversation_state]` block | Low | Same as `[turn_state]`; clear formatting. |
| Race between webhook + manual reset | Low | Reset endpoint acquires `try_lock_ai_dispatch`. |

## Rollback Plan

Pure additive. Revert the commit — existing convs continue working since `ai_conv_state` is `Option<_>` with `#[serde(default)]`. No migration to undo. Emergency kill switch: set `enable_ai_conversation_state=false`; dispatch skips reads/writes (mirrors `enable_ai_guardrails`).

## Dependencies

- Phase 1 `ai-agent-guardrails-and-hud` deployed (commits `79b707c`, `26dd0cb`, `85ef368`).
- No new external dependencies.

## Success Criteria

- [ ] `cargo check` passes with zero new warnings.
- [ ] After `lookup_customer`, `ai_conv_state.collected_data["client_id"]` is set and `completed_actions` contains `"lookup_customer"`.
- [ ] After `transfer_to_agent`, `current_step` is `"transferred_to_agent_X"`.
- [ ] `[conversation_state]` block appears in `system_instruction` logs when state is non-`None`.
- [ ] `GET /conversations/:id` returns `ai_conv_state`.
- [ ] `POST /conversations/:id/ai-state/reset` clears state.
- [ ] Existing conversations (`ai_conv_state == None`) continue working unchanged.
