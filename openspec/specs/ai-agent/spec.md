# Delta Spec: AI Agent — no_resolution Counter

## Capability: ai-agent / no_resolution counter

This delta spec defines the REQUIRED behavior for the `no_resolution` counter
after the `ai-agent-no-resolution-counter` change is applied. It is a delta over
the existing behavior described in the exploration artifact.

---

## Requirement 1: Tool Categorization

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

(Previously: table did not include `calculate_amount_bs`; now updated to include `report_payment`)

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

### Scenario 1.8: report_payment as Action — counter reset

**Given** a dispatch turn where `report_payment` executes with `success = true`
  and `no_resolution_count = N` (N ≥ 0)
**When** `tool_category("report_payment")` is evaluated
**Then** it MUST return `Action`
**And** the counter SHALL be reset to 0 per Scenario 1.2

---

## Requirement 1.5: calculate_amount_bs Tool

The system MUST expose a tool named `calculate_amount_bs` in the AI Agent tool
registry. The tool MUST be categorized as `InfoLookup` and MUST convert a USD
amount to Bs using the current BCV exchange rate and the EMPRESARIAL IVA
configuration. The tool MUST NOT fall back to any other `sTarget` value if
`EMPRESARIAL` is absent.

### Scenario 1.5.1: Successful conversion

- GIVEN `amount_usd > 0.0`, the BCV rate is available (Redis or DB), and a
  `BCV.IVA` document with `sTarget = "EMPRESARIAL"` exists
- WHEN the tool executes
- THEN it MUST return a JSON object with exactly these 7 fields, each rounded
  to 2 decimals where applicable:

  ```json
  {
    "amount_usd":        <number — input value, unchanged>,
    "bcv_rate":          <number — resolved rate, round2>,
    "rate_date":         "<YYYY-MM-DD in Venezuela timezone>",
    "iva_factor":        <number — e.g. 1.16>,
    "iva_percent":       <number — e.g. 16.0, round2((iva_factor-1)*100)>,
    "amount_bs_base":    <number — round2(amount_usd * bcv_rate)>,
    "amount_bs_with_iva":<number — round2(amount_usd * bcv_rate * iva_factor)>
  }
  ```

- AND `amount_bs_base` and `amount_bs_with_iva` MUST each be derived
  independently from the original `amount_usd` (NOT chained from each other)
- AND the rounding formula MUST be `(x * 100.0).round() / 100.0` for every
  rounded field

### Scenario 1.5.2: Invalid amount — zero or negative

- GIVEN `amount_usd <= 0.0`
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code `invalid_amount`

### Scenario 1.5.3: Missing or malformed arguments

- GIVEN the tool is called without the `amount_usd` field (or with a
  non-numeric value that cannot be parsed as `f64`)
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code `invalid_args`

### Scenario 1.5.4: BCV rate unavailable

- GIVEN Redis returns no rate (miss or error) AND the DB query for the latest
  exchange rate also returns no result
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code
  `exchange_rate_unavailable`

### Scenario 1.5.5: BCV rate is zero

- GIVEN the resolved exchange rate (from Redis or DB) is exactly `0.0`
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code `exchange_rate_zero`

### Scenario 1.5.6: EMPRESARIAL tax doc missing

- GIVEN no document with `sTarget = "EMPRESARIAL"` exists in the `BCV.IVA`
  collection
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code `tax_config_missing`
- AND the tool MUST NOT fall back to a document with `sTarget = "DEFAULT"`
  or any other value

### Scenario 1.5.7: Redis rate preferred over DB

- GIVEN Redis returns a valid non-zero exchange rate for the current day
- WHEN the tool executes
- THEN it MUST use the Redis-sourced rate without issuing a DB query for the
  exchange rate

### Scenario 1.5.8: Tool category is InfoLookup

- GIVEN the tool registry is queried for `calculate_amount_bs` category
- WHEN `tool_category("calculate_amount_bs")` is evaluated
- THEN it MUST return `InfoLookup`
- AND per Requirement 1 of the `no_resolution` counter spec, successful
  execution of this tool MUST NOT reset `no_resolution_count` and MUST NOT
  increment it

### Scenario 1.5.9: Sandbox parity

- GIVEN a conversation where `is_sandbox = true`
- WHEN the tool executes with valid inputs
- THEN it MUST behave identically to `is_sandbox = false`
- AND no special sandbox branch SHALL exist for this tool (it is read-only)

---

## Requirement 1.6: report_payment Tool

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
| `invalid_args:<msg>` | Malformed args (wrong type, can't deserialize to `ReportPaymentArgs`) |
| `invalid_client_id` | `client_id` cannot be parsed as ObjectId hex (consistent with `exec_get_invoices`) |
| `invalid_debt_id` | `debt_id` provided but cannot be parsed as ObjectId hex |
| `image_required` | `media_id` missing, empty, or whitespace-only |
| `image_empty` | Meta download succeeded but body is 0 bytes |
| `image_download_failed:<msg>` | Meta media download fails (404, network, relay error) |
| `image_save_failed:<msg>` | Filesystem write to `uploads/` fails |
| `reference_required` | `reference` is empty or whitespace-only |
| `amount_required` | Neither `amount_bs` nor `amount_usd` provided |
| `amount_conflict` | Both `amount_bs` and `amount_usd` provided |
| `invalid_amount` | Amount ≤ 0 or NaN |
| `client_not_found` | `client_id` parses but no matching document in `Clients` |
| `payment_method_not_configured` | Client's owner has no `idPaymentMethod` in `Users` |
| `exchange_rate_unavailable` | Redis miss AND DB error for BCV rate |
| `exchange_rate_zero` | Resolved rate is `<= 0.0` (defensive: covers exactly-zero and negative outliers) |
| `wa_settings_not_found` | `WaSettings` doc for the workspace can't be found (operational) |
| `wa_token_decrypt_failed` | The encrypted Meta access token can't be decrypted with `JWT_SECRET` |
| `db_error:<msg>` | Unexpected DB failure during insert or auxiliary lookup |

### Scenario 1.6.1: Successful registration

- GIVEN valid `client_id`, `reference`, downloadable `media_id`, exactly one of `amount_bs` / `amount_usd`, `ctx.is_sandbox = false`
- WHEN the tool executes
- THEN it MUST download the image from Meta, save it to `uploads/` local storage, compute the missing amount using BCV rate and client IVA, insert a `PaymentReport` doc with `state="Pendiente"`, `id_creator=ctx.ai_user_id`, `image_url=<local_path>`
- AND return `{ ok: true, mode: "live", payment_id: <new_id>, already_registered: false, amount_bs, amount_usd, exchange_rate, iva_rate, is_advance: <bool> }`
- AND `is_advance` MUST be `true` if no `debt_id` was provided (the report is on-account credit), `false` otherwise

### Scenario 1.6.2: Idempotent re-call — same `(client_id, reference)` pair

- GIVEN a `PaymentReport` (or `Payments`) document already exists for `(client_id, reference)` resolvable via `check_reference`
- WHEN the tool is called again with the same pair
- THEN it MUST NOT create a duplicate document AND MUST NOT download the image
- AND it MUST return a richer payload that lets the caller distinguish prior state without an extra DB query:

  ```json
  {
    "ok": true,
    "mode": "live",
    "already_registered": true,
    "source": "<PaymentReports | Payments>",
    "is_same_client": <bool>,
    "matched_reference": "<string>",
    "matched_state": "<Pendiente | Aprobado | Rechazado | ...>",
    "matched_amount_bs": <number | null>,
    "matched_amount_usd": <number | null>
  }
  ```

- AND `is_same_client` MUST be `true` if the matched document's `idClient` equals the requested `client_id`, `false` otherwise (catches mis-attribution: the reference is in use by a different client)
- AND `payment_id` is intentionally OMITTED from this shape: `check_reference` returns match info, not the matched `_id`. Callers that need the existing `_id` MUST do a follow-up query — out of scope for the AI tool, which only needs to know the prior reference is occupied
- AND the existing record's `image_url` MUST NOT be overwritten

### Scenario 1.6.3: amount_bs only — derive amount_usd

- GIVEN `amount_bs = N` provided, `amount_usd` not provided
- WHEN the tool executes
- THEN it MUST compute `amount_usd = round2((amount_bs / iva_rate) / exchange_rate)` and persist both

### Scenario 1.6.4: amount_usd only — derive amount_bs

- GIVEN `amount_usd = N` provided, `amount_bs` not provided
- WHEN the tool executes
- THEN it MUST compute `amount_bs = round2(amount_usd * exchange_rate * iva_rate)` and persist both

### Scenario 1.6.5: amount_required — neither amount provided

- GIVEN neither `amount_bs` nor `amount_usd` provided
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `amount_required` BEFORE any DB or download work

### Scenario 1.6.6: amount_conflict — both amounts provided

- GIVEN both `amount_bs` and `amount_usd` provided
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `amount_conflict`

### Scenario 1.6.7: invalid_amount — non-positive value

- GIVEN any provided amount is ≤ 0 or NaN
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `invalid_amount`

### Scenario 1.6.8: image_required — missing or blank media_id

- GIVEN `media_id` is absent, empty string, or whitespace-only
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `image_required` BEFORE any DB or download work

### Scenario 1.6.9: image_download_failed — Meta download error

- GIVEN the Meta media download fails (404, network error, relay error)
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `image_download_failed`
- AND no `PaymentReport` SHALL be created

### Scenario 1.6.10: client_not_found

- GIVEN `client_id` does not match any document in `Clients`
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `client_not_found`

### Scenario 1.6.11: payment_method_not_configured

- GIVEN the client's owner has no `idPaymentMethod` set in `Users`
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `payment_method_not_configured`

### Scenario 1.6.12: exchange_rate_unavailable

- GIVEN Redis returns no BCV rate AND the DB fallback also returns no result
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `exchange_rate_unavailable`

### Scenario 1.6.13: exchange_rate_zero

- GIVEN the resolved BCV rate is `<= 0.0` (zero or negative)
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `exchange_rate_zero`
- AND the implementation MAY use `rate <= 0.0` instead of `rate == 0.0` for defensive coverage of negative outliers

### Scenario 1.6.14: iva_rate default when tax absent

- GIVEN the client's `idTax` is None OR `find_tax_by_id` returns None
- WHEN the tool executes
- THEN `iva_rate` MUST default to `1.0` (no IVA applied), matching the existing endpoint behavior

### Scenario 1.6.15: id_creator persistence

- GIVEN a successful registration in live mode
- WHEN the `PaymentReport` document is persisted
- THEN the `idCreator` field MUST equal `ctx.ai_user_id` (the AI synthetic user UUID)

### Scenario 1.6.16: Sandbox mode — no side effects

- GIVEN `ctx.is_sandbox = true`
- WHEN the tool executes with otherwise valid args (validations still fire BEFORE the sandbox short-circuit)
- THEN it MUST NOT touch DB and MUST NOT download the image
- AND it MUST return `{ ok: true, mode: "sandbox", payment_id: "sandbox-fake-payment", already_registered: false, amount_bs: <input or null>, amount_usd: <input or null>, exchange_rate: 0.0, iva_rate: 1.0 }`
- AND the sandbox response MAY echo the raw input amounts (no BCV/IVA computation) — `exchange_rate` and `iva_rate` carry placeholder values to keep the response shape compatible with the live success contract

### Scenario 1.6.17: invalid_args — malformed input

- GIVEN args are malformed (wrong type, missing required field outside the specialized error codes above)
- WHEN the tool executes
- THEN it MUST return `ToolResult::err` with code `invalid_args:<descriptive_message>`

---

## Requirement 1.7: PaymentReport id_creator Field

The `PaymentReport` struct MUST include an `id_creator: Option<String>` field
(`idCreator` in MongoDB). The field MUST use `#[serde(default)]` so that existing
documents without `idCreator` deserialize without error.

### Scenario 1.7.1: Backwards compatibility — existing docs without idCreator

- GIVEN a `PaymentReport` document in MongoDB that predates this change (no `idCreator` field)
- WHEN the document is deserialized
- THEN `id_creator` MUST deserialize as `None` without error
- AND no data migration SHALL be required

---

## Requirement 2: Qualification Window

The system MUST support a per-agent `qualification_window_turns` field (type `u32`,
default `0`) on `AiEscalationRules`. When the current conversation's `prior_ai_turns`
is strictly less than `qualification_window_turns`, the counter logic MUST be fully
bypassed for that turn (no increment, no reset, no escalation evaluation).

### Scenario 2.1: Turn within qualification window — fully skipped

**Given** an agent with `qualification_window_turns = W` (W > 0)
  and a conversation where `prior_ai_turns = T` with `T < W`
**When** a dispatch turn completes (regardless of tool calls or their results)
**Then** the `no_resolution_count` SHALL remain unchanged
**And** no increment, reset, or escalation SHALL occur
**And** a `tracing::debug!` line SHALL be emitted with event `no_resolution_window_skip`,
  `prior_ai_turns=T`, and `window=W`

### Scenario 2.2: Turn at the qualification window boundary — normal evaluation applies

**Given** an agent with `qualification_window_turns = W`
  and a conversation where `prior_ai_turns = W` (exactly equal)
**When** a dispatch turn completes
**Then** the qualification window bypass SHALL NOT apply
**And** the standard counter logic (Requirement 1) SHALL execute normally

### Scenario 2.3: Agent with qualification_window_turns = 0 — no bypass

**Given** an agent with `qualification_window_turns = 0` (default)
**When** any dispatch turn completes
**Then** the qualification window bypass SHALL NOT apply for any turn
**And** behavior is identical to the pre-change implementation

### Scenario 2.4: Qualification window with no tool calls — Carla regression case

**Given** an agent with `qualification_window_turns = 4`, `max_turns_without_resolution = 4`
  and a conversation with 4 consecutive text-only turns (zero tool calls)
**When** turns 1 through 4 are evaluated (`prior_ai_turns` = 0, 1, 2, 3)
**Then** the counter SHALL remain at 0 after each of the 4 turns (bypassed by window)
**And** no escalation SHALL be triggered during turns 1–4
**And** on turn 5 (`prior_ai_turns = 4 >= window`), the counter SHALL increment to 1
  if the turn has no tool success, and escalation SHALL NOT fire (1 < 4)

### Scenario 2.5: Qualification window is independent of fresh-start detection

**Given** a conversation where fresh-start detection fires on turn 1
  (i.e., `prior_ai_turns == 0` and `prior_history_count > 0`)
  and the agent has `qualification_window_turns = W` (W > 0)
**When** fresh-start clears all counters AND turn 1 is evaluated
**Then** both mechanisms MAY apply to turn 1 independently without conflict:
  fresh-start clears counters (including `no_resolution`), then the window
  bypass also prevents any increment/reset for that turn
**And** subsequent turns within the window (turns 2..W) SHALL also be bypassed

---

## Requirement 3: Counter Increment and Escalation (Preserved Behavior)

These requirements document preserved behavior that MUST NOT regress.

### Scenario 3.1: Counter reaching MAX triggers auto_escalate

**Given** an agent with `max_turns_without_resolution = M` (M > 0),
  `qualification_window_turns = 0`
  and a conversation where `no_resolution_count = M - 1`
**When** a turn completes with no tool success (increment applies)
**Then** the counter SHALL increment to M
**And** `auto_escalate` SHALL be triggered
**And** the escalation side-effects SHALL execute: `ai_disabled=true`,
  assignment cleared, `ai_handoff` event recorded, counters cleared,
  optional `farewell_to_human` message sent, WS event `IaPausada` broadcast

### Scenario 3.2: max_turns_without_resolution = 0 disables the counter

**Given** an agent with `max_turns_without_resolution = 0`
**When** any dispatch turn completes
**Then** no increment, no reset, and no escalation SHALL occur for the `no_resolution` counter

### Scenario 3.3: Escalation path for loops — sanity regression

**Given** an agent with `max_turns_without_resolution = 3`, `qualification_window_turns = 0`
  and 3 consecutive turns with no tool calls
**When** turn 3 is evaluated
**Then** the counter SHALL reach 3 and `auto_escalate` SHALL fire
**And** this behavior SHALL be unchanged from pre-fix behavior

---

## Requirement 4: Targeted Redis Reset Method

A new Redis method MUST exist that resets ONLY the `no_resolution` counter for a
given conversation, without affecting any other per-conversation Redis keys.

### Scenario 4.1: reset_ai_no_resolution deletes only its own key

**Given** a Redis store where conversation `C` has keys:
  `ai_agent:no_resolution:{C}`, `ai_agent:turns_conv:{C}`, `ai_agent:id_attempts:{C}`
**When** `reset_ai_no_resolution(C)` is called
**Then** the key `ai_agent:no_resolution:{C}` SHALL be deleted (DEL operation)
**And** the keys `ai_agent:turns_conv:{C}` and `ai_agent:id_attempts:{C}`
  SHALL remain unmodified

### Scenario 4.2: reset_ai_no_resolution is idempotent

**Given** that `ai_agent:no_resolution:{C}` does not exist in Redis
  (either never set or already deleted)
**When** `reset_ai_no_resolution(C)` is called
**Then** no error SHALL be raised
**And** Redis SHALL return 0 (key not found — `DEL` on missing key is a no-op)

### Scenario 4.3: clear_ai_conv_counters is not modified

**Given** the existing `clear_ai_conv_counters` method
**When** called
**Then** it SHALL continue to delete all three keys:
  `ai_agent:no_resolution:{C}`, `ai_agent:turns_conv:{C}`, `ai_agent:id_attempts:{C}`
**And** its existing callers (escalation, fresh-start, tools.rs transfer_to_agent)
  SHALL continue to function without modification

---

## Requirement 5: transfer_to_agent Double-Reset (Idempotent)

The `transfer_to_agent` tool MUST continue its existing behavior while also
being covered by the new Action-tool reset path in the dispatch loop.

### Scenario 5.1: transfer_to_agent triggers both reset paths — idempotent

**Given** a dispatch turn where `transfer_to_agent` executes with `success = true`
**When** the turn is evaluated
**Then** `clear_ai_conv_counters` SHALL execute inside the tool (existing behavior)
  which deletes all three per-conversation Redis keys
**And** the dispatch Action-tool path SHALL also call `reset_ai_no_resolution`
**And** the second call SHALL be a no-op (key already deleted by `clear`)
**And** no error or unexpected state SHALL result from the double call

### Scenario 5.2: transfer_to_agent with DB failure does not reset

**Given** a dispatch turn where the DB write inside `transfer_to_agent` fails
**When** the tool returns an error result before executing `clear_ai_conv_counters`
**Then** `clear_ai_conv_counters` SHALL NOT execute (existing behavior preserved)
**And** the dispatch Action-tool path SHALL NOT treat this as a successful Action
  (tool `success` is `false`)
**And** the counter SHALL increment normally

---

## Requirement 6: Backwards Compatibility

### Scenario 6.1: Existing AiAgent documents without qualification_window_turns

**Given** an `AiAgent` document in MongoDB that was created before this change
  and does not contain the `qualification_window_turns` field
**When** the document is deserialized
**Then** `qualification_window_turns` SHALL default to `0`
**And** the dispatch behavior SHALL be identical to the pre-change behavior
  (no qualification window bypass)

### Scenario 6.2: No MongoDB schema migration required

**Given** existing `AiAgent` documents in MongoDB
**When** the new code is deployed
**Then** no data migration script SHALL be required
**And** documents without `qualification_window_turns` SHALL continue to work
  via the `#[serde(default)]` deserialization rule

---

## Requirement 7: Range Validation on CRUD Endpoints

### Scenario 7.1: qualification_window_turns at valid lower boundary (0)

**Given** a `POST /v1/auth-user/ai-agents` or `PUT /v1/auth-user/ai-agents/:id` request
  with `escalation.qualification_window_turns = 0`
**When** the endpoint processes the request
**Then** the request SHALL be accepted (value is within valid range)

### Scenario 7.2: qualification_window_turns at valid upper boundary (10)

**Given** a `POST /v1/auth-user/ai-agents` or `PUT /v1/auth-user/ai-agents/:id` request
  with `escalation.qualification_window_turns = 10`
**When** the endpoint processes the request
**Then** the request SHALL be accepted (value is within valid range)

### Scenario 7.3: qualification_window_turns above upper boundary (11+) — rejected

**Given** a `POST /v1/auth-user/ai-agents` or `PUT /v1/auth-user/ai-agents/:id` request
  with `escalation.qualification_window_turns = V` where `V > 10`
**When** the endpoint processes the request
**Then** the request SHALL be rejected with HTTP 400 Bad Request
**And** the response body SHALL match the project's standard `ApiError` envelope:
  ```json
  {
    "ok": false,
    "error": "qualification_window_turns_out_of_range"
  }
  ```
**And** the rejected value SHALL be logged via `tracing::warn!` with the submitted
  value `V` and the conversation/agent context (for operator diagnostics)
**And** NO `message` field SHALL be added to the response envelope (the admin UI
  knows the valid range `0..=10` from the schema documentation)

### Scenario 7.4: qualification_window_turns absent — defaults to 0 (valid)

**Given** a `POST /v1/auth-user/ai-agents` request where `escalation` does not include
  the `qualification_window_turns` field
**When** the endpoint processes the request
**Then** the request SHALL be accepted
**And** the stored value SHALL be `0`

---

## Requirement 8: Logging Contract

The logging behavior MUST distinguish between the counter paths with appropriate
severity levels.

| Event | Level | Required fields |
|-------|-------|-----------------|
| Counter incremented | `info` | `conv`, `count=N/MAX`, `resolved_now=false` |
| Skip by InfoLookup tool | `debug` | `event=no_resolution_skipped`, `tool`, `category=InfoLookup`, `count=N/MAX` |
| Reset by Action tool | `debug` | `event=no_resolution_reset`, `tool`, `category=Action`, `count=0/MAX` |
| Skip by qualification window | `debug` | `event=no_resolution_window_skip`, `prior_ai_turns`, `window` |
| Unknown tool name | `warn` | tool name, fallback category `InfoLookup` |

### Scenario 8.1: Increment path preserves existing info log

**Given** a turn that results in a counter increment
**When** `incr_ai_no_resolution` executes
**Then** a `tracing::info!` line SHALL be emitted (preserving the existing log format
  with `count=N/MAX` and `resolved_now=false`)

### Scenario 8.2: Skip/reset paths emit debug logs

**Given** a turn that results in a skip (InfoLookup or window) or reset (Action)
**When** the evaluation completes
**Then** a `tracing::debug!` line SHALL be emitted with the appropriate event name
  and relevant fields as per the table above
**And** NO `tracing::info!` SHALL be emitted for skip or reset paths

---

## Resolved Gaps

The following items were resolved by the user before the spec was finalized:

1. **`tracing::warn!` for unknown tools**: SHOULD (not MUST). Default fallback to
   `InfoLookup` is safe — the warn signals to the dev that a new tool was added
   without explicit categorization.

2. **Validation placement**: Implementation detail, deferred to the design phase.
   Spec only requires that invalid values are rejected before the DB write.

3. **Error response shape**: Locked to project's standard `ApiError` envelope
   `{ ok: false, error: "<code>" }` with NO `message` field. Error code is
   `qualification_window_turns_out_of_range`. The admin UI is expected to know
   the valid range (`0..=10`) from schema documentation. A separate future change
   may extend `ApiError` with an optional `message: Option<String>` field — when
   that lands, this endpoint will adopt it automatically.

4. **`prior_ai_turns` computation**: Confirmed pre-turn count via
   `dispatch.rs:331-335`. Used directly in the qualification window comparison.

---

# Added: AI Agent — Guardrails + Turn-State HUD

## Requirement 9: check_coverage Zone-Mention Guardrail

The system MUST validate that the claimed `zone` argument was explicitly mentioned
by the customer in recent inbound messages before executing `check_coverage`.
Matching is bidirectional substring, case/diacritic-insensitive (normalized via
`normalize_zone`). When `Config.enable_ai_guardrails = false`, this guardrail MUST
be skipped entirely.

### Scenario 9.1: Zone mentioned — guardrail passes

- GIVEN a recent customer inbound message contains "Valencia" (case/diacritic-insensitive)
- WHEN `exec_check_coverage` is called with `zone="Valencia"` (or "valencia carabobo")
- THEN the guardrail MUST allow execution and the tool proceeds with the coverage lookup

### Scenario 9.2: Zone NOT mentioned — guardrail fails

- GIVEN no recent customer inbound message contains the claimed zone (normalized)
- WHEN `exec_check_coverage` is called with `zone="Naguanagua"`
- THEN the tool MUST return `ToolResult::err` with code `zone_not_mentioned_by_customer`
- AND the tool MUST NOT query coverage zones from DB

### Scenario 9.3: Bidirectional substring match

- GIVEN a customer inbound message contains "San Diego"
- WHEN `exec_check_coverage` is called with `zone="San Diego, Carabobo"`
- THEN the guardrail MUST pass (either direction of substring inclusion satisfies the check)

### Scenario 9.4: Empty customer zones — guardrail fails

- GIVEN `customer_explicit_zones` is empty (customer mentioned no place name in `recent`)
- WHEN `exec_check_coverage` is called with any `zone`
- THEN the tool MUST return `ToolResult::err` with code `zone_not_mentioned_by_customer`

### Scenario 9.5: Kill switch disables guardrail

- GIVEN `Config.enable_ai_guardrails = false`
- WHEN `exec_check_coverage` is called regardless of `customer_explicit_zones`
- THEN the guardrail MUST be skipped and the tool runs as before this change
- AND `tracing::warn!` SHOULD log "guardrails disabled via config" at startup or first tool call

---

## Requirement 10: report_payment Media-ID-in-Conversation Guardrail

The system MUST validate that the `media_id` argument was present in the
recent inbound media of the current conversation before executing
`exec_report_payment`. When `Config.enable_ai_guardrails = false`, this
guardrail MUST be skipped.

### Scenario 10.1: media_id present in recent — guardrail passes

- GIVEN `ctx.recent_media_ids` contains the claimed `media_id`
- WHEN `exec_report_payment` is called
- THEN the guardrail MUST allow execution and the tool proceeds normally

### Scenario 10.2: media_id NOT in recent — guardrail fails

- GIVEN `ctx.recent_media_ids` does NOT contain the claimed `media_id`
- WHEN `exec_report_payment` is called
- THEN the tool MUST return `ToolResult::err` with code `media_id_not_in_conversation`
- AND the tool MUST NOT download or insert anything

### Scenario 10.3: Kill switch disables guardrail

- GIVEN `Config.enable_ai_guardrails = false`
- WHEN `exec_report_payment` is called regardless of `recent_media_ids`
- THEN the guardrail MUST be skipped

---

## Requirement 11: Turn-State HUD Block

The system MUST inject a `[turn_state]` block into the system instruction when at
least one of the following is true: `turn_number > 1`, `customer_explicit_zones`
non-empty, or `customer_explicit_intents` non-empty.
The block MUST appear after `[customer_lookup_by_phone]` and before `[faqs]`.
It MUST NOT contain `already_greeted` (that lives in the existing `[agent_state]` block).

`turn_number` MUST be computed as `count(history where role == User) + 1` —
i.e., the ordinal of the user message currently being processed. In well-formed
alternating conversations this equals "model messages + 1"; the User-based count
is the canonical formula because it does not require the latest user message to
be in `history`.

### Scenario 11.1: HUD injected when meaningful

- GIVEN `turn_number = 3`, `customer_explicit_zones = ["Valencia"]`, `customer_explicit_intents = ["internet"]`
- WHEN `build_system_instruction` runs
- THEN the system instruction MUST include a `[turn_state]` block with:
  ```
  turn_number: 3
  customer_explicit_zones: [Valencia]
  customer_explicit_intents: [internet]
  ```
- AND the block MUST appear after `[customer_lookup_by_phone]` and before `[faqs]`

### Scenario 11.2: HUD omitted when all values are baseline

- GIVEN `turn_number = 1`, `customer_explicit_zones = []`, `customer_explicit_intents = []`
- WHEN `build_system_instruction` runs
- THEN the `[turn_state]` block MAY be omitted

### Scenario 11.3: HUD does not duplicate already_greeted

- GIVEN the `[agent_state]` block already contains `already_greeted`
- WHEN `build_system_instruction` runs
- THEN the `[turn_state]` block MUST NOT contain an `already_greeted` field

---

## Requirement 12: ToolContext Field Additions

`ToolContext` MUST gain two new fields: `customer_explicit_zones: Vec<String>` and
`recent_media_ids: Vec<String>`. Sandbox runs MUST initialize both as `Vec::new()`.

### Scenario 12.1: Sandbox initializes new fields as empty

- GIVEN `sandbox.rs` constructs a `ToolContext` for a sandbox run
- WHEN the new fields are added
- THEN both `customer_explicit_zones` and `recent_media_ids` MUST be initialized to `Vec::new()`
- AND `cargo check` MUST pass with no new errors or warnings

### Scenario 12.2: Existing tools are unaffected

- GIVEN tools `lookup_customer`, `calculate_amount_bs`, `get_invoices`, `list_plans`
- WHEN they execute after this change
- THEN they MUST behave identically to pre-change behavior (they do not read the new fields)

---

## Requirement 13: Customer Intent Extraction — Keyword Set v1

The system MUST recognize intents from inbound customer message text using
case/diacritic-insensitive substring matching against the following canonical keyword set.
`customer_explicit_intents` MUST list matched intent keys (not raw substrings).

All match substrings MUST be stored normalized (lowercase, no accents). The
buffer to scan is also normalized (`normalize_zone` over each inbound body),
so accents in customer text match unaccented triggers and vice versa.

| Intent key     | Match substrings (any one triggers the intent)                                                       |
|----------------|------------------------------------------------------------------------------------------------------|
| `internet`     | internet, conexion, wifi, red                                                                        |
| `contratar`    | contratar, contrato, instalar, instalacion, nuevo servicio, instalan                                 |
| `precio`       | precio, costo, cuanto, vale                                                                          |
| `cobertura`    | cobertura, llegan, llega, cubren, zona                                                               |
| `factura`      | factura, facturacion                                                                                 |
| `pago`         | pago, pagar, pague, comprobante, deposito, transferencia, transferi, abono, referencia               |
| `saldo`        | saldo, debo, deuda, mora                                                                             |
| `planes`       | plan, planes, mbps, megas, velocidad                                                                 |
| `soporte`      | soporte, no anda, no funciona, no tengo internet, sin internet, lento, se cayo, no me anda, no carga, falla, averia, problema |
| `humano`       | humano, persona, asesor, operador, hablar con alguien, agente, supervisor                            |
| `plan_change`  | cambiar de plan, subir plan, bajar plan, upgrade, downgrade                                          |
| `account`      | actualizar, cambiar datos, mi correo, mi telefono, mi direccion                                      |
| `cancel`       | cancelar, dar de baja, retirar                                                                       |

### Scenario 13.1: Multiple intents matched

- GIVEN a customer message "cuánto vale el plan de internet"
- WHEN intent extraction runs
- THEN `customer_explicit_intents` MUST contain `["internet", "precio", "planes"]` (order follows declaration in `INTENT_KEYWORDS` table; uniqueness preserved)

### Scenario 13.2: No keyword matched

- GIVEN a customer message "hola buenos días"
- WHEN intent extraction runs
- THEN `customer_explicit_intents` MUST be empty (`[]`)

### Scenario 13.3: Intent keys in HUD, not raw text

- GIVEN a customer message "quiero contratar, cuánto cuesta"
- WHEN the `[turn_state]` HUD block is built
- THEN `customer_explicit_intents` in the block MUST be `[contratar, precio]`, NOT the raw substrings

---

## Requirement 14: Config Kill Switch

`Config` MUST expose a boolean field `enable_ai_guardrails` (default `true`).
When set to `false`, all guardrail checks in Requirements 9 and 10 MUST be bypassed.

### Scenario 14.1: Default value is true (guardrails active)

- GIVEN the environment does not set any guardrail override
- WHEN the server starts
- THEN `Config.enable_ai_guardrails` MUST be `true`

### Scenario 14.2: Set to false bypasses all guardrails

- GIVEN `Config.enable_ai_guardrails = false`
- WHEN `exec_check_coverage` or `exec_report_payment` executes
- THEN both guardrails (Requirements 9 and 10) MUST be skipped
- AND the tools run as if the guardrail logic does not exist

---

# Added: AI Agent — Persisted Conversation State (Phase 2)

## Requirement 15: WaConversationAiState Struct + Persistence

`WaConversation` MUST embed `ai_conv_state: Option<WaConversationAiState>` with
`#[serde(rename = "aiConvState", skip_serializing_if = "Option::is_none", default)]`.
Existing documents without the field MUST deserialize as `None` (zero migration).

`WaConversationAiState` MUST contain:

| Field | Type | Notes |
|---|---|---|
| `current_intent` | `Option<String>` | One of `customer_explicit_intents` group keys |
| `intent_confidence` | `Option<f32>` | 0.0–1.0; v1 binary 1.0 from keyword guardrail match |
| `collected_data` | `BTreeMap<String, String>` | Freeform; max 20 keys × 500 chars/value |
| `pending_data` | `Vec<String>` | Keys the agent is awaiting |
| `completed_actions` | `Vec<String>` | Deduped tool names that completed successfully |
| `current_step` | `Option<String>` | E.g. `"transferred_to_ventas"`, `"ticket_created"` |
| `failed_attempts` | `Vec<FailedAttempt>` | FIFO-capped at 5 |
| `updated_at` | `DateTime<Utc>` | Set on every write |

`FailedAttempt` MUST contain `tool: String`, `error: String`, `at: DateTime<Utc>`.

### Scenario 15.1: Zero-migration deserialization

- GIVEN an existing `WaConversation` document without `aiConvState`
- WHEN it is deserialized into `WaConversation`
- THEN `ai_conv_state` MUST be `None` with no deserialization error

### Scenario 15.2: Struct roundtrip

- GIVEN a `WaConversationAiState` with all fields populated
- WHEN serialized to BSON and deserialized back
- THEN all fields MUST be equal to the original values

---

## Requirement 16: ToolResult state_patches Contract

`ToolResult` MUST include `state_patches: Vec<StatePatch>` (not `Option<StatePatch>`).
Existing constructors `ToolResult::ok` and `ToolResult::err` MUST default this field to `vec![]`.

`StatePatch` MUST be an enum with exactly these variants:

| Variant | Fields |
|---|---|
| `SetIntent` | `intent: String`, `confidence: f32` |
| `SetCollectedData` | `key: String`, `value: String` |
| `AddCompletedAction` | `String` |
| `SetCurrentStep` | `String` |
| `AddFailedAttempt` | `tool: String`, `error: String` |

Each `exec_*` MUST return the following `state_patches` on its success path:

| Tool | state_patches (success) |
|---|---|
| `lookup_customer` | `[SetCollectedData{"client_id", id}, AddCompletedAction("lookup_customer")]` |
| `check_coverage` | `[SetCollectedData{"zone", zone}, AddCompletedAction("check_coverage")]` |
| `list_plans` | `[AddCompletedAction("list_plans")]` |
| `get_invoices` | `[AddCompletedAction("get_invoices")]` |
| `calculate_amount_bs` | `[AddCompletedAction("calculate_amount_bs")]` |
| `report_payment` (new payment) | `[AddCompletedAction("report_payment"), SetCurrentStep("payment_reported")]` |
| `report_payment` (already_registered) | `[SetCurrentStep("payment_already_registered")]` |
| `transfer_to_agent` (same-workspace) | `[SetCurrentStep("transferred")]` |
| `create_ticket` | `[SetCurrentStep("ticket_created"), AddCompletedAction("create_ticket")]` |
| `request_human` | `[SetCurrentStep("awaiting_human")]` |

All tools on error path MUST return `state_patches: vec![]` (dispatcher may add `AddFailedAttempt` generically).

### Scenario 16.1: Backward-compat default

- GIVEN existing call sites that construct `ToolResult::ok(value)` without specifying patches
- WHEN `state_patches` is added to the struct
- THEN `state_patches` MUST default to `vec![]` without requiring callers to change

### Scenario 16.2: lookup_customer patches

- GIVEN `exec_lookup_customer` succeeds with `client_id = "abc123"`
- WHEN the `ToolResult` is returned
- THEN `state_patches` MUST be `[SetCollectedData { key: "client_id", value: "abc123" }, AddCompletedAction("lookup_customer")]`

### Scenario 16.3: report_payment already_registered patches

- GIVEN `exec_report_payment` returns `already_registered = true`
- WHEN the `ToolResult` is returned
- THEN `state_patches` MUST be `[SetCurrentStep("payment_already_registered")]`
- AND `AddCompletedAction("report_payment")` MUST NOT be present

---

## Requirement 17: RunnerOutput state_patches Accumulation

`RunnerOutput` MUST include `state_patches: Vec<StatePatch>` that accumulates ALL patches
emitted by `ToolResult`s during the chain loop of a single `run_turn` call.

### Scenario 17.1: Patches accumulate across multiple tool calls in one turn

- GIVEN a turn where `lookup_customer` and `list_plans` are called (both succeed)
- WHEN `run_turn` returns `RunnerOutput`
- THEN `state_patches` MUST contain all patches from both tools in call order:
  `[SetCollectedData{"client_id",…}, AddCompletedAction("lookup_customer"), AddCompletedAction("list_plans")]`

### Scenario 17.2: No tool calls — empty patches

- GIVEN a turn with no tool calls
- WHEN `run_turn` returns
- THEN `RunnerOutput.state_patches` MUST be `vec![]`

---

## Requirement 18: [conversation_state] HUD Block

`build_system_instruction` MUST accept a `conversation_state: Option<&str>` parameter.
When `Some`, it MUST inject a `[conversation_state]` block AFTER `[turn_state]` and BEFORE `[faqs]`.
When `None`, the block MUST be omitted entirely.

Block format — emit only non-empty/non-None fields:

```
[conversation_state]
current_intent: <value>
intent_confidence: <value>
collected_data: key1=value1, key2=value2, ...
pending_data: [v1, v2, ...]
completed_actions: [a1, a2, ...]
current_step: <value>
recent_failed_attempts: [tool1, tool2, ...]
```

`recent_failed_attempts` shows only the tool name (not full error) and is limited to last 5.

### Scenario 18.1: Block injected when state is Some

- GIVEN `conv.ai_conv_state = Some(state)` with `current_intent = "pago"` and `completed_actions = ["lookup_customer"]`
- WHEN `build_system_instruction` runs
- THEN the system instruction MUST contain a `[conversation_state]` block with those values
- AND it MUST appear after `[turn_state]` and before `[faqs]`

### Scenario 18.2: Block omitted when state is None

- GIVEN `conv.ai_conv_state = None`
- WHEN `build_system_instruction` runs
- THEN no `[conversation_state]` block MUST appear in the system instruction

### Scenario 18.3: Empty fields omitted from block

- GIVEN `collected_data` is empty and `pending_data` is empty
- WHEN the block is rendered
- THEN those lines MUST be omitted from the block output

---

## Requirement 19: Dispatch State Lifecycle

Dispatch MUST implement the full read → inject → fold → write cycle around `run_turn`.

### Scenario 19.1: State read and injected pre-runner

- GIVEN `conv.ai_conv_state = Some(state)` after the post-debounce fetch
- WHEN dispatch builds context for `run_turn`
- THEN it MUST format `ai_conv_state` as a `[conversation_state]` text block and pass it as `conversation_state: Some(...)`

### Scenario 19.2: Patches folded and persisted post-runner

- GIVEN `run_turn` returns `RunnerOutput` with non-empty `state_patches`
- WHEN dispatch processes the result (before sending the WhatsApp message)
- THEN it MUST fold all patches into a new `WaConversationAiState` value
- AND call `update_conversation_ai_conv_state(conv_id, new_state)` exactly once
- AND this write MUST occur within the `try_lock_ai_dispatch` window

### Scenario 19.3: Synthetic SetIntent from guardrails

- GIVEN `customer_explicit_intents` is non-empty for this turn AND `conv.ai_conv_state.current_intent` is `None`
- WHEN dispatch folds patches
- THEN it MUST emit a synthetic `SetIntent { intent: <first matched group>, confidence: 1.0 }` patch

### Scenario 19.4: SetIntent not emitted when intent already set

- GIVEN `conv.ai_conv_state.current_intent = Some("pago")`
- WHEN a different intent is detected in `customer_explicit_intents` for this turn
- THEN dispatch MUST NOT overwrite the existing `current_intent`

---

## Requirement 20: Reset Semantics

### Scenario 20.1: Transfer same-workspace — clears intent and step

- GIVEN `exec_transfer_to_agent` succeeds with a same-workspace target
- WHEN the patch is applied
- THEN `current_step` MUST be set to `"transferred_to_<target_label>"` (sanitized label)
- AND `current_intent` and `intent_confidence` MUST be cleared (set to `None`) by dispatch before the next turn
- AND `collected_data`, `completed_actions`, and `pending_data` MUST be preserved

### Scenario 20.2: Transfer cross-workspace — no reset

- GIVEN `exec_transfer_to_agent` succeeds with a cross-workspace target
- WHEN the patch is applied
- THEN no state field MUST be cleared (conv stays; client is just told the other number)

### Scenario 20.3: Create ticket — step set, state preserved

- GIVEN `exec_create_ticket` succeeds
- WHEN the patch is applied
- THEN `current_step` MUST be set to `"ticket_created"`
- AND all other state fields MUST be preserved as audit

### Scenario 20.4: Conversation reopen — full wipe

- GIVEN the existing reopen flow runs (`update_conversation_ai_state` clears `ai_disabled`)
- WHEN reopen executes
- THEN `ai_conv_state` MUST be set to `None` in the conv document
- AND a `tracing::info!` MUST log "ai_conv_state cleared on reopen"

---

## Requirement 21: Caps and Trimming

### Scenario 21.1: collected_data cap at 20 keys

- GIVEN `collected_data.len() == 20`
- WHEN a `SetCollectedData` patch is applied
- THEN the oldest key MUST be evicted (insertion-order FIFO) before inserting the new one
- AND values longer than 500 characters MUST be silently truncated with a `tracing::warn!`

### Scenario 21.2: failed_attempts FIFO trim at 5

- GIVEN `failed_attempts.len() == 5`
- WHEN an `AddFailedAttempt` patch is applied
- THEN the oldest attempt MUST be removed before appending the new one

### Scenario 21.3: completed_actions dedup

- GIVEN `completed_actions` already contains `"lookup_customer"`
- WHEN an `AddCompletedAction("lookup_customer")` patch is applied
- THEN `"lookup_customer"` MUST NOT be added again

---

## Requirement 22: UI Exposure

`WhatsAppRepository` MUST gain a trait method `update_conversation_ai_conv_state(conv_id, state)`.
The Mongo impl MUST use `$set` to persist the value atomically.

### Scenario 22.1: GET /conversations/:id returns ai_conv_state

- GIVEN a conversation with `ai_conv_state = Some(...)`
- WHEN `GET /v1/auth-user/whatsapp/conversations/:id` is called
- THEN the response body MUST include `ai_conv_state` as a nested object with all populated fields

### Scenario 22.2: List endpoint unaffected

- GIVEN the conversations list endpoint
- WHEN called
- THEN per-item responses MAY omit `ai_conv_state` (out of scope for this change)

### Scenario 22.3: Manual reset endpoint

- GIVEN `POST /v1/auth-user/whatsapp/conversations/:id/agent-state/reset` is called
- AND the caller's JWT claims satisfy: `bCanChat == true` AND `nRole in [0.0, 0.5, 1.0]` (superadmin / operador / contador)
- WHEN the endpoint executes
- THEN `ai_conv_state` MUST be set to `None` in the conv document
- AND a `WaAudit` document MUST be created with `action = "ai_conv_state_reset"`, `actor_id = claims.id`, `actor_name`, `target_id = conv_id`, `note = "Manual AI state reset"`, `created_at = now()`
- AND a `tracing::info!` MUST record `"ai_conv_state reset by user_id={...}"`
- AND the back MUST broadcast a WS event `CONVERSACION_ESTADO_IA` (per Requirement 24) with the new state (`null`)
- AND the response MUST be `{ "ok": true, "conversation_id": "..." }`
- AND the endpoint MUST acquire `try_lock_ai_dispatch` to prevent races with an in-flight turn

### Scenario 22.4: Reset permission denied

- GIVEN the caller's JWT claims do NOT satisfy `bCanChat == true AND nRole in [0.0, 0.5, 1.0]`
- WHEN `POST .../agent-state/reset` is called
- THEN the response MUST be HTTP 403 with body `{ "ok": false, "error": "forbidden" }`
- AND no DB or audit write MUST occur

---

## Requirement 23: Kill Switch — enable_ai_conversation_state

`Config` MUST expose `enable_ai_conversation_state: bool` (default `true`).
When `false`, dispatch MUST skip the `[conversation_state]` block injection AND skip the
post-turn state write. Accumulated `state_patches` from tools MUST be silently discarded.

### Scenario 23.1: Kill switch off — no read, no write

- GIVEN `Config.enable_ai_conversation_state = false`
- WHEN dispatch runs a turn
- THEN no `[conversation_state]` block MUST appear in the system instruction
- AND `update_conversation_ai_conv_state` MUST NOT be called
- AND tools still execute normally; their `state_patches` are simply discarded

### Scenario 23.2: Kill switch on — default behavior

- GIVEN `Config.enable_ai_conversation_state = true` (default, env var not set)
- WHEN the server starts
- THEN the full state lifecycle (Requirements 19–21) MUST be active

---

## Requirement 24: WS Broadcast on State Change

The system MUST broadcast a WebSocket event `CONVERSACION_ESTADO_IA` to all connected agents whenever `ai_conv_state` changes for a conversation. This includes:
- Post-turn state writes from `dispatch.rs` (after `apply_state_patches` runs)
- Manual resets via the reset endpoint
- Conversation reopen wipes (when `ai_conv_state` is cleared)

### Scenario 24.1: Event shape

The broadcast event payload MUST be:
```json
{
  "tipo": "CONVERSACION_ESTADO_IA",
  "conversation_id": "<hex ObjectId>",
  "ai_conv_state": <WaConversationAiState | null>
}
```

### Scenario 24.2: Broadcast scope

- GIVEN a state mutation occurs for `conversation_id = C`
- WHEN the broadcast is emitted
- THEN it MUST go to ALL agents in the WsRegistry (not filtered by assignment), consistent with existing `MENSAJE_ACTUALIZADO` and `CHAT_TOMADO` patterns

### Scenario 24.3: No event when state unchanged

- GIVEN a turn produces zero state_patches AND `current_intent` derivation is also a no-op (no change)
- WHEN dispatch finishes
- THEN no `CONVERSACION_ESTADO_IA` event MUST be emitted (avoid noise)

---

# Added: AI Agent — Phase 3a: Pre-classifier + Metrics

## Requirement 25: Pre-classifier — opt-in per workspace

`WaSettings` MUST expose `pre_classifier_enabled: bool` (default `false`). When
`false`, dispatch MUST skip the pre-classifier entirely — no Gemini call, no
behavioral change. When `true`, dispatch MUST invoke the pre-classifier AFTER
keyword escalation and BEFORE `select_agent`.

### Scenario 25.1: Disabled by default — no LLM call

- GIVEN `WaSettings.pre_classifier_enabled = false`
- WHEN dispatch processes any inbound message
- THEN it MUST skip the pre-classifier AND continue with existing flow
- AND no Gemini Flash Lite call MUST be made

### Scenario 25.2: Opt-in activation — pre-classifier runs first

- GIVEN `WaSettings.pre_classifier_enabled = true` AND the inbound has non-empty text
- WHEN dispatch runs
- THEN it MUST invoke the pre-classifier BEFORE `select_agent` and BEFORE keyword escalation routing

### Scenario 25.3: Skipped on media-only messages

- GIVEN `pre_classifier_enabled = true` AND the inbound message text is empty or whitespace
- WHEN dispatch runs
- THEN it MUST skip the pre-classifier AND fall through to existing flow

---

## Requirement 26: Pre-classifier output contract

The pre-classifier MUST return a `PreClassResult` enum and an associated
confidence score. Results with `confidence < 0.85` MUST be treated as `Ambiguous`
by dispatch (confidence gate), but the original variant MUST still be persisted
to `AiInteraction.pre_class_result` for audit. The pre-classifier MUST use
`gemini-2.5-flash-lite` (not the main agent model). On Gemini error or
JSON-parse failure, dispatch MUST fall through to existing flow (treat as
`Ambiguous`) and log a warning.

`PreClassResult` variants:

| Variant | Meaning |
|---|---|
| `Spam` | Noise, forwarded chains, advertising |
| `GreetingOnly` | Single-word greeting, lone emoji |
| `ClearVentas` | Explicit contracting / pricing intent |
| `ClearPagos` | Explicit payment intent |
| `ClearSoporte` | Explicit technical support issue |
| `Ambiguous` | Uncertain or mixed intent |

### Scenario 26.1: High-confidence result used directly

- GIVEN the pre-classifier returns `result = ClearSoporte, confidence = 0.92`
- WHEN dispatch evaluates the result
- THEN it MUST treat the result as `ClearSoporte` and route accordingly

### Scenario 26.2: Low-confidence forces Ambiguous routing

- GIVEN the pre-classifier returns `result = ClearVentas, confidence = 0.72`
- WHEN dispatch evaluates the result
- THEN it MUST route as `Ambiguous` (fall through to existing flow)
- AND `AiInteraction.pre_class_result` MUST record `"ClearVentas"` (the original variant)

### Scenario 26.3: JSON parse failure treated as Ambiguous

- GIVEN Gemini responds with malformed JSON or missing fields
- WHEN the pre-classifier parses the response
- THEN dispatch MUST treat the result as `Ambiguous` AND emit a `tracing::warn!`
- AND no agent MUST be short-circuited

### Scenario 26.4: Required JSON output schema

The pre-classifier prompt MUST instruct Gemini to respond with:
```json
{ "result": "<variant>", "confidence": <0.0–1.0>, "reasoning": "<≤50 chars Spanish>" }
```
- `reasoning` is for audit logs only; it MUST NOT be sent to the customer

---

## Requirement 27: Pre-classifier action mapping

### Scenario 27.1: Spam → trivial response or silent drop

- GIVEN `result = Spam AND confidence >= 0.85`
- WHEN dispatch evaluates
- THEN it MUST look up an enabled `TrivialResponse` with `kind = "spam"` matching `user_text`
- AND IF a match is found: send the template's `response` text; persist `AiInteraction { pre_classified: true, pre_class_result: "Spam" }`; return early without invoking any agent
- AND IF no match: silently drop (no response); still persist `AiInteraction { pre_classified: true }`

### Scenario 27.2: GreetingOnly → trivial response, fallback if no match

- GIVEN `result = GreetingOnly AND confidence >= 0.85`
- WHEN dispatch evaluates
- THEN it MUST look up an enabled `TrivialResponse` with `kind = "greeting"`
- AND IF a match is found: send the template; persist `AiInteraction { pre_classified: true }`
- AND IF no match: fall through to existing flow (MUST NOT silently drop greetings)

### Scenario 27.3: Clear* → bypass receptionist, direct to specialist

- GIVEN `result in [ClearVentas, ClearPagos, ClearSoporte] AND confidence >= 0.85`
- WHEN dispatch evaluates
- THEN it MUST resolve the corresponding specialized agent using the same mechanism as `transfer_to_agent`
- AND IF a target agent is found: invoke `run_turn` directly (skip Sofía); persist `AiInteraction { pre_classified: true, pre_class_result: <variant> }`
- AND IF no target agent found: fall through to existing `select_agent` flow; persist `AiInteraction { pre_classified: false }`

### Scenario 27.4: Ambiguous → existing flow unchanged

- GIVEN `result = Ambiguous` (directly, via confidence gate, or via parse failure)
- WHEN dispatch evaluates
- THEN it MUST fall through to existing `select_agent` flow
- AND `AiInteraction.pre_classified` MUST be `false`

---

## Requirement 28: TrivialResponse data shape

`WaSettings` MUST embed `trivial_responses: Vec<TrivialResponse>` (default empty,
`#[serde(default)]`). `TrivialResponse` MUST contain:

| Field | Type | Notes |
|---|---|---|
| `id` | `String` | UUID v4 |
| `kind` | `String` | `"spam"` \| `"greeting"` \| `"sticker"` |
| `triggers` | `Vec<String>` | Substring triggers; case-insensitive after `normalize_zone` |
| `response` | `String` | Text sent verbatim to customer |
| `enabled` | `bool` | `false` = soft-disable without deletion |
| `priority` | `i32` | Default `0`; higher wins on tiebreaker |

### Scenario 28.1: Empty triggers acts as catch-all fallback

- GIVEN a `TrivialResponse` with `enabled = true AND triggers = []`
- WHEN dispatch looks up a trivial for `kind = "X"`
- THEN this entry MUST be selected only if no entry with non-empty triggers matched first

### Scenario 28.2: Tiebreaker — highest priority, then insertion order

- GIVEN multiple enabled entries with `kind = "X"` whose triggers all match
- WHEN dispatch picks one
- THEN it MUST select the entry with the HIGHEST `priority` value
- AND on priority tie: the FIRST entry in the list (insertion order) wins

---

## Requirement 29: AiInteraction schema extensions

`AiInteraction` MUST add four new fields, all with `#[serde(default)]`:

| Field | Type | Default | Source |
|---|---|---|---|
| `thinking_tokens` | `u32` | `0` | `thoughtsTokenCount` from Gemini `UsageMetadata` |
| `cached_tokens` | `u32` | `0` | `cachedContentTokenCount` from Gemini `UsageMetadata` |
| `pre_classified` | `bool` | `false` | `true` iff pre-classifier short-circuited dispatch |
| `pre_class_result` | `Option<String>` | `None` | Pre-classifier variant name when it ran |

### Scenario 29.1: Backward compatibility — legacy documents deserialize cleanly

- GIVEN an `AiInteraction` document without the four new fields
- WHEN deserialized
- THEN all four fields MUST take their default values without error
- AND no migration script SHALL be required

### Scenario 29.2: New document persists all four fields

- GIVEN a turn where the pre-classifier ran and returned `ClearSoporte` with `confidence = 0.90`
- WHEN the `AiInteraction` is persisted
- THEN `pre_classified = true`, `pre_class_result = Some("ClearSoporte")`, `cached_tokens >= 0`, `thinking_tokens >= 0` MUST all be present in the document

---

## Requirement 30: Metrics endpoint

`GET /v1/auth-user/whatsapp/ai-agent/agents/:id/metrics?from=&to=&granularity=` MUST
exist under the `user_jwt_auth_middleware` group (consistent with all other AI Agent
routes which live under `/v1/auth-user/whatsapp/ai-agent/...`).

Query params: `from` and `to` (RFC3339, required); `granularity` (`summary` default | `daily`).

**Summary response shape:**
```json
{
  "ok": true,
  "data": {
    "agent_id": "<hex>", "from": "<RFC3339>", "to": "<RFC3339>",
    "summary": {
      "total_turns": <u64>, "total_input_tokens": <u64>,
      "total_output_tokens": <u64>, "total_thinking_tokens": <u64>,
      "total_cached_tokens": <u64>, "total_cost_usd": <f64>,
      "avg_latency_ms": <f64>, "pre_classified_count": <u64>,
      "pre_classified_breakdown": { "Spam": <u64>, "GreetingOnly": <u64>,
        "ClearVentas": <u64>, "ClearPagos": <u64>,
        "ClearSoporte": <u64>, "Ambiguous": <u64> },
      "escalated_count": <u64>, "tool_calls_count": <u64>,
      "cache_hit_rate": <f64>
    }
  }
}
```
`cache_hit_rate = total_cached_tokens / total_input_tokens` (0 when input is 0).

### Scenario 30.1: Valid request returns aggregate summary

- GIVEN a valid `agent_id`, parseable `from`/`to`, and at least one matching `AiInteraction`
- WHEN the endpoint is called
- THEN it MUST return HTTP 200 with the summary shape above

### Scenario 30.2: Invalid agent_id → 400

- GIVEN `id` is not a valid hex ObjectId
- WHEN the endpoint is called
- THEN it MUST return `{ "ok": false, "error": "invalid_agent_id" }`

### Scenario 30.3: Invalid or missing date params → 400

- GIVEN `from` or `to` cannot be parsed, OR `from > to`
- WHEN the endpoint is called
- THEN it MUST return `{ "ok": false, "error": "invalid_date_range" }`

### Scenario 30.4: Agent not found → 404

- GIVEN `id` is a valid ObjectId but no `AiAgent` document exists
- WHEN the endpoint is called
- THEN it MUST return `{ "ok": false, "error": "agent_not_found" }`

### Scenario 30.5: Empty window returns zero summary

- GIVEN no `AiInteraction` documents fall in `[from, to]` for this agent
- WHEN the endpoint is called
- THEN it MUST return HTTP 200 with all `total_*` fields at `0`

### Scenario 30.6: granularity=daily adds per-day breakdown

- GIVEN `?granularity=daily` is provided
- WHEN the endpoint responds
- THEN the response MUST include `daily_breakdown: Vec<DailyBucket>` at the same level as `summary`
- AND each bucket MUST contain `date: "YYYY-MM-DD"` (Venezuela tz) plus a subset of summary fields
- AND buckets MUST be sorted ascending by date; missing days MAY be omitted

---

## Requirement 31: MongoDB index for AiInteractions

`scripts/create_indexes.js` MUST add a compound index on
`AiInteractions { agent_id: 1, created_at: -1 }`.
The metrics aggregate MUST use this index; a full-collection scan is not acceptable
at production scale (~12 000 interactions/day).

### Scenario 31.1: Index created by setup script

- GIVEN `mongosh <URI> < scripts/create_indexes.js` is run
- WHEN the command completes
- THEN the collection `AiInteractions` MUST have the compound index `agent_id_1_created_at_-1`
- AND the index MUST NOT already exist check (idempotent `createIndex` call)

---

## Requirement 32: cost_usd_estimate consistency

Per-model cost rate constants MUST live in a single table in `gemini.rs`; they
MUST NOT be duplicated across modules. Pre-classifier interactions (short-circuited
turns) MUST record `cost_usd_estimate` reflecting only the Flash Lite tokens (not
the main agent model). Implicit cache hits (`cached_tokens > 0`) MUST be billed at
25% of the standard input rate (75% discount) in the estimate.

### Scenario 32.1: Pre-classifier turn cost reflects Flash Lite only

- GIVEN a turn where the pre-classifier ran and returned `Spam` (no main-agent call)
- WHEN `AiInteraction.cost_usd_estimate` is computed
- THEN it MUST reflect Flash Lite's input + output tokens only
- AND the main-agent model's rate MUST NOT be applied

### Scenario 32.2: Cached tokens billed at 25% input rate

- GIVEN a turn where `cached_tokens = 1000` and `input_tokens = 5000` (4000 non-cached)
- WHEN `cost_usd_estimate` is computed
- THEN cached_tokens MUST be charged at 25% of the standard input rate
- AND non-cached input tokens MUST be charged at the standard input rate
