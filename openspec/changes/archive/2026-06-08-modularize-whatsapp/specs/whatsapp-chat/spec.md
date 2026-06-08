# Delta for WhatsApp Chat

## ADDED Requirements

### Requirement: External Route Contract Preservation

The system MUST preserve every existing WhatsApp HTTP route, method, path, status semantics, request shape, and response shape while internal modules are reorganized. Frontend clients SHALL NOT require code changes for this refactor.

#### Scenario: Existing frontend request remains valid
- GIVEN a frontend request that succeeds before modularization
- WHEN the same request is sent after modularization
- THEN the route MUST resolve to the same behavior
- AND the response contract MUST remain semantically equivalent

#### Scenario: Unknown or invalid request behavior remains stable
- GIVEN a WhatsApp request that currently returns an error
- WHEN the same invalid request is sent after modularization
- THEN the HTTP status and `{ ok: false, error: "<code>" }` semantics MUST be preserved

### Requirement: Auth and Workspace Permission Preservation

The system MUST preserve staff JWT validation, role/workspace filtering, account visibility, ownership rules, and permission failures for all protected WhatsApp routes.

#### Scenario: Authorized staff access
- GIVEN a staff token with access to a WhatsApp workspace
- WHEN the user calls a protected WhatsApp endpoint
- THEN the same data visibility and mutation permission MUST apply

#### Scenario: Unauthorized or cross-workspace access
- GIVEN a missing, invalid, blocked, or out-of-scope token
- WHEN the user calls a protected WhatsApp endpoint
- THEN the request MUST fail exactly as before modularization

### Requirement: Public Webhook Contract Preservation

The system MUST keep the Meta webhook public, outside JWT protection, and preserve signature validation, business-number lookup, and Meta-compatible HTTP 200 behavior for accepted webhook delivery semantics.

#### Scenario: Meta webhook delivery
- GIVEN Meta sends a valid webhook payload and signature
- WHEN the webhook route receives it
- THEN processing side effects MUST match current behavior
- AND Meta-facing response behavior MUST remain compatible

#### Scenario: Webhook processing edge case
- GIVEN Meta sends unsupported message types or media failure callbacks
- WHEN the webhook handles the payload
- THEN message persistence, placeholders, reactions, and retry semantics MUST remain unchanged

### Requirement: WebSocket Event Shape Preservation

The system MUST preserve WebSocket connection authentication and all JSON event discriminants, field names, payload shapes, and delivery semantics.

#### Scenario: Agent receives chat event
- GIVEN an authenticated agent WebSocket is subscribed
- WHEN a WhatsApp conversation event occurs
- THEN the emitted `tipo` and JSON payload MUST match the existing contract

#### Scenario: Invalid WebSocket input
- GIVEN a malformed or unauthorized WebSocket action
- WHEN it is processed after modularization
- THEN the same error event semantics MUST be emitted

### Requirement: OpenAPI Semantic Preservation

OpenAPI registrations MAY move to new module paths, but documented WhatsApp paths, operations, schemas, security declarations, and tags MUST remain semantically equivalent.

#### Scenario: Documentation parity
- GIVEN the OpenAPI document before modularization
- WHEN the refactored registrations are generated
- THEN WhatsApp API meaning MUST remain equivalent

### Requirement: Internal Module Boundary Only

The system MUST modularize inside the existing API binary only. It MUST NOT introduce a separate WhatsApp binary, service boundary, deployment unit, DB schema, Redis key contract, or configuration requirement in this change.

#### Scenario: Deployment unchanged
- GIVEN the existing API deployment process
- WHEN the modularized code is built and run
- THEN no new process, binary, migration, or environment variable MUST be required

### Requirement: Verification Gate

The change MUST be verified with compile checks, focused regression coverage, route/OpenAPI parity review, and chained PR slice review sized for the configured 800-line review budget.

#### Scenario: Refactor accepted
- GIVEN a chained PR slice is ready
- WHEN verification runs
- THEN compile checks MUST pass and contract drift MUST be reviewed before merge

#### Scenario: Contract drift detected
- GIVEN verification finds route, auth, webhook, WS, or OpenAPI drift
- WHEN the slice is reviewed
- THEN the drift MUST be fixed or explicitly rejected as out of scope

## MODIFIED Requirements

None — this is a behavior-preserving internal refactor.

## REMOVED Requirements

None.

## RENAMED Requirements

None.
