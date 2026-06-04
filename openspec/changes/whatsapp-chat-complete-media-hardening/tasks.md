# Tasks — whatsapp-chat-complete-media-hardening

## Phase 1 — Audit and contract

- [x] 1.1 Audit current inbound type inference against Meta known types and existing `WaMessage` shape.
- [x] 1.2 Audit current media failure fallback and determine whether it only sends customer notice or also surfaces agent-visible placeholder.
- [x] 1.3 Audit OpenAPI registration for media routes, send payloads, and schemas.

## Phase 2 — Implementation

- [x] 2.1 Add/adjust helpers so unknown inbound message types are persisted with raw payload and readable preview.
- [x] 2.2 Harden media failure fallback so failed inbound media without DB message becomes visible and traceable.
- [x] 2.3 Improve media download/upload validation edge cases without changing existing successful contracts.
- [x] 2.4 Ensure WebSocket broadcasts still use existing event names and payloads.

## Phase 3 — Documentation

- [x] 3.1 Update OpenAPI schemas/paths if any contract changes.
- [x] 3.2 Update project docs/AGENTS with supported inbound/outbound WhatsApp message types and media-failure behavior.

## Phase 4 — Verification

- [x] 4.1 Run `cargo fmt` if code changed.
- [x] 4.2 Run `cargo check`.
- [x] 4.3 Review diff to confirm no unrelated changes.
- [x] 4.4 Commit and push according to repository delivery rule.
