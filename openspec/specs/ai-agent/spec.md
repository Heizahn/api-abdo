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

(Previously: table did not include `calculate_amount_bs`)

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
