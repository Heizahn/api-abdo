# Tasks: AI Agent — `report_payment` tool

## Phase 1: Schema (`PaymentReport.id_creator`)

- [x] 1.1 In `src/models/payment.rs`, add field to `PaymentReport` after `rejection_reason`:
  `#[serde(rename = "idCreator", skip_serializing_if = "Option::is_none", default)]`
  `pub id_creator: Option<String>,`
  Spec ref: *PaymentReport id_creator Field* — backwards compat + Scenario: Backwards compatibility
- [x] 1.2 In `src/modules/payments/handler.rs`, add `id_creator: None` to both `PaymentReport` struct literals (lines ~425 and ~620).
  Spec ref: Design §2.1 — "ONE side effect on existing code"
- [x] 1.3 `cargo check` — zero errors, zero new warnings.

## Phase 2: Refactor `ai_agent_secret` visibility

- [x] 2.1 In `src/modules/ai_agent/dispatch.rs`, change `fn ai_agent_secret()` to `pub(super) fn ai_agent_secret()`.
  Spec ref: Design §2.4 imports note, ADR Q4
- [x] 2.2 `cargo check` — confirm `dispatch.rs` and `sandbox.rs` still build.

## Phase 3: Tool implementation (`tools.rs`)

- [x] 3.1 Add `pub const T_REPORT_PAYMENT: &str = "report_payment";` near the other `T_*` constants.
  Spec ref: Requirement: report_payment Tool
- [x] 3.2 Add `tool_default()` arm for `T_REPORT_PAYMENT` with description (LLM instructions) and JSON schema (9 properties, `required: [client_id, reference, media_id]`).
  Spec ref: Input contract table in spec
- [x] 3.3 Add `T_REPORT_PAYMENT` to the `ToolCategory::Action` arm in `tool_category()`.
  Spec ref: MODIFIED Tool Categorization — Scenario: report_payment as Action
- [x] 3.4 Add arm in `execute_tool()`: `T_REPORT_PAYMENT => exec_report_payment(args, ctx, started).await`
  Spec ref: Requirement: report_payment Tool
- [x] 3.5 Add `ReportPaymentArgs` struct with `#[derive(Deserialize)]` and all 9 fields (3 required, 6 with `#[serde(default)]`).
  Spec ref: Input contract table
- [x] 3.6 Add import `use super::dispatch::ai_agent_secret;` and `use crate::crypto::aes::decrypt_payload;` if not present.
  Spec ref: Design §2.4 imports
- [x] 3.7 Verify `find_client_by_id` impl in `src/db/mongo/profile.rs` returns `Err(...)` (not panic) on "not found". If it panics or returns a non-error for missing, add a guard or use the appropriate variant.
  FINDING: Returns `Ok(fake_client)` on "not found". Added guard: compare `client._id != client_oid` to detect missing.
  Spec ref: Design §5 Q5 — flagged risk
- [x] 3.8 Implement `exec_report_payment` following the exact 15-step order from design §2.4:
  1. Parse args → `invalid_args`
  2. Validate `media_id` non-empty → `image_required`
  3. Validate `reference` non-empty → `reference_required`
  4. Validate amount XOR → `amount_required` / `amount_conflict` / `invalid_amount`
  5. Sandbox short-circuit → synthetic payload
  6. Parse `client_id` → `invalid_client_id`
  7. `find_client_by_id` → `client_not_found` (with Ok-fake guard)
  8. `check_reference` → `already_registered: true` short-circuit
  9. `find_client_owner_by_id` + `find_user_payment_info_by_id` → `payment_method_not_configured`
  10. Exchange rate (Redis → DB fallback) → `exchange_rate_unavailable` / `exchange_rate_zero`
  11. IVA rate (`find_tax_by_id`, default 1.0)
  12. Compute missing amount with `round2`
  13. `find_wa_settings_by_id` + decrypt token + build `WhatsAppService` + optional relay → `download_media`
  14. Save bytes to `uploads/{Uuid::new_v4()}.{ext}` (mime→ext mapping: png/webp/gif, default jpg)
  15. Build `PaymentReport { id_creator: Some(ctx.ai_user_id.clone()), state: "Pendiente", ... }` → `create_payment_report`

  Spec ref: All 18 scenarios in spec (happy path, idempotency, amount derivations, all error codes)
- [x] 3.9 `cargo check` — zero errors, zero new warnings.

## Phase 4: Verification

- [x] 4.1 Final `cargo check` across the full workspace — zero warnings.
- [x] 4.2 Regression check: confirm the existing 8 `tool_category()` arms are unchanged (no accidental removal or category shift).
  Spec ref: MODIFIED Tool Categorization table — all 8 pre-existing tools
- [ ] 4.3 Manual smoke test plan (post-deploy, shadow agent):
  - Happy path `amount_usd` only → verify `PaymentReports` doc has `idCreator = ai_user_id`, `sImageUrl = /uploads/<uuid>.jpg`, computed `nBs`.
    Spec ref: Scenario: Successful registration, Scenario: id_creator persistence, Scenario: amount_usd only — derive amount_bs
  - Happy path `amount_bs` only → verify `nAmountUSD` derived correctly.
    Spec ref: Scenario: amount_bs only — derive amount_usd
  - Re-call same `(client_id, reference)` → `already_registered: true`, no new doc, no Meta CDN hit.
    Spec ref: Scenario: Idempotent re-call
  - Empty `media_id` → `image_required` before any I/O.
    Spec ref: Scenario: image_required
  - Both amounts → `amount_conflict`.
    Spec ref: Scenario: amount_conflict
  - Negative amount → `invalid_amount`.
    Spec ref: Scenario: invalid_amount — non-positive value
  - Unknown `client_id` → `client_not_found`.
    Spec ref: Scenario: client_not_found
  - Sandbox mode → `mode: "sandbox"`, `payment_id: "sandbox-fake-payment"`, no DB/FS write.
    Spec ref: Scenario: Sandbox mode — no side effects
  - Existing doc without `idCreator` field → deserializes as `None` without error.
    Spec ref: Scenario: Backwards compatibility — existing docs without idCreator

## Parallelism notes

- Phases are strictly sequential: Phase 1 → Phase 2 → Phase 3 → Phase 4.
- Within Phase 3: tasks 3.1–3.7 can be done in any order; task 3.8 depends on all of them; 3.9 closes the phase.
- Phase 4 tasks 4.2 and 4.3 can run in parallel once 4.1 passes.
