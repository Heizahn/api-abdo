# Tasks: AI Agent — Persisted Conversation State (Phase 2)

> Each phase gates on `cargo check` before the next phase starts.
> Spec refs: Requirements 15–24 from `specs/ai-agent/spec.md`.

---

## Phase 1: Models (`src/models/whatsapp.rs`)

- [ ] 1.1 Add `FailedAttempt { tool: String, error: String, at: DateTime<Utc> }` with `Serialize/Deserialize/Clone/ToSchema`. `Spec ref: Sc 15`
- [ ] 1.2 Add `WaConversationAiState` with all fields as specced (`BTreeMap`, `Vec`, `Option`, `updated_at`); derive `Debug/Clone/Serialize/Deserialize/ToSchema/Default`. `Spec ref: Sc 15`
- [ ] 1.3 Add `StatePatch` enum — 5 variants with `#[serde(tag = "kind")]`. `Spec ref: Sc 16`
- [ ] 1.4 Embed `ai_conv_state: Option<WaConversationAiState>` in `WaConversation` with `#[serde(rename = "aiConvState", skip_serializing_if = "Option::is_none", default)]`. `Spec ref: Sc 15.1`
- [ ] 1.5 Add `ai_conv_state: Option<WaConversationAiState>` to `ConversationItem` with `#[serde(skip_serializing_if = "Option::is_none")]` (no rename — JSON key stays `ai_conv_state`). `Spec ref: Sc 22.1`
- [ ] 1.6 `cargo check` — zero new errors/warnings. Gate for Phase 2.

## Phase 2: Patch application module (`src/modules/ai_agent/state.rs` NEW)

- [ ] 2.1 Create `src/modules/ai_agent/state.rs`. Add pub consts: `COLLECTED_DATA_KEY_CAP=20`, `COLLECTED_DATA_VALUE_CHAR_CAP=500`, `PENDING_DATA_CAP=20`, `COMPLETED_ACTIONS_CAP=50`, `FAILED_ATTEMPTS_CAP=5`. `Spec ref: Sc 21`
- [ ] 2.2 Implement `pub fn apply_state_patches(state: WaConversationAiState, patches: &[StatePatch]) -> WaConversationAiState` — LWW semantics per ADR-8; caps per ADR-5; always sets `updated_at`. `Spec ref: Sc 21.1, 21.2, 21.3`
- [ ] 2.3 Implement `pub fn format_conversation_state(state: &WaConversationAiState) -> String` — omit None/empty fields; multi-line block matching spec §18 format. `Spec ref: Sc 18.2, 18.3`
- [ ] 2.4 Add `pub fn slugify_label(label: &str) -> String` — lowercase, replace non-alphanumeric runs with `_`, trim surrounding underscores. Used in Phase 5 tool wiring. `Spec ref: Sc 20.1`
- [ ] 2.5 Add `pub mod state;` to `src/modules/ai_agent/mod.rs`.
- [ ] 2.6 `cargo check` — zero new errors/warnings. Gate for Phase 3.

## Phase 3: DB trait + impl

- [ ] 3.1 Add `async fn update_conversation_ai_conv_state(&self, conv_id: &ObjectId, state: Option<&WaConversationAiState>) -> Result<(), String>;` to `WhatsAppRepository` trait in `src/db/mod.rs`. `Spec ref: Sc 22`
- [ ] 3.2 Implement in `src/db/mongo/whatsapp.rs`: `Some(s)` → `$set { "aiConvState": bson_state }`, `None` → `$unset { "aiConvState": "" }`. Single `update_one`. `Spec ref: Sc 22`
- [ ] 3.3 `cargo check` — zero new errors/warnings. Gate for Phase 4.

## Phase 4: Config kill switch (`src/config.rs`)

- [ ] 4.1 Add `pub enable_ai_conversation_state: bool` parsed from env var `ENABLE_AI_CONVERSATION_STATE`; default `true` (mirrors `enable_ai_guardrails` pattern). `Spec ref: Sc 23`
- [ ] 4.2 `cargo check` — zero new errors/warnings. Gate for Phase 5.

## Phase 5: ToolResult extension + per-tool patches (`src/modules/ai_agent/tools.rs`)

- [ ] 5.1 Add `pub state_patches: Vec<StatePatch>` field to `ToolResult`.
- [ ] 5.2 Update `ToolResult::ok` and `ToolResult::err` constructors to default `state_patches: Vec::new()`. `Spec ref: Sc 16.1`
- [ ] 5.3 Add `fn with_patches(mut self, patches: Vec<StatePatch>) -> Self` builder on `ToolResult`.
- [ ] 5.4 Wire `lookup_customer` success → `SetCollectedData{"client_id", id}` + `AddCompletedAction("lookup_customer")`; no items → `AddCompletedAction` only. `Spec ref: Sc 16.2`
- [ ] 5.5 Wire `check_coverage` covered → `SetCollectedData{"zone", zone}` + `AddCompletedAction`; not covered → `AddCompletedAction` only. Wire `list_plans`, `get_invoices`, `calculate_amount_bs` → `AddCompletedAction` only. `Spec ref: Sc 16`
- [ ] 5.6 Wire `report_payment` new → `AddCompletedAction("report_payment")` + `SetCurrentStep("payment_reported")`. `already_registered` → `SetCurrentStep("payment_already_registered")` only (no `AddCompletedAction`). `Spec ref: Sc 16.3`
- [ ] 5.7 Wire `transfer_to_agent` live same-workspace → `AddCompletedAction` + `SetCurrentStep("transferred_to_<slugify_label(target.label)>")`. Cross-workspace → `AddCompletedAction` + `SetCurrentStep("cross_workspace_redirect")`. `Spec ref: Sc 20.1, 20.2`
- [ ] 5.8 Wire `create_ticket` live → `AddCompletedAction("create_ticket")` + `SetCurrentStep("ticket_created")` + `SetCollectedData{"ticket_id", id}`. Wire `request_human` live → `AddCompletedAction("request_human")` + `SetCurrentStep("transferred_to_human")`. `Spec ref: Sc 20.3`
- [ ] 5.9 `cargo check` — zero new errors/warnings. Gate for Phase 6.

## Phase 6: RunnerOutput + `run_turn` / `build_system_instruction` (`src/modules/ai_agent/runner.rs` + `sandbox.rs`)

- [ ] 6.1 Add `pub state_patches: Vec<StatePatch>` to `RunnerOutput`; init to `Vec::new()` at struct construction. `Spec ref: Sc 17`
- [ ] 6.2 Add `state_patches_acc: Vec<StatePatch>` accumulator inside `run_turn`'s tool-execution loop; after each `execute_tool`: success → `extend(result.state_patches)`, failure → `push(AddFailedAttempt{tool, error})`. Thread into `RunnerOutput.state_patches`. `Spec ref: Sc 17.1, 17.2`
- [ ] 6.3 Add `conversation_state: Option<&str>` parameter to `build_system_instruction` (8th position, after `turn_state`, before `prompt_vars`). Inject `[conversation_state]` chunk between `[turn_state]` and `[faqs]` when `Some` and non-empty. `Spec ref: Sc 18.1, 18.2`
- [ ] 6.4 Add matching `conversation_state: Option<&str>` parameter to `run_turn` (after `turn_state`); forward to `build_system_instruction`. `Spec ref: Sc 18`
- [ ] 6.5 Mirror new `run_turn` signature in `src/modules/ai_agent/sandbox.rs` — pass `conversation_state: None` (sandbox is stateless). `Spec ref: Sc 18`
- [ ] 6.6 `cargo check` — zero new errors/warnings. Gate for Phase 7.

## Phase 7: Dispatch lifecycle wiring (`src/modules/ai_agent/dispatch.rs`)

- [ ] 7.1 Initialize `all_state_patches: Vec<StatePatch>` before the chain loop. After each `run_turn` call inside the loop, `all_state_patches.extend(output.state_patches)`. `Spec ref: Sc 17, 19.2`
- [ ] 7.2 Before the chain loop: if `config.enable_ai_conversation_state` AND `conv.ai_conv_state.is_some()`, call `format_conversation_state` and pass as `conversation_state` to `run_turn`. Otherwise pass `None`. `Spec ref: Sc 19.1, 23.1`
- [ ] 7.3 After the chain loop (inside lock window): if kill switch off, skip to 7.8. Otherwise: synthetic intent — if `conv.ai_conv_state.current_intent.is_none()` AND `customer_explicit_intents.first().is_some()` → `all_state_patches.insert(0, SetIntent { intent: first, confidence: 1.0 })`. `Spec ref: Sc 19.3, 19.4`
- [ ] 7.4 Fold patches: `let current = conv.ai_conv_state.clone().unwrap_or_default(); let mut new_state = apply_state_patches(current, &all_state_patches)`. `Spec ref: Sc 19.2`
- [ ] 7.5 Transfer-reset: if `new_state.current_step.as_deref().map_or(false, |s| s.starts_with("transferred_to_"))` → set `new_state.current_intent = None; new_state.intent_confidence = None`. `Spec ref: Sc 20.1`
- [ ] 7.6 Call `db.update_conversation_ai_conv_state(conv_id, Some(&new_state))`; log warn on error (non-fatal). `Spec ref: Sc 19.2`
- [ ] 7.7 WS broadcast `CONVERSACION_ESTADO_IA` after write: if `new_state != old_state` (deep-eq via `PartialEq` derive or field-by-field), broadcast `{ tipo, conversation_id, ai_conv_state: new_state }` to all `WsRegistry` connections. Skip if no change. `Spec ref: Sc 24.1, 24.2, 24.3`
- [ ] 7.8 `cargo check` — zero new errors/warnings. Gate for Phase 8.

## Phase 8: Reopen hook (`src/db/mongo/whatsapp.rs` + `whatsapp/handler.rs`)

- [ ] 8.1 In `reopen_conversation` (`db/mongo/whatsapp.rs`): extend the existing `$unset` block to also include `"aiConvState": ""`. Single atomic update — no new DB call. Add `tracing::info!("ai_conv_state cleared on reopen conv={}")`. `Spec ref: Sc 20.4`
- [ ] 8.2 In the handler reopen flow (`whatsapp/handler.rs`): after `update_conversation_ai_state` call, broadcast `CONVERSACION_ESTADO_IA` with `ai_conv_state: null`. `Spec ref: Sc 24`
- [ ] 8.3 `cargo check` — zero new errors/warnings. Gate for Phase 9.

## Phase 9: Reset endpoint (`whatsapp/handler.rs` + `axum_router.rs` + `openapi.rs`)

- [ ] 9.1 Add `ResetAiStateResponse { ok: bool, conversation_id: String }` struct with `Serialize/ToSchema` in `whatsapp/handler.rs`. `Spec ref: Sc 22.3`
- [ ] 9.2 Implement `reset_ai_conv_state_handler`: parse `conv_id`, check `claims.b_can_chat == true AND claims.n_role in [0.0, 0.5, 1.0]` → 403 `forbidden` otherwise; `find_conversation_by_id` → 404; `try_lock_ai_dispatch` → 503/locked; `update_conversation_ai_conv_state(conv_id, None)`; write `WaAudit { action="ai_conv_state_reset", actor_id, actor_name, target_id=conv_id, note, created_at }`; `tracing::info!`; broadcast `CONVERSACION_ESTADO_IA` with `ai_conv_state: null`; release lock; return `{ ok: true, conversation_id }`. `Spec ref: Sc 22.3, 22.4`
- [ ] 9.3 Add `#[utoipa::path]` annotation to handler with path `/v1/auth-user/whatsapp/conversations/{id}/agent-state/reset`, security bearerAuth, response shapes. `Spec ref: Sc 22.3`
- [ ] 9.4 Register route in `src/axum_router.rs` under `user_protected` group: `POST /v1/auth-user/whatsapp/conversations/:id/agent-state/reset`. `Spec ref: Sc 22.3`
- [ ] 9.5 Register path + schemas in `src/openapi.rs`: `reset_ai_conv_state_handler`, `ResetAiStateResponse`, `WaConversationAiState`, `FailedAttempt`. `Spec ref: Sc 22.3`
- [ ] 9.6 `cargo check` — zero new errors/warnings. Gate for Phase 10.

## Phase 10: UI propagation + final check

- [ ] 10.1 In `whatsapp/handler.rs::conv_to_item`, propagate `ai_conv_state: c.ai_conv_state.clone()` to `ConversationItem`. `Spec ref: Sc 22.1`
- [ ] 10.2 Verify `GET /v1/auth-user/whatsapp/conversations/:id` response includes `ai_conv_state` when non-None. `Spec ref: Sc 22.1`
- [ ] 10.3 Final `cargo check` — zero new warnings end-to-end.
- [ ] 10.4 Smoke test checklist (manual, dev environment): intent derivation → lookup_customer patch → check_coverage patch → transfer clears intent → reset endpoint (success + 403) → kill switch off confirms no state writes. `Spec ref: Sc 15–24`

---

## Dependency notes

- Phases 1–4 are strict prerequisites for everything downstream (types, config, DB).
- Phase 5 depends only on Phase 1 (needs `StatePatch`).
- Phase 6 depends on Phases 1 + 5 (needs `StatePatch` + `ToolResult`).
- Phase 7 depends on Phases 2 + 3 + 4 + 6 (needs `state.rs`, DB impl, config, runner sig).
- Phase 8 depends on Phase 3 (DB) + Phase 7 (dispatch pattern reference only).
- Phase 9 depends on Phases 3 + 7 (DB method + WS broadcast helper already wired).
- Phase 10 depends on Phase 1 (ConversationItem field already added in 1.5).
- Phases 5 and 4 can proceed in parallel after Phase 3 completes.
- Phases 8, 9, and 10 can proceed in parallel after Phase 7 completes.
