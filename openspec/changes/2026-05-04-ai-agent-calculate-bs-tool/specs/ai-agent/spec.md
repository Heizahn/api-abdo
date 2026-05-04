# Delta for AI Agent

## ADDED Requirements

### Requirement: calculate_amount_bs Tool

The system MUST expose a tool named `calculate_amount_bs` in the AI Agent tool
registry. The tool MUST be categorized as `InfoLookup` and MUST convert a USD
amount to Bs using the current BCV exchange rate and the EMPRESARIAL IVA
configuration. The tool MUST NOT fall back to any other `sTarget` value if
`EMPRESARIAL` is absent.

#### Scenario 1: Successful conversion

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

#### Scenario 2: Invalid amount — zero or negative

- GIVEN `amount_usd <= 0.0`
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code `invalid_amount`

#### Scenario 3: Missing or malformed arguments

- GIVEN the tool is called without the `amount_usd` field (or with a
  non-numeric value that cannot be parsed as `f64`)
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code `invalid_args`

#### Scenario 4: BCV rate unavailable

- GIVEN Redis returns no rate (miss or error) AND the DB query for the latest
  exchange rate also returns no result
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code
  `exchange_rate_unavailable`

#### Scenario 5: BCV rate is zero

- GIVEN the resolved exchange rate (from Redis or DB) is exactly `0.0`
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code `exchange_rate_zero`

#### Scenario 6: EMPRESARIAL tax doc missing

- GIVEN no document with `sTarget = "EMPRESARIAL"` exists in the `BCV.IVA`
  collection
- WHEN the tool executes
- THEN it MUST return a `ToolResult::err` with error code `tax_config_missing`
- AND the tool MUST NOT fall back to a document with `sTarget = "DEFAULT"`
  or any other value

#### Scenario 7: Redis rate preferred over DB

- GIVEN Redis returns a valid non-zero exchange rate for the current day
- WHEN the tool executes
- THEN it MUST use the Redis-sourced rate without issuing a DB query for the
  exchange rate

#### Scenario 8: Tool category is InfoLookup

- GIVEN the tool registry is queried for `calculate_amount_bs` category
- WHEN `tool_category("calculate_amount_bs")` is evaluated
- THEN it MUST return `InfoLookup`
- AND per Requirement 1 of the `no_resolution` counter spec, successful
  execution of this tool MUST NOT reset `no_resolution_count` and MUST NOT
  increment it

#### Scenario 9: Sandbox parity

- GIVEN a conversation where `is_sandbox = true`
- WHEN the tool executes with valid inputs
- THEN it MUST behave identically to `is_sandbox = false`
- AND no special sandbox branch SHALL exist for this tool (it is read-only)

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
