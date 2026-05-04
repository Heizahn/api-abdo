# Delta for AI Agent

## ADDED Requirements

### Requirement: report_payment Tool

The system MUST expose a tool named `report_payment` in the AI Agent tool registry.
The tool MUST register a client payment report end-to-end: download proof image, resolve
amounts via BCV rate + client IVA, and persist a `PaymentReport` document with audit trail.

Input contract (all fields passed as JSON args):

| Field | Type | Required | Notes |
|---|---|---|---|
| `client_id` | string | MUST | MongoDB ObjectId (hex) |
| `reference` | string | MUST | Payment reference number |
| `media_id` | string | MUST | Meta media reference; tool downloads it |
| `amount_bs` | number | XOR | Exclusive with `amount_usd` |
| `amount_usd` | number | XOR | Exclusive with `amount_bs` |
| `bank` | string | MAY | Accepted as-is, no whitelist validation |
| `phone` | string | MAY | Sender phone |
| `debt_id` | string | MAY | ObjectId of debt to associate |
| `payment_date` | string | MAY | ISO date string |

Success response shape (live mode):

```json
{
  "ok": true,
  "mode": "live",
  "payment_id": "<hex ObjectId>",
  "already_registered": false,
  "amount_bs": <number>,
  "amount_usd": <number>,
  "exchange_rate": <number>,
  "iva_rate": <number>
}
```

Error codes (returned as `ToolResult::err`):

| Code | Trigger |
|---|---|
| `invalid_args:<msg>` | Malformed args (wrong type, missing required field outside specialized codes) |
| `image_required` | `media_id` missing, empty, or whitespace-only |
| `amount_required` | Neither `amount_bs` nor `amount_usd` provided |
| `amount_conflict` | Both `amount_bs` and `amount_usd` provided |
| `invalid_amount` | Amount ≤ 0 or NaN |
| `client_not_found` | `client_id` not found in `Clients` |
| `image_download_failed` | Meta media download fails (404, network, relay error) |
| `payment_method_not_configured` | Client's owner has no `idPaymentMethod` in `Users` |
| `exchange_rate_unavailable` | Redis miss AND DB miss for BCV rate |
| `exchange_rate_zero` | Resolved rate equals 0.0 |
| `db_error` | Unexpected DB failure during insert |

#### Scenario: Successful registration

- GIVEN valid `client_id`, `reference`, downloadable `media_id`, exactly one of `amount_bs` / `amount_usd`, `ctx.is_sandbox = false`
- WHEN the tool executes
- THEN it MUST download the image from Meta, save it to `uploads/` local storage, compute the missing amount using BCV rate and client IVA, insert a `PaymentReport` doc with `state="Pendiente"`, `id_creator=ctx.ai_user_id`, `image_url=<local_path>`
- AND return `{ ok: true, mode: "live", payment_id: <new_id>, already_registered: false, amount_bs, amount_usd, exchange_rate, iva_rate }`

#### Scenario: Idempotent re-call — same (client_id, reference) pair

- GIVEN a `PaymentReport` already exists for `(client_id, reference)`
- WHEN the tool is called again with the same pair
- THEN it MUST NOT create a duplicate document
- AND it MUST return `{ ok: true, mode: "live", payment_id: <existing_id>, already_registered: true }`
- AND the existing record's `image_url` MUST NOT be overwritten

#### Scenario: amount_bs only — derive amount_usd

- GIVEN `amount_bs = N` provided, `amount_usd` not provided
- WHEN the tool executes
- THEN it MUST compute `amount_usd = round2((amount_bs / iva_rate) / exchange_rate)` and persist both

#### Scenario: amount_usd only — derive amount_bs

- GIVEN `amount_usd = N` provided, `amount_bs` not provided
- WHEN the tool executes
- THEN it MUST compute `amount_bs = round2(amount_usd * exchange_rate * iva_rate)` and persist both

#### Scenario: amount_required — neither amount provided

- GIVEN neither `amount_bs` nor `amount_usd` provided
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `amount_required` BEFORE any DB or download work

#### Scenario: amount_conflict — both amounts provided

- GIVEN both `amount_bs` and `amount_usd` provided
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `amount_conflict`

#### Scenario: invalid_amount — non-positive value

- GIVEN any provided amount is ≤ 0 or NaN
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `invalid_amount`

#### Scenario: image_required — missing or blank media_id

- GIVEN `media_id` is absent, empty string, or whitespace-only
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `image_required` BEFORE any DB or download work

#### Scenario: image_download_failed — Meta download error

- GIVEN the Meta media download fails (404, network error, relay error)
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `image_download_failed`
- AND no `PaymentReport` SHALL be created

#### Scenario: client_not_found

- GIVEN `client_id` does not match any document in `Clients`
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `client_not_found`

#### Scenario: payment_method_not_configured

- GIVEN the client's owner has no `idPaymentMethod` set in `Users`
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `payment_method_not_configured`

#### Scenario: exchange_rate_unavailable

- GIVEN Redis returns no BCV rate AND the DB fallback also returns no result
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `exchange_rate_unavailable`

#### Scenario: exchange_rate_zero

- GIVEN the resolved BCV rate equals 0.0
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `exchange_rate_zero`

#### Scenario: iva_rate default when tax absent

- GIVEN the client's `idTax` is None OR `find_tax_by_id` returns None
- WHEN the tool executes
- THEN `iva_rate` MUST default to `1.0` (no IVA applied), matching the existing endpoint behavior

#### Scenario: id_creator persistence

- GIVEN a successful registration in live mode
- WHEN the `PaymentReport` document is persisted
- THEN the `idCreator` field MUST equal `ctx.ai_user_id` (the AI synthetic user UUID)

#### Scenario: Sandbox mode — no side effects

- GIVEN `ctx.is_sandbox = true`
- WHEN the tool executes with otherwise valid args
- THEN it MUST NOT touch DB and MUST NOT download the image
- AND it MUST return `{ ok: true, mode: "sandbox", payment_id: "<sandbox-fake>", already_registered: false }`

#### Scenario: invalid_args — malformed input

- GIVEN args are malformed (wrong type, missing required field outside the specialized error codes above)
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `invalid_args:<descriptive_message>`

---

### Requirement: PaymentReport id_creator Field

The `PaymentReport` struct MUST include an `id_creator: Option<String>` field
(`idCreator` in MongoDB). The field MUST use `#[serde(default)]` so that existing
documents without `idCreator` deserialize without error.

#### Scenario: Backwards compatibility — existing docs without idCreator

- GIVEN a `PaymentReport` document in MongoDB that predates this change (no `idCreator` field)
- WHEN the document is deserialized
- THEN `id_creator` MUST deserialize as `None` without error
- AND no data migration SHALL be required

---

## MODIFIED Requirements

### Requirement: Tool Categorization

The system MUST distinguish between `InfoLookup` and `Action` tool categories
when evaluating counter behavior at the end of each dispatch turn.

Categorization table (exhaustive over currently known tools):

| Tool name                | Category    |
|--------------------------|-------------|
| `lookup_customer`        | InfoLookup  |
| `list_plans`             | InfoLookup  |
| `check_coverage`         | InfoLookup  |
| `get_invoices`           | InfoLookup  |
| `calculate_amount_bs`    | InfoLookup  |
| `create_ticket`          | Action      |
| `request_human`          | Action      |
| `transfer_to_agent`      | Action      |
| `report_payment`         | Action      |

(Previously: table did not include `report_payment`)

Unknown tool names MUST default to `InfoLookup` (safe default).

A `tracing::warn!` SHOULD be emitted for any tool name not present in the
categorization table, to signal that an explicit categorization is missing.

### Scenario 1.1: InfoLookup tool with success — skip, no reset

**Given** a dispatch turn for conversation `C` where `no_resolution_count = N` (N ≥ 0),
  `qualification_window_turns = 0`, and the qualification window does NOT apply
**When** the turn contains at least one tool call of category `InfoLookup`
  with `success = true`, and no tool call of category `Action` with `success = true`
**Then** the counter SHALL remain at `N` (no increment, no reset)
**And** a `tracing::debug!` line SHALL be emitted with event `no_resolution_skipped`,
  tool name, category `InfoLookup`, and current `count=N/MAX`

### Scenario 1.2: Action tool with success — reset to zero

**Given** a dispatch turn for conversation `C` where `no_resolution_count = N` (N ≥ 0)
  and the qualification window does NOT apply
**When** the turn contains at least one tool call of category `Action`
  with `success = true` (regardless of any InfoLookup tools also present)
**Then** the counter SHALL be reset to 0 via `reset_ai_no_resolution(conv_id)`
**And** a `tracing::debug!` line SHALL be emitted with event `no_resolution_reset`,
  the Action tool name, and `count=0/MAX`
**And** no escalation SHALL be triggered on this turn

### Scenario 1.3: No tool call / all tools failed — increment

**Given** a dispatch turn for conversation `C` where `no_resolution_count = N`,
  `max_turns_without_resolution > 0`, and the qualification window does NOT apply
**When** the turn contains no tool calls, OR all tool calls have `success = false`
**Then** the counter SHALL increment by 1 (via `incr_ai_no_resolution`)
**And** a `tracing::info!` line SHALL be emitted with `count=(N+1)/MAX`

### Scenario 1.4: Failed InfoLookup only — increment

**Given** a dispatch turn for conversation `C` where `no_resolution_count = N`
  and the qualification window does NOT apply
**When** the turn contains one or more tool calls all of category `InfoLookup`
  with `success = false`
**Then** the counter SHALL increment by 1 (treated as no useful tool call)
**And** a `tracing::info!` line SHALL be emitted with `count=(N+1)/MAX`

### Scenario 1.5: Mix of InfoLookup success and Action success in one turn — Action wins (reset)

**Given** a dispatch turn where at least one `InfoLookup` tool succeeds
  AND at least one `Action` tool succeeds
**When** the turn is evaluated
**Then** the Action path SHALL take precedence and the counter SHALL be reset to 0
**And** the InfoLookup skip path SHALL NOT execute independently in the same turn

### Scenario 1.6: Multiple Action tools in one turn — single idempotent reset

**Given** a dispatch turn containing two or more successful `Action` tool calls
**When** the turn is evaluated
**Then** the counter SHALL be reset to 0 exactly once (idempotent)
**And** `reset_ai_no_resolution` SHALL be called only once per turn evaluation

### Scenario 1.7: Unknown tool name defaults to InfoLookup

**Given** a dispatch turn where the tool name does not appear in the categorization table
**When** the tool call has `success = true`
**Then** the system MUST treat it as `InfoLookup` (skip increment, no reset)
**And** a `tracing::warn!` SHOULD be emitted indicating the tool name is uncategorized

### Scenario: report_payment as Action — counter reset

**Given** a dispatch turn where `report_payment` executes with `success = true`
  and `no_resolution_count = N` (N ≥ 0)
**When** `tool_category("report_payment")` is evaluated
**Then** it MUST return `Action`
**And** the counter SHALL be reset to 0 per Scenario 1.2
