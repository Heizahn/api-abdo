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
