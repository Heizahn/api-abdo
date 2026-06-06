# Apply Progress: Modularize WhatsApp Module

## PR1: Shared Helpers Foundation

Status: partially complete

Branch: `feature/modularize-whatsapp-pr1-shared`

## Completed

- Created `src/modules/whatsapp/shared/` with focused helper modules:
  - `authz.rs`
  - `mappers.rs`
  - `response.rs`
  - `time.rs`
  - `workspace.rs`
  - `mod.rs`
- Registered `shared` in `src/modules/whatsapp/mod.rs`.
- Moved behavior-preserving helper logic out of `handler.rs` where safe.
- Updated internal callers to use shared helpers:
  - `src/modules/ai_agent/dispatch.rs`
  - `src/modules/ai_agent/escalation.rs`
  - `src/modules/whatsapp/audit.rs`
  - `src/modules/whatsapp/tickets.rs`
  - `src/modules/whatsapp/url_preview.rs`
- Preserved compatibility wrappers in `handler.rs` for existing callers.

## Task Status

- Completed: 1.1, 1.2
- Partially advanced: 1.3
- Remaining: 1.3 full facade cleanup, all Phase 2-4 tasks

## Verification

- `cargo check` passed during apply.
- Fresh review reported helper moves as behavior-preserving by inspection.
- `git diff --check` passed during review.

## Rollback Boundary

Revert the PR1 commit to restore the pre-extraction helper layout. No route paths, payloads, DB traits, OpenAPI registrations, WebSocket event schemas, or deployment wiring were intentionally changed.

## Known Follow-up

`handler.rs` is not yet a re-export-only facade. It still contains substantial handler implementation and compatibility wrappers. Full facade cleanup should happen only after domain extraction slices move webhook, conversation, messaging, settings, quick replies, and template handlers safely.

## PR2: Webhook + Conversations Domain Wiring

Status: in progress

Branch: `feature/modularize-whatsapp-pr2-webhook-conversations`

## Completed

- Added webhook domain module boundary: `src/modules/whatsapp/webhook/`
  - `mod.rs`
  - `handler.rs` (compatibility forwarding for `verify_webhook`, `receive_webhook`, `debug_last_webhook_handler`)
  - `normalize.rs`
  - `media_failures.rs`
  - `status.rs`
- Added conversations domain module boundary: `src/modules/whatsapp/conversations/`
  - `mod.rs`
  - `handlers.rs` (compatibility forwarding for conversation REST endpoints)
  - `queries.rs`
  - `lifecycle.rs`
- Rewired WhatsApp routing in `src/modules/whatsapp/mod.rs` to use:
  - `webhook::handler::*` for webhook routes
  - `conversations::handlers::*` for conversation routes
  - kept route paths/order unchanged
- Updated `src/openapi.rs` WhatsApp registration to point moved conversation handlers to
  `crate::modules::whatsapp::conversations::handlers`.
- Updated `openspec/changes/modularize-whatsapp/tasks.md`:
  - 3.1 and 3.2 marked `[x]`
  - 2.1 and 2.2 left unchecked with notes because PR2 created module boundaries/re-exports, but did not move implementation bodies yet

## Verification

- Route inventory parity maintained in `src/modules/whatsapp/mod.rs` during this slice.
- OpenAPI path registrations updated for moved conversation handlers.
- `cargo check` passed after structural rewiring.

## Task Status

- Completed: 1.1, 1.2, 3.1, 3.2
- Partially advanced: 1.3, 2.1, 2.2
- Remaining: 1.3 full facade cleanup, 2.1/2.2 implementation body extraction, 2.3, 2.4, 3.3

## Rollback Boundary (PR2)

- Added modules under `src/modules/whatsapp/webhook/` and
  `src/modules/whatsapp/conversations/`
- Route wiring in `src/modules/whatsapp/mod.rs`
- OpenAPI endpoint references in `src/openapi.rs`

This PR2 rollback is self-contained to structural wiring and does not alter
business logic behavior, route contracts, or response shapes.

## PR2b: Webhook Verify/Debug Ownership Slice

Status: in progress

Branch: `feature/modularize-whatsapp-pr2b-webhook-impl`

## Completed

- Moved webhook ownership scaffolding items from `src/modules/whatsapp/handler.rs` to
  `src/modules/whatsapp/webhook/handler.rs`:
  - `WebhookVerifyParams`
  - `verify_webhook`
  - `debug_last_webhook_handler`
  - `LAST_WEBHOOK_PAYLOAD`
  - `last_payload_store`
  - `verify_meta_signature`
  - `hex_decode`
  - `hex_nibble`
- Kept `receive_webhook` implementation body in `src/modules/whatsapp/handler.rs` and
  re-exported it from `src/modules/whatsapp/webhook/handler.rs`.

## Notes

- This is intentionally a minimal contract-safe slice: only endpoint ownership for
  webhook verify/debug moved. `receive_webhook` remains legacy and still executes in
  place.
- OpenAPI route registrations and documented semantics were not intentionally changed
  in this slice.

## Task Status Impact

- 2.1: partially advanced (simple endpoint ownership only)
- 2.2: still pending

## Verification

- Route wiring remains unchanged because `src/modules/whatsapp/mod.rs` already points
  webhook routes to `webhook::handler::*`.

## Rollback Boundary (PR2b)

- Revert this PR2b commit to restore `WebhookVerifyParams` / verify/debug helpers to
`handler.rs` and `receive_webhook` remains untouched.

## PR2c: Webhook normalization helper extraction

Status: complete

## Completed

- Refactored inbound webhook normalization and metadata helpers out of
  `src/modules/whatsapp/handler.rs` into
  `src/modules/whatsapp/webhook/normalize.rs`:
  - `infer_inbound_effective_type`
  - `inbound_payload_markers`
  - `should_store_raw_payload`
  - `is_known_inbound_type`
  - `inbound_raw_payload`
  - `build_top_level_delta_message`
  - `extract_media_fields`
  - `extract_inbound_content`
  - `first_string_at`
  - `extract_text_from_payload`
  - `normalize_delta_body`
  - `describe_top_level_group`
  - `extract_inbound_payload_target_wa_id`
  - `extract_inbound_delta_target_wa_id`
  - `should_apply_message_delta_update`
- Updated `src/modules/whatsapp/handler.rs` to consume these helpers from
  `super::webhook::normalize`, reducing in-file logic duplication.
- Fixed an extraction-side syntax issue in `group` branch normalization and
  re-ran formatter + typecheck.

## Verification

- `cargo fmt --check` passes after formatting.
- `git diff --check` passes.
- `cargo check` passes with no warnings.

## Rollback Boundary (PR2c)

- Revert this PR2c commit to restore normalization helpers and related logic to
  `src/modules/whatsapp/handler.rs` if needed. No route paths, OpenAPI entries,
  DB queries, webhook status handling, or assignment/media pipelines were changed
  in this slice.

## PR2d: Webhook status helpers micro-extraction

Status: complete

Branch: `feature/modularize-whatsapp-pr2d-webhook-status`

## Completed

- Moved webhook status-only helpers from `src/modules/whatsapp/handler.rs` to
  `src/modules/whatsapp/webhook/status.rs`:
  - `InboundMediaFailureDetails`
  - `impl InboundMediaFailureDetails::from_status_error`
  - `log_webhook_top_level_errors`
  - `is_inbound_media_failure_status` (tiny pure predicate)
  - `has_meta_throttle_131049` (tiny pure predicate)
- Updated `src/modules/whatsapp/handler.rs` call sites to use status helpers via
  `super::webhook::status`.
- Updated release version metadata to keep repo versioning in sync:
  - `Cargo.toml`
  - `src/openapi.rs`
  - `src/main.rs`

## Verification

- `cargo fmt --check` passed.
- `cargo check` passed.
- `cargo check --tests` passed.
- `git diff --check` passed.

## Task Status Impact

- `2.1` remains intentionally partial/unchecked by task definition (webhook
  body extraction in prior PRs; this PR extracts only status helper ownership).

## Rollback Boundary (PR2d)

- Revert this PR2d commit to move the three status helpers back into
`src/modules/whatsapp/handler.rs` and restore the previous call site usage.
  Route wiring, webhook semantics, and payload mutation behavior remain unchanged.

## PR2e: Conversation read/query handler extraction

Status: in progress

Branch: `feature/modularize-whatsapp-pr2e-conversation-read`

## Completed

- Split conversation query/read handlers into `src/modules/whatsapp/conversations/queries.rs`:
  - `ConversationStatsQuery`
  - `conversations_stats_handler`
  - `get_conversation_handler`
  - `get_conversation_client_link_handler`
- Updated `src/modules/whatsapp/conversations/handlers.rs` to re-export moved handlers and `__path_*` symbols from the new module while keeping other conversation handlers in `handler.rs`.
- Preserved `list_conversations_handler` in `src/modules/whatsapp/handler.rs` as required by scope for this slice.
- Version bump applied in manifest and startup/OpenAPI metadata.

## Notes

- Task `2.2` remains **unchecked/partial** because the full conversation modularization path still requires follow-up extraction for remaining query/messaging handlers and module cleanup.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `git diff --check`

## Task Status Impact

- `2.2`: now partially advanced (query/read extraction only; no messaging/aux list body migration)
- `3.3`: unchanged
