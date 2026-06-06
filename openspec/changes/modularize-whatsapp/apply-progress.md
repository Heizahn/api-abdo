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

- Re-run after final warning cleanup:
  - `cargo check --tests`
  - `cargo test`

## Task Status Impact

- `2.2`: now partially advanced (query/read extraction only; no messaging/aux list body migration)
- `3.3`: unchanged

## PR2f: Conversation list query extraction

Status: in progress

Branch: `feature/modularize-whatsapp-pr2f-conversation-list`

## Completed

- Moved `list_conversations_handler` query/list logic from `handler.rs` to `conversations/queries.rs`:
  - `ConversationsQuery`
  - `list_conversations_handler`
  - `resolve_last_message_agent_names`
  - `resolve_assigned_agent_names`
- In `conversations/queries.rs`, updated list handler mapping to use
  `crate::modules::whatsapp::shared::response::conv_to_item` directly.
- Updated `src/modules/whatsapp/conversations/handlers.rs` to re-export:
  - `list_conversations_handler`
  - `__path_list_conversations_handler`
  from `conversations::queries`.
- Preserved all non-assigned handlers in `handler.rs` (messages lifecycle, messaging,
  lifecycle, webhook, settings/media/WS/template/quick-reply code, routes, payload contracts, DB traits, and OpenAPI semantics).
- Applied version bump in versioned artifacts:
  - `Cargo.toml`
  - `src/openapi.rs`
  - `src/main.rs`

## Notes

- Task `2.2` remains intentionally **unchecked/partial** after this PR slice; further conversation flow handlers remain in `handler.rs` by design for this bounded change.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `git diff --check`

## Task Status Impact

- `2.2`: still partially advanced (query/list extraction now includes list handler; implementation body migration continues)
- `3.3`: unchanged

## PR2g: Conversation messages query extraction slice

Branch: `feature/modularize-whatsapp-pr2g-conversation-messages`

## Completed

- Moved `get_conversation_messages_handler` and `MessagesQuery` from
  `src/modules/whatsapp/handler.rs` into
  `src/modules/whatsapp/conversations/queries.rs`.
- Moved/ported `resolve_sent_by_names` to `conversations/queries.rs` and wired
  it into the moved query handler flow.
- Updated query mapping to use shared mapper helpers directly:
  - `crate::modules::whatsapp::shared::mappers::msg_to_item`
  - `crate::modules::whatsapp::shared::mappers::resolve_reply_to_items`
- Preserved `record_conversation_open` call/semantics exactly as before for GET
  `/v1/auth-user/whatsapp/conversations/{id}/messages`:
  - executed on every successful read path,
  - warning-only handling on DB failure,
  - unchanged comments/behavioral meaning.
- Updated `src/modules/whatsapp/conversations/handlers.rs` to re-export:
  - `get_conversation_messages_handler`
  - `__path_get_conversation_messages_handler`
  from `conversations::queries`.
- Kept legacy handlers (send/mark/lifecycle/settings/media/webhook/template/quick-replies/WS/contracts/DB traits/OpenAPI semantics) in
  `src/modules/whatsapp/handler.rs`.
- Bumped project version metadata in all standard artifacts:
  - `Cargo.toml`
  - `src/main.rs`
  - `src/openapi.rs`

## Notes

- Task `2.2` remains intentionally **unchecked/partial** after this bounded
  PR slice, as required. Remaining message/lifecycle logic is still in
  `handler.rs`.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.2`: remains unchecked/partial (query/list/messaging read extraction only in
  this slice)
- `3.3`: unchanged

## PR2h: Conversation mark-read extraction

Branch: `feature/modularize-whatsapp-pr2h-conversation-mark-read`

## Completed

- Moved `mark_read_handler` from `src/modules/whatsapp/handler.rs` to
  `src/modules/whatsapp/conversations/lifecycle.rs` and preserved behavior.
- Updated `src/modules/whatsapp/conversations/handlers.rs` to re-export
  `mark_read_handler` and `__path_mark_read_handler` from
  `conversations::lifecycle`.
- Extracted WhatsApp service helpers into `src/modules/whatsapp/shared/service.rs`:
  - `resolve_service_for_phone`
  - `settings_secret`
  - `apply_media_relay`
- Updated `src/modules/whatsapp/handler.rs` callsites for those helpers to use the
  shared implementation.
- Bumped version metadata for this PR slice:
  - `Cargo.toml`
  - `src/openapi.rs`
  - `src/main.rs`

## Notes

- Task `2.2` remains **unchecked/partial** because this PR intentionally extracts
  only `mark_read` lifecycle ownership; other conversation handlers, messaging,
  settings, quick reply, and template flows remain in legacy `handler.rs`.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.2`: remains unchecked/partial (mark-read slice only)
- `3.3`: unchanged

## PR2i: Conversation take extraction

Status: complete

Branch: `feature/modularize-whatsapp-pr2i-conversation-take`

## Completed

- Moved `take_conversation_handler` from `src/modules/whatsapp/handler.rs` to
  `src/modules/whatsapp/conversations/lifecycle.rs`, preserving behavior and
  route contract.
- Updated the moved handler to invoke `state.db.record_conversation_event(...)`
  directly with best-effort warning on persistence failure (no change in
  caller-facing result).
- Reused shared helpers directly in the moved code:
  - `shared::authz::{require_can_chat, require_workspace_actor_for_conversation}`
  - `shared::workspace::resolve_workspace_name`
  - `shared::mappers::{resolve_customer_name, resolve_last_message_agent_name_one}`
  - `shared::response::conv_to_item`
- Re-exported `take_conversation_handler` and `__path_take_conversation_handler`
  from `src/modules/whatsapp/conversations/handlers.rs` via
  `conversations::lifecycle`.
- Kept transfer/close/reopen/send/initiate/mark-read and route ownership for all
  non-targeted conversation/message domains in legacy `handler.rs`.
- Bumped version metadata for PR2i:
  - `Cargo.toml`
  - `src/openapi.rs`
  - `src/main.rs`

## Notes

- Task `2.2` remains **unchecked/partial** for PR2 as requested.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.2`: remains unchecked/partial (take handler extracted; task remains scoped)
- `3.3`: unchanged

## PR2j: Conversation transfer extraction slice

Branch: `feature/modularize-whatsapp-pr2j-conversation-transfer`

## Completed

- Moved `transfer_conversation_handler` from `src/modules/whatsapp/handler.rs` to
  `src/modules/whatsapp/conversations/lifecycle.rs`, preserving behavior.
- In the moved handler, switched to shared helpers directly:
  - `shared::authz::{require_can_chat, require_workspace_actor_for_conversation,
    ensure_transfer_target_allowed_for_workspace}`
  - `shared::workspace::resolve_workspace_name`
  - `shared::mappers::{resolve_customer_name, resolve_last_message_agent_name_one,
    resolve_assigned_agent_name_one}`
  - `shared::response::conv_to_item`
- Updated `conversations/handlers.rs` to re-export `transfer_conversation_handler`
  and `__path_transfer_conversation_handler` from `conversations::lifecycle`.
- Kept `record_conv_event` in `handler.rs` unchanged and used
  `state.db.record_conversation_event(...)` directly in the moved transfer
  implementation (best-effort warning on persistence failure).
- Kept all legacy conversation handlers and other WhatsApp flows untouched:
  `close/reopen/send/initiate/intervene/reset/list-transferable/webhook/media/settings/templates/quick replies`.
- Bumped version metadata:
  - `Cargo.toml`
  - `src/main.rs`
  - `src/openapi.rs`

## Notes

- Task `2.2` remains **unchecked/partial** for PR2j by scope (single transfer
  extraction only).

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.2`: remains unchecked/partial (transfer ownership moved; implementation body
  extraction continues)
- `3.3`: unchanged

## PR2k: Conversation close/reopen lifecycle extraction

Branch: `feature/modularize-whatsapp-pr2k-conversation-close-reopen`

## Completed

- Moved `close_conversation_handler` and `reopen_conversation_handler` from
  `src/modules/whatsapp/handler.rs` to
  `src/modules/whatsapp/conversations/lifecycle.rs`, preserving behavior.
- In moved handlers, switched to direct `state.db.record_conversation_event(...)`
  calls with best-effort warning logs on failure (no caller-facing impact).
- Updated moved handlers to call shared helpers directly:
  - `shared::workspace::resolve_workspace_name`
  - `shared::mappers::{resolve_customer_name, resolve_last_message_agent_name_one,
    resolve_assigned_agent_name_one}`
  - `shared::response::conv_to_item`
- Updated `src/modules/whatsapp/conversations/handlers.rs` to re-export:
  `close_conversation_handler`, `reopen_conversation_handler`,
  `__path_close_conversation_handler`, `__path_reopen_conversation_handler`
  from `conversations::lifecycle`.
- Preserved all non-requested scope handlers in `src/modules/whatsapp/handler.rs`:
  send/initiate/intervene/reset/list-transferable/webhook/media/settings/templates/
  quick replies, routes, payload contracts, DB traits, WS event schemas.
- Bumped version metadata for PR2k:
  - `Cargo.toml`
  - `src/openapi.rs`
  - `src/main.rs`

## Notes

- Task `2.2` remains **unchecked/partial** as requested: close/reopen ownership
  moved only.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.2`: remains **unchecked/partial** (close/reopen lifecycle extraction only)
- `3.3`: unchanged

## PR2l: Conversation AI controls extraction

Status: complete

Branch: `feature/modularize-whatsapp-pr2l-conversation-ai-controls`

## Completed

- Moved AI control endpoint definitions and handlers out of `src/modules/whatsapp/handler.rs` into `src/modules/whatsapp/conversations/lifecycle.rs`:
  - `ResetAiStateResponse`
  - `InterveneData`
  - `InterveneResponse`
  - `reset_ai_conv_state_handler`
  - `intervene_conversation_handler`
- In moved handlers, kept authorization/side-effect behavior consistent and switched to
  direct `state.db.record_conversation_event(...)` persistence (best-effort warning on DB failures).
- Updated `src/modules/whatsapp/conversations/handlers.rs` to re-export `__path_*` and handler symbols from
  `conversations::lifecycle` for:
  - `intervene_conversation_handler`
  - `reset_ai_conv_state_handler`
- Updated OpenAPI type imports in `src/openapi.rs` to reference:
  - `crate::modules::whatsapp::conversations::lifecycle::{InterveneData, InterveneResponse, ResetAiStateResponse}`.
- Project version metadata was kept aligned after PR2l changes:
  - `Cargo.toml`
  - `src/main.rs`
  - `src/openapi.rs`

## Notes

- Task `2.2` remains intentionally **unchecked/partial** by design: conversation messaging/send/list/transfer/close/reopen etc remain in `handler.rs` by bounded scope.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.2`: remains **unchecked/partial** (conversation AI controls moved; conversation flow extraction continues)
- `3.3`: unchanged

## PR3d: Messaging send handler extraction

Branch: `feature/modularize-whatsapp-pr3d-send-handler`

Status: complete

## Completed

- Moved full ownership of `send_message_handler` from `src/modules/whatsapp/handler.rs` to
  `src/modules/whatsapp/messaging/send.rs`:
  - `send_message_handler` implementation and OpenAPI annotation
  - `require_workspace_agent_or_assigned` helper (private helper needed only by this handler)
- Updated `src/modules/whatsapp/conversations/handlers.rs` to re-export send symbols from
  `crate::modules::whatsapp::messaging::send` so route and OpenAPI entry points remain stable:
  - `send_message_handler`
  - `__path_send_message_handler`
- Removed now-orphaned send-only helpers from `src/modules/whatsapp/handler.rs`:
  - `send_message_handler`
  - `require_workspace_agent_or_assigned`
- Preserved behavior-sensitive logic paths (DB write flow, idempotency, WS events, quick-reply increment,
  `require_can_chat`/workspace checks, template media header auto-fill, reaction handling context).
- Applied patch-version metadata bump to:
  - `Cargo.toml`
  - `Cargo.lock`
  - `src/main.rs`
  - `src/openapi.rs`
- Kept task `2.3` status as partial because `media.rs` / `reactions.rs` still remain in `handler.rs` by scope.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.3`: still **partial/unchecked** (`send_message_handler` moved, but media/reaction modules still pending)
- `3.3`: unchanged

## PR2m: Transferable agents query extraction slice

Branch: `feature/modularize-whatsapp-pr2m-transferable-agents`

## Completed

- Moved `TransferableAgentsQuery` and `list_transferable_agents_handler` from
  `src/modules/whatsapp/handler.rs` into
  `src/modules/whatsapp/conversations/queries.rs`.
- In the moved handler, switched authz resolution to direct `shared::authz` helpers:
  - `require_can_chat`
  - `is_superadmin`
  - `is_chat_workspace_match`
  - `is_transfer_target_allowed_for_workspace`
  - `is_transfer_target_allowed_for_actor_workspaces`
- Reused the existing `normalize_to_e164` in `conversations/queries` for
  `business_phone` filtering.
- Updated `src/modules/whatsapp/conversations/handlers.rs` to re-export:
  - `list_transferable_agents_handler`
  - `__path_list_transferable_agents_handler`
  from `conversations::queries`.
- Kept route path, OpenAPI registration path references, response shape, DB traits,
  route registration, settings/media/webhook/send/initiate/quick replies/templates behavior
  unchanged.
- Applied version bump for this PR2m slice:
  - `Cargo.toml`
  - `src/openapi.rs`
  - `src/main.rs`

## Notes

- Task `2.2` remains intentionally **unchecked/partial** by scope (transferable
  agents query extraction only).

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.2`: remains **unchecked/partial** (transferable-agent query extracted; full task remains partial by design)
- `3.3`: unchanged

## PR2n: Conversation initiation ownership extraction slice

Branch: `feature/modularize-whatsapp-pr2n-initiate-conversation`

Status: complete

## Completed

- Added `src/modules/whatsapp/conversations/outbound.rs` with implementation ownership for `initiate_conversation_handler` and its local helpers:
  - `initiate_conversation_handler`
  - `auto_fill_template_header_media`
  - `map_template_send_error`
  - `normalize_to_e164`
- Wired the new module in `src/modules/whatsapp/conversations/mod.rs`.
- Updated `src/modules/whatsapp/conversations/handlers.rs` to re-export:
  - `initiate_conversation_handler`
  - `__path_initiate_conversation_handler`
  from `conversations::outbound`.
- Removed the legacy `initiate_conversation_handler` implementation from `src/modules/whatsapp/handler.rs`; route and OpenAPI callsites continue through the `conversations::handlers` facade, which now re-exports the canonical implementation from `conversations::outbound`.
- Kept route and OpenAPI registration unchanged (`src/modules/whatsapp/mod.rs`, `src/openapi.rs` still target `conversations::handlers`).
- Applied project metadata version bump for this PR2n slice:
  - `Cargo.toml`
  - `src/openapi.rs`
  - `src/main.rs`

## Notes

- Task `2.2` remains **unchecked/partial** by scope: messaging (`send_message_handler`), webhook/WS, templates/media/settings/quick replies still remain in `handler.rs`; this slice only migrates the conversation initiation ownership.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.2`: remains **partial/unchecked** (initiation ownership moved; conversation implementation migration still ongoing)
- `3.3`: unchanged

## PR3a: Messaging preview helper extraction

Branch: `feature/modularize-whatsapp-pr3a-send-message`

## Completed

- Added `src/modules/whatsapp/messaging/` module boundary:
  - `mod.rs`
  - `preview.rs`
- Moved preview helpers only (low coupling) from `src/modules/whatsapp/handler.rs` to `src/modules/whatsapp/messaging/preview.rs`:
  - `interactive_preview`
  - `template_preview`
- Updated `src/modules/whatsapp/handler.rs` callsites to use
  `crate::modules::whatsapp::messaging::preview::{interactive_preview, template_preview}`.
- Added `pub mod messaging;` in `src/modules/whatsapp/mod.rs`.
- Preserved behavior and output strings of moved helpers.
- Explicitly kept `send_message_handler`, `SendMode`, `SentData`, `resolve_send_mode`,
  `dispatch_send`, media/template header helpers, reactions, webhook, conversations,
  settings, quick replies, templates, routes, OpenAPI path symbols, DB traits, and WS
  schemas unchanged.
- Kept task `2.3` in `tasks.md` unchecked and marked as partial for this bounded safe
  pre-step.
- Bumped version metadata for this PR:
  - `Cargo.toml`
  - `src/openapi.rs`
  - `src/main.rs`

## Notes

- This batch is a partial extraction only (`PR3a` safe pre-step).

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.3`: unchanged/partial (preview helper extraction only)
- `3.3`: unchanged

## PR3b: Messaging send-mode extraction

Branch: `feature/modularize-whatsapp-pr3b-send-helpers`

## Completed

- Added `src/modules/whatsapp/messaging/mode.rs` and moved send-mode resolution logic from `src/modules/whatsapp/handler.rs`:
  - `SendMode`
  - `resolve_send_mode`
  - private helper functions:
    - `freeform_window_expired_error`
    - `nonempty`
    - `validate_media_id`
- Exported messaging submodule in `src/modules/whatsapp/messaging/mod.rs` with `pub mod mode;`.
- Updated `src/modules/whatsapp/handler.rs` imports to consume:
  - `super::messaging::mode::{resolve_send_mode, SendMode}`
- Removed legacy `resolve_send_mode_local` from `handler.rs` so the callsite now resolves the helper from `messaging::mode`.
- Kept task `2.3` in `tasks.md` as unchecked (partial), with explicit scope note that this PR only moves send-mode plumbing.
- Applied version bump for this slice:
  - `Cargo.toml`
  - `Cargo.lock`
  - `src/openapi.rs`
  - `src/main.rs`

## Notes

- This batch still does **not** extract `dispatch_send`, `SentData`, or other messaging ownership slices.
- OpenAPI/path registrations and send handler behavior remain unchanged.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.3`: unchanged/partial (send-mode extraction only, full task 2.3 still pending)
- `3.3`: unchanged

## PR3c: Messaging dispatch/send data extraction

Branch: `feature/modularize-whatsapp-pr3c-dispatch-send`

## Completed

- Added `src/modules/whatsapp/messaging/send.rs` and moved dispatch ownership types +
  function from `handler.rs`:
  - `TemplateFields`
  - `SentData`
  - `dispatch_send`
- Updated `src/modules/whatsapp/messaging/mod.rs` with `pub mod send;`.
- Updated `src/modules/whatsapp/handler.rs` imports and callsites:
  - `dispatch_send` now imported from `super::messaging::send::dispatch_send`.
  - `auto_fill_template_header_media` callsites switched to the existing helper in
    `conversations::outbound`.
- Removed private `handler.rs` duplicates that were already safely owned in other
  modules:
  - `auto_fill_template_header_media`
  - `map_template_send_error`
  (logic preserved by delegating to `crate::modules::whatsapp::conversations::outbound`).
- Kept `send_message_handler` and all behavior-sensitive WhatsApp flows in `handler.rs`
  unchanged per bounded slice.
- Kept task `2.3` intentionally **unchecked/partial** (send handler remains in
  `handler.rs` for this slice).
- Applied version bump for this PR:
  - `Cargo.toml`
  - `Cargo.lock`
  - `src/main.rs`
  - `src/openapi.rs`

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.3`: still unchanged/partial (dispatch/send helper ownership advanced, handler remains).
- `3.3`: unchanged

## PR3e: Messaging reactions extraction

Branch: `feature/modularize-whatsapp-pr3e-reactions`

Status: complete

## Completed

- Added `src/modules/whatsapp/messaging/reactions.rs` and moved reaction ownership out of
  `src/modules/whatsapp/handler.rs`:
  - `ReactMessageRequest`
  - `ReactMessageResponse`
  - `react_message_handler`
  - `handle_inbound_reaction`
- Updated inbound webhook processing in `handler.rs` to delegate reaction handling:
  `super::messaging::reactions::handle_inbound_reaction`.
- Added `pub mod reactions;` to `src/modules/whatsapp/messaging/mod.rs`.
- Rewired the existing WhatsApp reaction route in `src/modules/whatsapp/mod.rs` to use:
  `messaging::reactions::react_message_handler`.
- Updated OpenAPI registrations to moved symbols:
  - `crate::modules::whatsapp::messaging::reactions::react_message_handler`
  - `crate::modules::whatsapp::messaging::reactions::ReactMessageRequest`
  - `crate::modules::whatsapp::messaging::reactions::ReactMessageResponse`
- Kept request/response contract, WS event shape, DB update semantics, and reaction
  behaviors unchanged.
- Applied project version bump to `0.3.41` in:
  - `Cargo.toml`
  - `Cargo.lock`
  - `src/main.rs`
  - `src/openapi.rs`
- Kept task `2.3` as partial because `messaging/media.rs` still remains in `handler.rs`.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.3`: partially advanced (reactions extracted; `media.rs` still pending).
- `3.3`: unchanged

## PR3f: Messaging media extraction

Branch: `feature/modularize-whatsapp-pr3f-media`

Status: partial

## Completed

- Added `src/modules/whatsapp/messaging/media.rs` and moved upload/media-limits ownership out of `src/modules/whatsapp/handler.rs`:
  - `upload_media_handler`
  - `get_media_limits_handler`
  - media constants/constants helpers used by upload validation:
    - `MIME_*`
    - `MAX_*`
    - `human_bytes`
    - `media_type_label`
    - `media_type_limits`
    - `infer_type_from_mime`
- Exported `media` module in `src/modules/whatsapp/messaging/mod.rs` with `pub mod media;`.
- Rewired WhatsApp routes in `src/modules/whatsapp/mod.rs` to use moved handlers:
  - `GET /v1/auth-user/whatsapp/media/limits`
  - `POST /v1/auth-user/whatsapp/media`
- Updated OpenAPI registration path references to moved symbols in `src/openapi.rs`:
  - `crate::modules::whatsapp::messaging::media::upload_media_handler`
  - `crate::modules::whatsapp::messaging::media::get_media_limits_handler`
- Kept endpoint contracts unchanged:
  - multipart parsing, validation messages, max-size checks, MIME allowlist behavior,
  - SHA-256 computation, conversation lookup flow, and Meta upload flow.
- Kept route order and `DefaultBodyLimit::disable()` behavior on upload endpoint.
- Applied project version bump to `0.3.42` in:
  - `Cargo.toml`
  - `Cargo.lock`
  - `src/main.rs`
  - `src/openapi.rs`
- Kept media download/proxy/cache/template-header upload in `handler.rs` for a later slice.

## Verification

- `cargo fmt --check`
- `cargo check`
- `cargo check --tests`
- `cargo test`
- `git diff --check`

## Task Status Impact

- `2.3`: partially advanced (upload + limits extracted; download/proxy/cache/template-header upload still pending)
- `3.3`: unchanged
