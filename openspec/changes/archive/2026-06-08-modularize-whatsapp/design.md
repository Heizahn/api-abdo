# Design: Modularize WhatsApp Module

## Technical Approach

Refactor only inside `src/modules/whatsapp/`, preserving `axum_router.rs` mounting, route paths, auth layers, DB traits, Redis keys, Meta API behavior, WS event schemas, and OpenAPI semantics. The current `handler.rs` remains a compatibility facade during extraction, then shrinks as route-domain modules own handlers and local helpers. This maps to the proposal by reducing coupling without creating a new binary or product behavior.

## Target Module Tree

```text
src/modules/whatsapp/
  mod.rs
  handler.rs                # temporary facade/re-exports, then minimal legacy shim
  shared/{mod.rs,authz.rs,mappers.rs,time.rs,workspace.rs,response.rs}
  webhook/{mod.rs,handler.rs,normalize.rs,media_failures.rs,status.rs}
  conversations/{mod.rs,handlers.rs,lifecycle.rs,queries.rs}
  messaging/{mod.rs,send.rs,reactions.rs,media.rs,preview.rs}
  settings/{mod.rs,handlers.rs,validation.rs}
  quick_replies/{mod.rs,handlers.rs}
  templates/{mod.rs,handlers.rs,meta.rs,header_media.rs}
  audit.rs
  tickets.rs
  service.rs
  ws.rs
  assignment.rs
  backfill.rs
  url_preview.rs
  quick_reply_validation.rs
```

## Architecture Decisions

| Decision | Choice | Alternatives considered | Rationale |
|---|---|---|---|
| Boundary | Route-domain modules under same binary | Separate `waba` binary now | Keeps deployment/config stable and avoids cross-process auth/DB/WS redesign. |
| Shared helpers | Move cross-cutting helpers to `shared::*` with `pub(super)`/`pub(crate)` as needed | Let modules import from `handler.rs` | Breaks circular dependencies; `tickets.rs`, `audit.rs`, and AI modules already depend on handler helpers. |
| Route composition | Keep public functions `webhook_routes`, `ws_routes`, `user_routes` in `mod.rs` | Nest subrouters with stripped prefixes | Existing absolute paths and route order are explicit; preserving order avoids `:id` capturing literals like `test-connection`/`categories`. |
| OpenAPI migration | Move path macros with handlers and update `openapi.rs` paths/schemas per slice | Re-export all handlers forever | Keeps docs tied to owners while allowing temporary re-exports only for clean PR boundaries. |

## Data Flow

```text
Meta webhook -> webhook::handler -> webhook::normalize/status/media_failures
             -> shared::mappers -> DB/Redis -> assignment/ws
Staff REST -> domain handler -> shared::authz/workspace/mappers -> DB/Meta service/ws
AI modules -> shared::mappers::build_message_item -> ws broadcast
```

## Stepwise Extraction Approach

1. Create `shared` modules and move pure helpers first: `iso8601`, response builders, `build_message_item`, `build_conversation_item`, auth/workspace guards. Update AI call sites from `whatsapp::handler::build_message_item` to `whatsapp::shared::mappers::build_message_item`.
2. Extract webhook normalization/status/media-failure logic, keeping `handler.rs` re-exports for `verify_webhook`, `receive_webhook`, and `debug_last_webhook_handler` until routes/OpenAPI are migrated.
3. Extract conversations lifecycle/list/messages-read/take/transfer/close/reopen/initiate. Keep WS calls in `ws.rs`; no new event schema.
4. Extract messaging/media/reactions, then settings, quick replies, and templates. Remove facade re-exports only when `mod.rs` and `openapi.rs` point to final modules.

## File Changes

| File | Action | Description |
|---|---|---|
| `src/modules/whatsapp/shared/*` | Create | Shared authz, workspace, mappers, time/response helpers. |
| `src/modules/whatsapp/{webhook,conversations,messaging,settings,quick_replies,templates}/*` | Create | Domain-owned handlers and private helpers. |
| `src/modules/whatsapp/mod.rs` | Modify | Wire exact same routes in exact same order to new module functions. |
| `src/modules/whatsapp/handler.rs` | Modify | Shrink into temporary facade, then remove dead sections. |
| `src/openapi.rs` | Modify | Replace `handler::...` paths/schemas with final module paths as each slice moves. |
| `src/modules/ai_agent/{dispatch.rs,escalation.rs}` | Modify | Point message mapper import to `shared::mappers`. |

## Interfaces / Contracts

No public API changes. Internal contract: shared helpers expose stable functions such as `require_can_chat`, `require_superadmin`, `build_message_item`, `build_conversation_item`, and workspace guards from `shared`, not from route handlers.

## OpenAPI Migration Approach

Each PR moves `#[utoipa::path]` with the handler function and updates `openapi.rs` in the same slice. Validate that `/docs/openapi.json` path list, tags, request/response schemas, and security intent remain semantically equivalent.

## Chained PR Boundaries

- PR 1: `shared` extraction + AI import migration; no route ownership change.
- PR 2: webhook + conversation read/lifecycle extraction.
- PR 3: messaging/media/reactions + settings extraction.
- PR 4: quick replies/templates cleanup + OpenAPI parity + remove obsolete facade code.

## Testing Strategy

| Layer | What to Test | Approach |
|---|---|---|
| Compile | Module visibility/cycles | `cargo check`. |
| Unit | Existing webhook normalization, WS/url preview tests | `cargo test webhook_normalization_tests`, `cargo test -p api-abdo whatsapp::ws`, targeted names as available. |
| Integration/parity | Routes/OpenAPI | Compare route declarations and generated OpenAPI before/after each slice; smoke key endpoints manually if env is available. |

## Migration / Rollout

No data migration required. Roll out by chained PR; each slice is code-only and independently revertible. Rollback is reverting the latest PR because schemas, DB data, env vars, Redis keys, and deployment topology do not change.

## Open Questions

None blocking.
