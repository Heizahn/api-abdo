# Delta Spec: AI Agent — Persisted Conversation State (Phase 2)

## Capability: ai-agent / conversation state

Delta over `openspec/specs/ai-agent/spec.md` (Requirements 1–14). Adds Requirements 15–23.

---

## ADDED Requirements

### Requirement 15: WaConversationAiState Struct + Persistence

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

#### Scenario 15.1: Zero-migration deserialization

- GIVEN an existing `WaConversation` document without `aiConvState`
- WHEN it is deserialized into `WaConversation`
- THEN `ai_conv_state` MUST be `None` with no deserialization error

#### Scenario 15.2: Struct roundtrip

- GIVEN a `WaConversationAiState` with all fields populated
- WHEN serialized to BSON and deserialized back
- THEN all fields MUST be equal to the original values

---

### Requirement 16: ToolResult state_patches Contract

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

#### Scenario 16.1: Backward-compat default

- GIVEN existing call sites that construct `ToolResult::ok(value)` without specifying patches
- WHEN `state_patches` is added to the struct
- THEN `state_patches` MUST default to `vec![]` without requiring callers to change

#### Scenario 16.2: lookup_customer patches

- GIVEN `exec_lookup_customer` succeeds with `client_id = "abc123"`
- WHEN the `ToolResult` is returned
- THEN `state_patches` MUST be `[SetCollectedData { key: "client_id", value: "abc123" }, AddCompletedAction("lookup_customer")]`

#### Scenario 16.3: report_payment already_registered patches

- GIVEN `exec_report_payment` returns `already_registered = true`
- WHEN the `ToolResult` is returned
- THEN `state_patches` MUST be `[SetCurrentStep("payment_already_registered")]`
- AND `AddCompletedAction("report_payment")` MUST NOT be present

---

### Requirement 17: RunnerOutput state_patches Accumulation

`RunnerOutput` MUST include `state_patches: Vec<StatePatch>` that accumulates ALL patches
emitted by `ToolResult`s during the chain loop of a single `run_turn` call.

#### Scenario 17.1: Patches accumulate across multiple tool calls in one turn

- GIVEN a turn where `lookup_customer` and `list_plans` are called (both succeed)
- WHEN `run_turn` returns `RunnerOutput`
- THEN `state_patches` MUST contain all patches from both tools in call order:
  `[SetCollectedData{"client_id",…}, AddCompletedAction("lookup_customer"), AddCompletedAction("list_plans")]`

#### Scenario 17.2: No tool calls — empty patches

- GIVEN a turn with no tool calls
- WHEN `run_turn` returns
- THEN `RunnerOutput.state_patches` MUST be `vec![]`

---

### Requirement 18: [conversation_state] HUD Block

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

#### Scenario 18.1: Block injected when state is Some

- GIVEN `conv.ai_conv_state = Some(state)` with `current_intent = "pago"` and `completed_actions = ["lookup_customer"]`
- WHEN `build_system_instruction` runs
- THEN the system instruction MUST contain a `[conversation_state]` block with those values
- AND it MUST appear after `[turn_state]` and before `[faqs]`

#### Scenario 18.2: Block omitted when state is None

- GIVEN `conv.ai_conv_state = None`
- WHEN `build_system_instruction` runs
- THEN no `[conversation_state]` block MUST appear in the system instruction

#### Scenario 18.3: Empty fields omitted from block

- GIVEN `collected_data` is empty and `pending_data` is empty
- WHEN the block is rendered
- THEN those lines MUST be omitted from the block output

---

### Requirement 19: Dispatch State Lifecycle

Dispatch MUST implement the full read → inject → fold → write cycle around `run_turn`.

#### Scenario 19.1: State read and injected pre-runner

- GIVEN `conv.ai_conv_state = Some(state)` after the post-debounce fetch
- WHEN dispatch builds context for `run_turn`
- THEN it MUST format `ai_conv_state` as a `[conversation_state]` text block and pass it as `conversation_state: Some(...)`

#### Scenario 19.2: Patches folded and persisted post-runner

- GIVEN `run_turn` returns `RunnerOutput` with non-empty `state_patches`
- WHEN dispatch processes the result (before sending the WhatsApp message)
- THEN it MUST fold all patches into a new `WaConversationAiState` value
- AND call `update_conversation_ai_conv_state(conv_id, new_state)` exactly once
- AND this write MUST occur within the `try_lock_ai_dispatch` window

#### Scenario 19.3: Synthetic SetIntent from guardrails

- GIVEN `customer_explicit_intents` is non-empty for this turn AND `conv.ai_conv_state.current_intent` is `None`
- WHEN dispatch folds patches
- THEN it MUST emit a synthetic `SetIntent { intent: <first matched group>, confidence: 1.0 }` patch

#### Scenario 19.4: SetIntent not emitted when intent already set

- GIVEN `conv.ai_conv_state.current_intent = Some("pago")`
- WHEN a different intent is detected in `customer_explicit_intents` for this turn
- THEN dispatch MUST NOT overwrite the existing `current_intent`

---

### Requirement 20: Reset Semantics

#### Scenario 20.1: Transfer same-workspace — clears intent and step

- GIVEN `exec_transfer_to_agent` succeeds with a same-workspace target
- WHEN the patch is applied
- THEN `current_step` MUST be set to `"transferred_to_<target_label>"` (sanitized label)
- AND `current_intent` and `intent_confidence` MUST be cleared (set to `None`) by dispatch before the next turn
- AND `collected_data`, `completed_actions`, and `pending_data` MUST be preserved

#### Scenario 20.2: Transfer cross-workspace — no reset

- GIVEN `exec_transfer_to_agent` succeeds with a cross-workspace target
- WHEN the patch is applied
- THEN no state field MUST be cleared (conv stays; client is just told the other number)

#### Scenario 20.3: Create ticket — step set, state preserved

- GIVEN `exec_create_ticket` succeeds
- WHEN the patch is applied
- THEN `current_step` MUST be set to `"ticket_created"`
- AND all other state fields MUST be preserved as audit

#### Scenario 20.4: Conversation reopen — full wipe

- GIVEN the existing reopen flow runs (`update_conversation_ai_state` clears `ai_disabled`)
- WHEN reopen executes
- THEN `ai_conv_state` MUST be set to `None` in the conv document
- AND a `tracing::info!` MUST log "ai_conv_state cleared on reopen"

---

### Requirement 21: Caps and Trimming

#### Scenario 21.1: collected_data cap at 20 keys

- GIVEN `collected_data.len() == 20`
- WHEN a `SetCollectedData` patch is applied
- THEN the oldest key MUST be evicted (insertion-order FIFO) before inserting the new one
- AND values longer than 500 characters MUST be silently truncated with a `tracing::warn!`

#### Scenario 21.2: failed_attempts FIFO trim at 5

- GIVEN `failed_attempts.len() == 5`
- WHEN an `AddFailedAttempt` patch is applied
- THEN the oldest attempt MUST be removed before appending the new one

#### Scenario 21.3: completed_actions dedup

- GIVEN `completed_actions` already contains `"lookup_customer"`
- WHEN an `AddCompletedAction("lookup_customer")` patch is applied
- THEN `"lookup_customer"` MUST NOT be added again

---

### Requirement 22: UI Exposure

`WhatsAppRepository` MUST gain a trait method `update_conversation_ai_conv_state(conv_id, state)`.
The Mongo impl MUST use `$set` to persist the value atomically.

#### Scenario 22.1: GET /conversations/:id returns ai_conv_state

- GIVEN a conversation with `ai_conv_state = Some(...)`
- WHEN `GET /v1/auth-user/whatsapp/conversations/:id` is called
- THEN the response body MUST include `ai_conv_state` as a nested object with all populated fields

#### Scenario 22.2: List endpoint unaffected

- GIVEN the conversations list endpoint
- WHEN called
- THEN per-item responses MAY omit `ai_conv_state` (out of scope for this change)

#### Scenario 22.3: Manual reset endpoint

- GIVEN `POST /v1/auth-user/whatsapp/conversations/:id/agent-state/reset` is called
- AND the caller's JWT claims satisfy: `bCanChat == true` AND `nRole in [0.0, 0.5, 1.0]` (superadmin / operador / contador)
- WHEN the endpoint executes
- THEN `ai_conv_state` MUST be set to `None` in the conv document
- AND a `WaAudit` document MUST be created with `action = "ai_conv_state_reset"`, `actor_id = claims.id`, `actor_name`, `target_id = conv_id`, `note = "Manual AI state reset"`, `created_at = now()`
- AND a `tracing::info!` MUST record `"ai_conv_state reset by user_id={...}"`
- AND the back MUST broadcast a WS event `CONVERSACION_ESTADO_IA` (per Requirement 24) with the new state (`null`)
- AND the response MUST be `{ "ok": true, "conversation_id": "..." }`
- AND the endpoint MUST acquire `try_lock_ai_dispatch` to prevent races with an in-flight turn

#### Scenario 22.4: Reset permission denied

- GIVEN the caller's JWT claims do NOT satisfy `bCanChat == true AND nRole in [0.0, 0.5, 1.0]`
- WHEN `POST .../agent-state/reset` is called
- THEN the response MUST be HTTP 403 with body `{ "ok": false, "error": "forbidden" }`
- AND no DB or audit write MUST occur

---

### Requirement 24: WS Broadcast on State Change

The system MUST broadcast a WebSocket event `CONVERSACION_ESTADO_IA` to all connected agents whenever `ai_conv_state` changes for a conversation. This includes:
- Post-turn state writes from `dispatch.rs` (after `apply_state_patches` runs)
- Manual resets via the reset endpoint
- Conversation reopen wipes (when `ai_conv_state` is cleared)

#### Scenario 24.1: Event shape

The broadcast event payload MUST be:
```json
{
  "tipo": "CONVERSACION_ESTADO_IA",
  "conversation_id": "<hex ObjectId>",
  "ai_conv_state": <WaConversationAiState | null>
}
```

#### Scenario 24.2: Broadcast scope

- GIVEN a state mutation occurs for `conversation_id = C`
- WHEN the broadcast is emitted
- THEN it MUST go to ALL agents in the WsRegistry (not filtered by assignment), consistent with existing `MENSAJE_ACTUALIZADO` and `CHAT_TOMADO` patterns

#### Scenario 24.3: No event when state unchanged

- GIVEN a turn produces zero state_patches AND `current_intent` derivation is also a no-op (no change)
- WHEN dispatch finishes
- THEN no `CONVERSACION_ESTADO_IA` event MUST be emitted (avoid noise)

---

### Requirement 23: Kill Switch — enable_ai_conversation_state

`Config` MUST expose `enable_ai_conversation_state: bool` (default `true`).
When `false`, dispatch MUST skip the `[conversation_state]` block injection AND skip the
post-turn state write. Accumulated `state_patches` from tools MUST be silently discarded.

#### Scenario 23.1: Kill switch off — no read, no write

- GIVEN `Config.enable_ai_conversation_state = false`
- WHEN dispatch runs a turn
- THEN no `[conversation_state]` block MUST appear in the system instruction
- AND `update_conversation_ai_conv_state` MUST NOT be called
- AND tools still execute normally; their `state_patches` are simply discarded

#### Scenario 23.2: Kill switch on — default behavior

- GIVEN `Config.enable_ai_conversation_state = true` (default, env var not set)
- WHEN the server starts
- THEN the full state lifecycle (Requirements 19–21) MUST be active
