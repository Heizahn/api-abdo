# Proposal: Modularize WhatsApp Module

## Intent

Reduce maintenance and deployment risk in the WhatsApp area by splitting the oversized handler/module into cohesive internal modules while preserving runtime behavior. `src/modules/whatsapp/handler.rs` currently mixes webhook processing, conversation APIs, sending/media/template/settings/quick-reply flows, response mapping, auth/workspace helpers, and OpenAPI annotations. This makes small WhatsApp bug fixes risky and slows review.

## Scope

### In Scope
- Modularize strongly inside the existing API binary using route/domain boundaries.
- Preserve all frontend contracts: routes, payload shapes, auth behavior, HTTP statuses/error semantics, OpenAPI meaning, webhook handling, WS events, DB traits, deployment, and configuration.
- Move code into cohesive files under `src/modules/whatsapp/` without changing product behavior.
- Use chained PR delivery because `delivery_strategy=force-chained` and the module is large.

### Out of Scope
- Creating a separate microservice or separate `waba` binary now.
- Changing MongoDB schemas, Redis keys, Meta Cloud API behavior, assignment policy, ticket/audit semantics, or WS event schema.
- Adding new WhatsApp product features.

## Capabilities

### New Capabilities
- None — behavior-preserving refactor only.

### Modified Capabilities
- None — no spec-level behavior changes intended.

## Approach

Refactor in reviewable slices: first expose stable internal helpers/types, then extract route-domain groups such as webhook, conversations, messaging/media, templates, settings, quick replies, tickets/audit integration, and response mapping. Keep `mod.rs` route composition and `axum_router.rs` external mounting unchanged. Use Rust module visibility deliberately so extracted modules do not depend on handler internals. Treat compile/test/OpenAPI parity as the contract.

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `src/modules/whatsapp/handler.rs` | Modified | Split responsibilities into cohesive modules. |
| `src/modules/whatsapp/*.rs` | New/Modified | Add internal route/service/helper modules. |
| `src/modules/whatsapp/mod.rs` | Modified | Preserve route groups while wiring extracted modules. |
| `src/openapi.rs` | Modified | Keep documented paths/schemas semantically equivalent. |

## Chained PR Plan

- PR 1: helper/type visibility and no-op structure prep.
- PR 2: webhook/conversation extraction.
- PR 3: messaging/media/template/settings extraction.
- PR 4: quick replies, tickets/audit integration, cleanup, parity verification.

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Contract drift during refactor | Medium | Compile checks, focused regression tests, OpenAPI diff/parity review. |
| Polluted chained diffs | Medium | One domain slice per PR; retarget/rebase until each diff is clean. |
| Hidden helper coupling | Medium | Extract shared helpers first and keep module boundaries explicit. |

## Rollback Plan

Revert the affected chained PR slice. Because behavior, schema, deployment, and DB contracts remain unchanged, rollback is code-only with no migration.

## Dependencies

- Existing exploration artifact `sdd/modularize-whatsapp/explore`.
- Current Axum router/auth/OpenAPI patterns.

## Success Criteria

- [ ] WhatsApp routes, webhook, WS events, frontend payloads, auth, DB traits, deployment, and OpenAPI semantics remain unchanged.
- [ ] `handler.rs` is substantially smaller and organized by cohesive responsibilities.
- [ ] Each chained PR is reviewable within the 800-line review budget or clearly justified.
- [ ] The structure prepares, but does not require, a future separate `waba` binary.
