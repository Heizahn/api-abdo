# Design — AI Agent: Persisted Conversation State (Phase 2)

> Status: design / pre-implementation
> Companion artifacts: [`proposal.md`](./proposal.md), spec at `openspec/specs/ai-agent/spec.md` (delta to come).
> Phase 1 dependency: `ai-agent-guardrails-and-hud` (already deployed).

## 1. ADRs

The proposal frames the WHAT. This section locks the HOW with eight rationale-bearing decisions. All eight are intentionally conservative — Phase 2 is additive, optional, and reversible by env flag.

### ADR-1 — Storage: embed `ai_conv_state: Option<WaConversationAiState>` in `WaConversation`

**Context.** State must be cheap to read on every inbound dispatch and easy to project into `ConversationItem` for the UI listing. We considered three options:

1. **Embed** the state document inside the existing `WaConversations` document (chosen).
2. **Sidecar collection** `WaConversationAiStates`, keyed by `conversation_id`.
3. **Redis-only** ephemeral state, rebuilt on miss.

**Decision.** Embed. The state is small (caps in ADR-5: ≤ 20 keys × 500 chars + ≤ 5 failed attempts), 1:1 with the conversation, and is read every time we already load `find_conversation_by_id` in dispatch (`dispatch.rs:227-231`). A sidecar would add a roundtrip per turn and a roundtrip per `conv_to_item` listing call. Redis is wrong because state must survive crashes and feed the staff UI long after the IA-active window.

**Consequences.**

- Document grows by at most ~10 KB (20 × 500 = 10 000 bytes plus per-key overhead). Well below the 16 MB BSON limit.
- No migration: `Option<_>` + `#[serde(default)]` makes legacy docs read as `None` without conversion.
- All updates go through a single trait method `update_conversation_ai_conv_state` (atomic `$set` / `$unset`), mirroring the existing `update_conversation_ai_state` pattern (`db/mongo/whatsapp.rs:512-549`).

**Rejected: sidecar.** Operational overhead (extra collection, extra index `{conversation_id: 1}` unique, extra cleanup on archive) without a payoff at current scale.

**Rejected: Redis-only.** Loses the staff-UI requirement and the "human takeover continues without restart" bullet from the proposal.

---

### ADR-2 — `ToolResult.state_patches: Vec<StatePatch>` (NOT `Option<StatePatch>`)

**Context.** The proposal's first draft modeled patches as `state_patch: Option<StatePatch>`. While reviewing scenario 16.3 in the spec mapping, it became clear that several tools must emit MORE than one patch in a single call:

- `lookup_customer` succeeds → emit `SetCollectedData{key:"client_id", ...}` AND `AddCompletedAction("lookup_customer")`.
- `check_coverage` matches → emit `SetCollectedData{key:"zone", ...}` AND `AddCompletedAction("check_coverage")`.
- `report_payment` succeeds → emit `SetCollectedData{key:"payment_id", ...}`, `AddCompletedAction("report_payment")`, and `SetCurrentStep("payment_reported")`.

**Decision.** The `ToolResult` field is `pub state_patches: Vec<StatePatch>` with `Vec::new()` as the default. Tools opt-in by calling `ok(...).with_patches(vec![...])`. The runner extends the per-turn accumulator with each tool's patches in execution order.

**Rejected: `Option<StatePatch>`** would require either tool collusion (ugly: tool stuffs both intents into one variant) or two tool invocations per logical action.

**Rejected: enum-with-multi-payload variants** like `SetDataAndComplete { ... }`. Combinatorial explosion.

---

### ADR-3 — Dispatch hook ordering

**Context.** State must be read ONCE before the runner starts the chain, and written ONCE after it ends. The chain itself can call `transfer_to_agent`, which makes a different agent take over within the same dispatch invocation. We need a single fold so a transfer mid-chain still produces a consistent final state.

**Decision.**

```
INSIDE try_lock_ai_dispatch window:
  1. read   → state_block = format_conversation_state(&conv.ai_conv_state)
  2. derive → state_patches.push(SetIntent { ... }) if conv has no current_intent
              and customer_explicit_intents is non-empty
  3. run    → run_turn(... conversation_state: state_block.as_deref() ...)
                returns RunnerOutput.state_patches accumulated through chain
  4. fold   → new_state = apply_state_patches(current, all_patches)
  5. transfer-reset → if any patch is SetCurrentStep("transferred_to_..."):
                       new_state.current_intent = None
                       new_state.intent_confidence = None
  6. write  → db.update_conversation_ai_conv_state(conv_id, Some(&new_state))
RELEASE lock
```

The whole window is already serialized by `try_lock_ai_dispatch` (`dispatch.rs:203`). No additional locking required for the new state column.

**Why read BEFORE `run_turn`:** the model needs the current state injected as `[conversation_state]` block. We cannot inject what we have not read yet.

**Why fold AFTER chain loop:** the chain can transfer between agents up to N times; each agent's tools may emit patches. Folding after the loop ensures we apply the union of patches from all agents in order, with later patches winning where they overlap (LWW semantics on `SetCollectedData` for the same key — see ADR-8).

**Why write inside the lock window:** prevents a parallel `POST .../ai-state/reset` from racing the dispatch tail and resurrecting state we are about to delete (or vice versa).

---

### ADR-4 — Intent classification source: dispatch derives from `customer_explicit_intents`

**Context.** Three options for setting `current_intent`:

1. The LLM emits a `SetIntent` patch via a dedicated `classify_intent` tool.
2. Each tool that "implies" an intent (e.g. `report_payment` implies `pagos`) emits `SetIntent` itself.
3. The dispatch derives intent deterministically from Phase 1's already-existing `extract_customer_explicit_intents(&recent)` keyword scanner.

**Decision.** Option 3. Tools do NOT emit `SetIntent`. The dispatch, **before folding patches**, prepends a synthetic `SetIntent` if and only if:

- `conv.ai_conv_state.is_none()` OR `conv.ai_conv_state.as_ref().unwrap().current_intent.is_none()`, AND
- `customer_explicit_intents` (already computed in `dispatch.rs:307`) is non-empty.

In that case it pushes `SetIntent { intent: customer_explicit_intents[0].clone(), confidence: 1.0 }` to the front of the patch list.

**Why prepend, not append:** so a same-turn `SetCurrentStep("transferred_to_X")` (which clears intent in the post-fold transfer-reset step, ADR-6) wins over the synthetic intent we just set. That preserves the rule "transfer wipes intent so the new agent reclassifies".

**Rejected: LLM-emitted intent.** Asking the model to classify intent is asking it to do work that a 30-line keyword table already does deterministically. Phase 1 already pays the keyword-scan cost in `dispatch.rs:307`. Reusing it costs zero.

**Rejected: per-tool implicit intent.** Couples intent to tool implementation. A new tool added later would need to remember to set intent — exactly the kind of footgun we want to avoid.

**Future:** when we move to LLM-scored confidence (out of scope for v1), we add a tool `set_intent` and the `derive_intent_from_keywords` step becomes a fallback only when the LLM produces no patch.

---

### ADR-5 — Caps: `collected_data` 20 keys × 500 chars; `failed_attempts` 5 entries FIFO

**Context.** Proposal §"Risks" mentions "unbounded growth". We need concrete numerical caps that match real conversation sizes. A single conversation rarely needs more than ~10 collected data points (name, dni, zone, plan, client_id, phone, etc.). 500 chars/value is generous (an address, a long note). 5 failed attempts is plenty for a debugging window — older failures are noise.

**Decision.**

| Field | Cap | Eviction policy |
|---|---|---|
| `collected_data` keys | 20 | When full and a new key arrives → reject the new one (preserve the older context). |
| `collected_data` value length | 500 chars | Truncate at 500, log warn. |
| `failed_attempts` | 5 entries | FIFO — drop oldest when pushing the 6th. |
| `pending_data` | 20 entries | Same as `collected_data` — reject new when full. |
| `completed_actions` | 50 entries | FIFO — drop oldest. Dedup by exact string. |

**Why reject-on-full for `collected_data` instead of FIFO:** an early-conversation `client_id` is more valuable to keep than a late-conversation `last_zone_mentioned`. FIFO would silently lose the high-value early data.

**Why FIFO on `failed_attempts`:** opposite case — recent failures are more diagnostically useful than ancient ones.

All caps live in `apply_state_patches` (ADR-8). Surfacing them as `pub const` in `src/modules/ai_agent/state.rs` makes them tunable without spelunking.

---

### ADR-6 — Reset semantics

Three reset triggers, three different policies:

| Trigger | `current_intent` | `intent_confidence` | `current_step` | `collected_data` | `pending_data` | `completed_actions` | `failed_attempts` |
|---|---|---|---|---|---|---|---|
| `transfer_to_agent` (same workspace) | clear | clear | set to `"transferred_to_<label>"` | preserve | preserve | preserve | preserve |
| `transfer_to_agent` (cross-workspace) | preserve | preserve | preserve | preserve | preserve | preserve | preserve |
| `create_ticket` success | preserve | preserve | set to `"ticket_created"` | preserve | preserve | append `"create_ticket"` | preserve |
| Conversation `reopen` | full wipe (`ai_conv_state = None`) | — | — | — | — | — | — |
| Manual endpoint `POST .../ai-state/reset` | full wipe (`ai_conv_state = None`) | — | — | — | — | — | — |

**Why partial-reset on transfer:** the receiving agent benefits from `collected_data["client_id"]`, `collected_data["zone"]`, etc. — the conversation context is not wasted. But intent must be re-derived because the new agent's classifier may disagree (e.g. recepcionista classified `internet`, then transferred to ventas, and now `internet` is no longer the active intent — it is now `contratacion`). Letting the new agent reclassify on its first turn ensures consistency.

**Why no reset on cross-workspace:** the conversation stays where it is; only the suggested-next-number changes. The original agent keeps owning the conv until the customer migrates.

**Why full wipe on reopen:** `reopen_conversation` already clears `ai_active_agent_id`, `ai_transfer_context`, `assigned_to` (`db/mongo/whatsapp.rs:467-485`). Logically the IA is starting over; its persisted brain state should start over too. We extend that `$unset` block with `aiConvState`.

**Implementation note for transfer-reset:** the dispatch checks the post-fold `state_patches` array (or equivalently, looks at `new_state.current_step` after applying patches). If `current_step.starts_with("transferred_to_")` → null out intent fields. This logic lives in dispatch (ADR-3 step 5), not inside `apply_state_patches` itself, because it is a cross-cutting policy.

---

### ADR-7 — Kill switch: `Config.enable_ai_conversation_state` (env-only, default `true`)

**Context.** Phase 1 introduced `enable_ai_guardrails` (`config.rs:79`, default `true`, env-only). Phase 2 mirrors that exactly.

**Decision.** Add `pub enable_ai_conversation_state: bool` to `Config`. Read from `ENABLE_AI_CONVERSATION_STATE` env var with the same parsing logic (`false` / `0` / `no` → `false`; everything else → `true`; missing → `true`).

When `false`:

- Dispatch SKIPS the read step (`format_conversation_state` not called; `conversation_state: None` passed to `run_turn`).
- Dispatch SKIPS the write step (`update_conversation_ai_conv_state` not called).
- Tools STILL produce `state_patches` in their `ToolResult` — patches are computed but discarded by the dispatch fold step, which becomes a no-op when the flag is off.
- Tools STILL get the existing Phase 1 features (zones, intents, media_ids).
- Existing `ai_conv_state` documents in MongoDB are NOT erased — the flag is purely a runtime gate.

Tools producing patches even when the flag is off costs ~5 ns of allocation per call and avoids a per-tool conditional. Worth it for simplicity.

**Rejected: serde-driven config field.** `Config` already uses pure env loading (`config.rs:160-166` style); we keep parity.

**Rejected: per-agent toggle.** Out of scope. If a single agent breaks because of state, the global flag is the emergency lever; a permanent per-agent toggle is a future optimization.

---

### ADR-8 — BSON representation

| Rust type | BSON shape | Rationale |
|---|---|---|
| `BTreeMap<String, String>` for `collected_data` | BSON `document` (object) | `BTreeMap` gives stable ordering Rust-side (handy for tests, logging, deterministic prompt formatting). Serde maps it to a BSON document — keys become field names in MongoDB. |
| `Vec<FailedAttempt>` | BSON `array` of subdocs | Standard. |
| `Vec<String>` for `pending_data` / `completed_actions` | BSON `array` of strings | Standard. |
| `Option<String>` for `current_intent`, `current_step` | string or absent | `skip_serializing_if = "Option::is_none"` keeps documents lean. |
| `Option<f32>` for `intent_confidence` | double or absent | Same. |
| `DateTime<Utc>` for `updated_at` | BSON datetime | Always written — `updated_at` is the freshness indicator. |

**Patch application semantics:**

- `SetIntent { intent, confidence }` → overwrite `current_intent` and `intent_confidence` (LWW within the patch list).
- `SetCollectedData { key, value }` → `collected_data.insert(key, truncate(value, 500))` IF len < 20 keys OR key already exists. Otherwise drop and `tracing::warn!`.
- `AddCompletedAction(name)` → push to `completed_actions` if not already present (dedup); FIFO trim to 50.
- `SetCurrentStep(s)` → overwrite `current_step` (LWW).
- `AddFailedAttempt { tool, error }` → push, FIFO trim to 5.

`updated_at = Utc::now()` is set unconditionally on every `apply_state_patches` call.

---

## 2. Code-level design

### 2.1 Models (`src/models/whatsapp.rs`)

New types defined alongside existing WA types. Note the explicit `BTreeMap` (`std::collections::BTreeMap`) — `HashMap` would still serialize but we want deterministic ordering for prompt formatting and test assertions.

```rust
use std::collections::BTreeMap;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FailedAttempt {
    pub tool: String,
    pub error: String,
    /// UTC. Wire as ISO-8601 in `ToSchema` (utoipa derives via chrono feature).
    pub at: DateTime<Utc>,
}

/// Persistent per-conversation IA brain state. Embedded in `WaConversation`.
/// Read every dispatch turn, written once at the end of the chain.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct WaConversationAiState {
    /// Active customer intent group key (matches `INTENT_KEYWORDS` keys from
    /// `guardrails.rs`, e.g. "internet", "pagos", "contratacion"). `None` until
    /// dispatch derives it from explicit keywords or until reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_intent: Option<String>,

    /// 0.0..=1.0. v1 always sets 1.0 (keyword-derived). Reserved for future
    /// LLM-scored values without breaking the schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_confidence: Option<f32>,

    /// Bounded key→value freeform context. Caps: 20 keys × 500 chars/value.
    /// Examples: `client_id`, `zone`, `payment_reference`, `plan_name`.
    #[serde(default)]
    pub collected_data: BTreeMap<String, String>,

    /// Free-form short list of asks the model has open. Cap 20.
    #[serde(default)]
    pub pending_data: Vec<String>,

    /// Successful tool calls + actions, dedup'd, FIFO 50.
    #[serde(default)]
    pub completed_actions: Vec<String>,

    /// Free-form step marker. Not parsed by the back; format is the prompt's
    /// concern. Examples: `"transferred_to_ventas"`, `"ticket_created"`,
    /// `"awaiting_payment_proof"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_step: Option<String>,

    /// FIFO ring buffer (cap 5) of recent tool failures — diagnostic only.
    #[serde(default)]
    pub failed_attempts: Vec<FailedAttempt>,

    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StatePatch {
    SetIntent { intent: String, confidence: f32 },
    SetCollectedData { key: String, value: String },
    AddCompletedAction(String),
    SetCurrentStep(String),
    AddFailedAttempt { tool: String, error: String },
}
```

Embed in `WaConversation` (after the `ai_last_processed_at` field, line ~96):

```rust
/// Persistent IA brain state for this conversation. Read-once at dispatch
/// start, written-once at chain end. `None` for legacy convs and for fresh
/// convs that have not yet had any AI turn. See `WaConversationAiState`.
#[serde(rename = "aiConvState", skip_serializing_if = "Option::is_none", default)]
pub ai_conv_state: Option<WaConversationAiState>,
```

Embed in `ConversationItem` (after `ai_last_processed_at`, line ~693):

```rust
/// IA brain state — same shape as in the underlying `WaConversation`. The
/// staff UI renders it in a sidebar so a human takeover can see what data
/// the IA already collected.
#[serde(skip_serializing_if = "Option::is_none")]
pub ai_conv_state: Option<WaConversationAiState>,
```

The BSON field name `aiConvState` keeps the convention with `aiActiveAgentId` (camelCase rename). Internal Rust field name stays snake_case.

### 2.2 `ToolResult` extension (`src/modules/ai_agent/tools.rs`)

```rust
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub success: bool,
    pub data: Value,
    pub error: Option<String>,
    pub duration_ms: u32,
    /// Patches the dispatch will fold into `WaConversation.ai_conv_state`
    /// after the chain loop. Empty by default — opt-in per tool.
    pub state_patches: Vec<StatePatch>,
}

impl ToolResult {
    fn ok(data: Value, started: Instant) -> Self {
        ToolResult {
            success: true,
            data,
            error: None,
            duration_ms: started.elapsed().as_millis() as u32,
            state_patches: Vec::new(),
        }
    }

    fn err(msg: impl Into<String>, started: Instant) -> Self {
        let m = msg.into();
        ToolResult {
            success: false,
            data: json!({ "error": &m }),
            error: Some(m),
            duration_ms: started.elapsed().as_millis() as u32,
            state_patches: Vec::new(),
        }
    }

    /// Builder-style attach. Tools call this on the `ok(...)` chain when they
    /// have something worth persisting.
    fn with_patches(mut self, patches: Vec<StatePatch>) -> Self {
        self.state_patches = patches;
        self
    }
}
```

### 2.3 Per-tool patch wiring

| Tool | Trigger | Patches emitted |
|---|---|---|
| `lookup_customer` | success AND `items.len() >= 1` AND first item has `_id` | `SetCollectedData { key: "client_id", value: items[0].id.to_hex() }` + `AddCompletedAction("lookup_customer")` |
| `lookup_customer` | success but no items | `AddCompletedAction("lookup_customer")` only |
| `get_invoices` | success | `AddCompletedAction("get_invoices")` |
| `list_plans` | success | `AddCompletedAction("list_plans")` |
| `check_coverage` | success AND `covered == true` | `SetCollectedData { key: "zone", value: matched_zone_or_queried }` + `AddCompletedAction("check_coverage")` |
| `check_coverage` | success AND `covered == false` | `AddCompletedAction("check_coverage")` only |
| `calculate_amount_bs` | success | `AddCompletedAction("calculate_amount_bs")` |
| `report_payment` | success AND `mode == "live"` | `SetCollectedData { key: "last_payment_id", value: payment_id }` + `AddCompletedAction("report_payment")` + `SetCurrentStep("payment_reported")` |
| `report_payment` | success AND `already_registered == true` | `AddCompletedAction("report_payment")` + `SetCurrentStep("payment_already_registered")` |
| `request_human` | success AND live | `AddCompletedAction("request_human")` + `SetCurrentStep("transferred_to_human")` |
| `create_ticket` | success AND live | `AddCompletedAction("create_ticket")` + `SetCurrentStep("ticket_created")` + `SetCollectedData { key: "ticket_id", value: ticket_id }` |
| `transfer_to_agent` | success AND `mode == "live"` | `AddCompletedAction("transfer_to_agent")` + `SetCurrentStep(format!("transferred_to_{}", target.label.to_snake_case()))` |
| `transfer_to_agent` | success AND `mode == "cross_workspace"` | `AddCompletedAction("transfer_to_agent")` + `SetCurrentStep("cross_workspace_redirect")` |
| Any tool | failure | `AddFailedAttempt { tool: name, error: error_string }` (emitted from a helper at the runner level — see ADR clarification below) |

**Failure-patch placement.** Per-tool `err(...)` calls do NOT call `with_patches`; instead the runner detects `result.success == false` and synthesizes the `AddFailedAttempt` patch right after the `execute_tool` call (centralized — adding a new tool does not require remembering to add the failed-attempt boilerplate). This keeps `tools.rs` patch-emission focused on success cases.

**Snake-case helper for `current_step` in transfer.** A small helper `to_snake_case_safe(label)` in `src/modules/ai_agent/state.rs` lowercases, replaces non-alphanumeric runs with `_`, and trims leading/trailing underscores — defensive against agent labels like "Ventas y Contrataciones" → `"transferred_to_ventas_y_contrataciones"`. The dispatch transfer-reset check uses prefix matching `starts_with("transferred_to_")`, so the suffix is opaque.

### 2.4 `RunnerOutput` extension (`src/modules/ai_agent/runner.rs`)

```rust
#[derive(Debug, Clone)]
pub struct RunnerOutput {
    // ── existing fields, lines 113-135 ──
    pub response_text: Option<String>,
    pub tool_calls: Vec<AiToolCallLog>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub thinking_tokens: u32,
    pub total_tokens: u32,
    pub cost_usd_estimate: f64,
    pub latency_ms: u32,
    pub escalated: bool,
    pub escalation_reason: Option<String>,
    pub finish_reason: Option<String>,
    pub transfer: Option<TransferInfo>,

    /// Accumulated state patches from every tool executed during this turn,
    /// in execution order. Dispatch folds these into `ai_conv_state` after
    /// the chain loop. Empty when no tool ran or all tools opted out.
    pub state_patches: Vec<StatePatch>,
}
```

Inside `run_turn`'s tool-execution loop (around `runner.rs:517`):

```rust
let result = execute_tool(&call.name, call.args.clone(), tool_ctx).await;

// ── existing logging / tool_call_logs.push() / escalation flag ──

// Accumulate patches from successful tools.
if result.success {
    state_patches_acc.extend(result.state_patches.iter().cloned());
} else {
    // Centralized failure patch (see §2.3).
    state_patches_acc.push(StatePatch::AddFailedAttempt {
        tool: call.name.clone(),
        error: result.error.clone().unwrap_or_else(|| "unknown_error".into()),
    });
}
```

`state_patches_acc: Vec<StatePatch>` is initialized at the top of `run_turn` and threaded into the final `RunnerOutput`.

### 2.5 `build_system_instruction` extension

Add `conversation_state: Option<&str>` as the 8th `Option<&str>` parameter, immediately after `turn_state`. Inject as a labeled block in the same chunk-list pattern (`runner.rs:262-266`):

```rust
fn build_system_instruction(
    agent: &AiAgent,
    faqs_inline: Option<&str>,
    customer_context: Option<&str>,
    transfer_context: Option<&str>,
    first_turn_note: Option<&str>,
    agent_state: Option<&str>,
    turn_state: Option<&str>,
    conversation_state: Option<&str>,   // NEW
    vars: Option<&PromptVariables>,
) -> SystemInstruction {
    // ... existing chunks up to [turn_state] ...

    if let Some(ts) = turn_state {
        if !ts.trim().is_empty() {
            chunks.push(format!("[turn_state]\n{}", ts.trim()));
        }
    }

    // NEW: persistent conversation state, between turn_state and faqs.
    if let Some(cs) = conversation_state {
        if !cs.trim().is_empty() {
            chunks.push(format!("[conversation_state]\n{}", cs.trim()));
        }
    }

    if let Some(faqs) = faqs_inline {
        // ... unchanged ...
    }
    // ...
}
```

`run_turn` signature gains the matching parameter (between `turn_state` and `prompt_vars`):

```rust
pub async fn run_turn(
    ...,
    turn_state: Option<&str>,
    conversation_state: Option<&str>,   // NEW
    prompt_vars: Option<&PromptVariables>,
    tool_ctx: &ToolContext,
) -> Result<RunnerOutput, ApiError> {
    let system_instruction = build_system_instruction(
        agent, faqs_inline, customer_context, transfer_context,
        first_turn_note, agent_state, turn_state,
        conversation_state,                 // NEW
        prompt_vars,
    );
    // ... unchanged ...
}
```

`sandbox.rs` mirrors the signature — passes `None` for `conversation_state` (sandbox runs are stateless by definition).

### 2.6 Dispatch hooks (`src/modules/ai_agent/dispatch.rs`)

Four edits, all inside `run_dispatch` (within the `try_lock_ai_dispatch` window):

**(a) Read state — just before the chain loop starts (~line 519, near `initial_transfer_context_owned`):**

```rust
// Format the persisted brain state for injection. None when:
//   - feature flag is off
//   - conv has never had IA state written (legacy or fresh)
let conversation_state_owned: Option<String> = if state.config.enable_ai_conversation_state {
    conv.ai_conv_state.as_ref().map(format_conversation_state)
} else {
    None
};
```

`format_conversation_state` lives in `src/modules/ai_agent/state.rs` (§2.8).

**(b) Pass to `run_turn` (~line 663, alongside `turn_state_owned.as_deref()`):**

```rust
let output = match run_turn(
    &state.reqwest_client,
    &active_agent,
    &active_api_key,
    relay,
    endpoint_override,
    &history,
    &effective_user_message,
    &user_media,
    active_faqs_inline.as_deref(),
    customer_context.as_deref(),
    active_transfer_context.as_deref(),
    ftn_for_iter,
    agent_state_owned.as_deref(),
    turn_state_owned.as_deref(),
    conversation_state_owned.as_deref(),    // NEW
    Some(&active_prompt_vars),
    &tool_ctx,
).await { ... };
```

**(c) Accumulate patches across the chain.** The chain loop iterates multiple `run_turn` calls in cross-agent transfers. Each iteration produces a `RunnerOutput.state_patches`. Initialize a `Vec<StatePatch>` outside the loop and `extend` from each output:

```rust
let mut all_state_patches: Vec<StatePatch> = Vec::new();
// ── chain loop ──
//   ... existing ...
//   all_state_patches.extend(output.state_patches.iter().cloned());
// ── /chain loop ──
```

**(d) Derive intent + fold + transfer-reset + write — after the chain loop ends, before the `transfer_context` cleanup block (~line 999):**

```rust
if state.config.enable_ai_conversation_state {
    // Synthetic intent derivation (ADR-4): only if the conversation has no
    // active intent yet AND Phase 1 detected explicit keywords.
    let needs_intent = conv.ai_conv_state
        .as_ref()
        .map(|s| s.current_intent.is_none())
        .unwrap_or(true);
    if needs_intent {
        if let Some(first_intent) = customer_explicit_intents.first() {
            all_state_patches.insert(
                0,
                StatePatch::SetIntent {
                    intent: first_intent.clone(),
                    confidence: 1.0,
                },
            );
        }
    }

    if !all_state_patches.is_empty() || conv.ai_conv_state.is_none() {
        let current = conv.ai_conv_state.clone().unwrap_or_default();
        let mut new_state = apply_state_patches(current, &all_state_patches);

        // Transfer reset (ADR-6): clear intent when current_step marks transfer.
        if let Some(step) = new_state.current_step.as_deref() {
            if step.starts_with("transferred_to_") {
                new_state.current_intent = None;
                new_state.intent_confidence = None;
            }
        }

        if let Err(e) = state
            .db
            .update_conversation_ai_conv_state(&inbound.conversation_id, Some(&new_state))
            .await
        {
            tracing::warn!(
                "[ai_agent.dispatch] update_conversation_ai_conv_state failed (conv={}): {}",
                inbound.conversation_id.to_hex(),
                e
            );
        }
    }
}
```

The early-out `if !all_state_patches.is_empty() || conv.ai_conv_state.is_none()` prevents writing for no-op turns where the IA neither called any patching tool nor needs a fresh-default state.

### 2.7 DB trait + impl

**`src/db/mod.rs`** — extend `WhatsAppRepository`:

```rust
/// Replaces the entire `aiConvState` field of the conversation. `None` =
/// `$unset` (used by reset endpoint and reopen flow). `Some(state)` = `$set`
/// with the full document. Atomic single-update.
async fn update_conversation_ai_conv_state(
    &self,
    conv_id: &ObjectId,
    state: Option<&WaConversationAiState>,
) -> Result<(), String>;
```

**`src/db/mongo/whatsapp.rs`** — implementation alongside `update_conversation_ai_state`:

```rust
async fn update_conversation_ai_conv_state(
    &self,
    conv_id: &ObjectId,
    state: Option<&WaConversationAiState>,
) -> Result<(), String> {
    let update = match state {
        Some(s) => {
            let bson_state = mongodb::bson::to_bson(s)
                .map_err(|e| format!("serialize ai_conv_state: {}", e))?;
            doc! { "$set": { "aiConvState": bson_state } }
        }
        None => doc! { "$unset": { "aiConvState": "" } },
    };
    self.wa_conversations()
        .update_one(doc! { "_id": conv_id }, update)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
```

**Reopen extension.** Edit `reopen_conversation` (`db/mongo/whatsapp.rs:467`) to also `$unset: { aiConvState: "" }` in the same atomic update — keeps the reopen wipe atomic. Same for `close_conversation` (`db/mongo/whatsapp.rs:449`): the proposal does NOT mandate close-clears-state, but logically a closed conversation should release the embed; we add it for symmetry. A re-opened conv resumes empty.

### 2.8 Patch application logic — new file `src/modules/ai_agent/state.rs`

```rust
//! Persistent IA conversation state — patch fold + prompt formatting.
//!
//! The dispatch reads `WaConversation.ai_conv_state`, formats it for the
//! `[conversation_state]` block, runs the chain, and at the end folds all
//! `StatePatch` emitted by tools into a new state that is written back.

use std::collections::BTreeMap;

use chrono::Utc;

use crate::models::whatsapp::{FailedAttempt, StatePatch, WaConversationAiState};

/// Caps — see ADR-5. Tunable in one place.
pub const COLLECTED_DATA_KEY_CAP: usize = 20;
pub const COLLECTED_DATA_VALUE_CHAR_CAP: usize = 500;
pub const PENDING_DATA_CAP: usize = 20;
pub const COMPLETED_ACTIONS_CAP: usize = 50;
pub const FAILED_ATTEMPTS_CAP: usize = 5;

/// Pure: applies a list of patches in order. LWW semantics on overwriting
/// patches. Always sets `updated_at = Utc::now()`.
pub fn apply_state_patches(
    mut state: WaConversationAiState,
    patches: &[StatePatch],
) -> WaConversationAiState {
    for p in patches {
        match p {
            StatePatch::SetIntent { intent, confidence } => {
                state.current_intent = Some(intent.clone());
                state.intent_confidence = Some(*confidence);
            }
            StatePatch::SetCollectedData { key, value } => {
                let truncated = truncate_chars(value, COLLECTED_DATA_VALUE_CHAR_CAP);
                if state.collected_data.contains_key(key)
                    || state.collected_data.len() < COLLECTED_DATA_KEY_CAP
                {
                    state.collected_data.insert(key.clone(), truncated);
                } else {
                    tracing::warn!(
                        "[ai_agent.state] collected_data cap reached ({}); dropping key='{}'",
                        COLLECTED_DATA_KEY_CAP, key
                    );
                }
            }
            StatePatch::AddCompletedAction(name) => {
                if !state.completed_actions.iter().any(|a| a == name) {
                    state.completed_actions.push(name.clone());
                    while state.completed_actions.len() > COMPLETED_ACTIONS_CAP {
                        state.completed_actions.remove(0);
                    }
                }
            }
            StatePatch::SetCurrentStep(s) => {
                state.current_step = Some(s.clone());
            }
            StatePatch::AddFailedAttempt { tool, error } => {
                state.failed_attempts.push(FailedAttempt {
                    tool: tool.clone(),
                    error: error.clone(),
                    at: Utc::now(),
                });
                while state.failed_attempts.len() > FAILED_ATTEMPTS_CAP {
                    state.failed_attempts.remove(0);
                }
            }
        }
    }
    state.updated_at = Utc::now();
    state
}

/// Format the state into the `[conversation_state]` block body. The
/// `runner::build_system_instruction` adds the bracket header.
///
/// Format is line-based, label: value, mirroring `[turn_state]`. Empty
/// fields are skipped to keep the prompt lean.
pub fn format_conversation_state(state: &WaConversationAiState) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(intent) = &state.current_intent {
        lines.push(format!("current_intent: {}", intent));
    }
    if let Some(conf) = state.intent_confidence {
        lines.push(format!("intent_confidence: {:.2}", conf));
    }
    if !state.collected_data.is_empty() {
        let pairs: Vec<String> = state
            .collected_data
            .iter()
            .map(|(k, v)| format!("  {}: {}", k, v))
            .collect();
        lines.push(format!("collected_data:\n{}", pairs.join("\n")));
    }
    if !state.pending_data.is_empty() {
        lines.push(format!("pending_data: {}", state.pending_data.join(", ")));
    }
    if !state.completed_actions.is_empty() {
        lines.push(format!("completed_actions: {}", state.completed_actions.join(", ")));
    }
    if let Some(step) = &state.current_step {
        lines.push(format!("current_step: {}", step));
    }
    if !state.failed_attempts.is_empty() {
        let recent: Vec<String> = state
            .failed_attempts
            .iter()
            .map(|f| format!("  {} → {}", f.tool, f.error))
            .collect();
        lines.push(format!("failed_attempts:\n{}", recent.join("\n")));
    }
    lines.join("\n")
}

/// Snake-case-ish slug for `current_step` values like `transferred_to_<label>`.
pub fn slugify_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_is_underscore = true; // skip leading underscores
    for c in label.chars() {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_is_underscore = false;
        } else if !prev_is_underscore {
            out.push('_');
            prev_is_underscore = true;
        }
    }
    while out.ends_with('_') { out.pop(); }
    out
}

#[inline]
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
```

`src/modules/ai_agent/mod.rs` adds `pub mod state;`.

### 2.9 Reset semantics implementation

**Reopen flow** (`db/mongo/whatsapp.rs:467-485`): extend the `$unset` block:

```rust
"$unset": {
    "assigned_to": "",
    "ai_active_agent_id": "",
    "ai_transfer_context": "",
    "aiConvState": "",
}
```

This is atomic with the rest of the reopen and matches ADR-6.

**Transfer (same-workspace)** is NOT a DB-level reset — the dispatch loop's transfer-reset code (§2.6 step d) clears `current_intent` / `intent_confidence` on the in-memory `new_state` before writing. `current_step = "transferred_to_X"` IS preserved.

**Cross-workspace transfer** writes nothing special — the `state_patches` from the transfer tool include `SetCurrentStep("cross_workspace_redirect")` and `AddCompletedAction("transfer_to_agent")`, no intent changes.

**Manual reset endpoint** — see §2.10.

### 2.10 Reset endpoint

`POST /v1/auth-user/whatsapp/conversations/:id/ai-state/reset`

Routing under `user_protected` group (already JWT-validated by `user_jwt_auth_middleware`). Inside the handler, we add a soft authorization check: `claims.b_can_chat == true` to gate to staff with WhatsApp permissions. (Same gate the rest of `whatsapp/handler.rs` uses — no new role concept.)

Handler skeleton:

```rust
#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/conversations/{id}/ai-state/reset",
    tag = "WhatsApp",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex de la conversación")),
    responses(
        (status = 200, description = "AI state reseteado", body = ResetAiStateResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Sin permisos de WhatsApp"),
        (status = 404, description = "Conversación no encontrada"),
        (status = 409, description = "Otro dispatch IA está corriendo — reintentar"),
    )
)]
pub async fn reset_ai_state_handler(
    State(state): State<Arc<AppState>>,
    Path(conv_hex): Path<String>,
    Extension(claims): Extension<UserClaims>,
) -> Result<Json<ResetAiStateResponse>, ApiError> {
    if !claims.b_can_chat {
        return Err(ApiError::new(403, "wa_permission_required"));
    }

    let conv_id = ObjectId::parse_str(&conv_hex)
        .map_err(|_| ApiError::new(400, "invalid_conversation_id"))?;

    // Existence check (and gives us a 404 if missing).
    state.db.find_conversation_by_id(&conv_id).await
        .map_err(|e| ApiError::new(500, &format!("db_error:{}", e)))?
        .ok_or_else(|| ApiError::new(404, "conversation_not_found"))?;

    // Acquire dispatch lock to prevent racing with an in-flight turn.
    if !state.redis.try_lock_ai_dispatch(&conv_hex).await {
        return Err(ApiError::new(409, "ai_dispatch_in_progress"));
    }

    let result = state.db.update_conversation_ai_conv_state(&conv_id, None).await;

    state.redis.release_ai_dispatch_lock(&conv_hex).await;

    result.map_err(|e| ApiError::new(500, &format!("db_error:{}", e)))?;

    Ok(Json(ResetAiStateResponse {
        ok: true,
        conversation_id: conv_hex,
    }))
}
```

Response model:

```rust
#[derive(Debug, Serialize, ToSchema)]
pub struct ResetAiStateResponse {
    pub ok: bool,
    pub conversation_id: String,
}
```

Routing — `src/axum_router.rs`, append under the WhatsApp `user_protected` block:

```rust
.route(
    "/v1/auth-user/whatsapp/conversations/:id/ai-state/reset",
    post(crate::modules::whatsapp::handler::reset_ai_state_handler),
)
```

OpenAPI — `src/openapi.rs`, register path and schemas: `reset_ai_state_handler`, `ResetAiStateResponse`, `WaConversationAiState`, `FailedAttempt`.

---

## 3. Sequence diagram

```
WhatsApp inbound (Meta webhook)
  ↓
modules::whatsapp::handler::receive_webhook
  ↓
modules::ai_agent::dispatch::handle_inbound_message
  ↓ (debounce window)
try_lock_ai_dispatch (Redis SET NX)        ◀── serializes per-conv
  │
  ├─► load conv from DB (already includes ai_conv_state)
  │
  ├─► load `recent` messages (RECENT_WINDOW)
  │
  ├─► Phase 1 guardrails extraction:
  │     customer_explicit_zones
  │     customer_explicit_intents
  │     recent_media_ids
  │
  ├─► [NEW] conversation_state_block =
  │        if config.enable_ai_conversation_state:
  │            conv.ai_conv_state.as_ref().map(format_conversation_state)
  │        else: None
  │
  ├─► build ToolContext (zones, media_ids, allowed targets, agent snapshot, ...)
  │
  ├─► Chain loop (cross-agent transfers, MAX 3 hops):
  │     │
  │     ├─► run_turn(... turn_state, conversation_state ...) returns RunnerOutput
  │     │     │
  │     │     ├─► build_system_instruction injects [conversation_state] block
  │     │     │   between [turn_state] and [faqs]
  │     │     │
  │     │     └─► tool exec loop:
  │     │           result = execute_tool(...)
  │     │           if result.success:
  │     │               state_patches_acc.extend(result.state_patches)
  │     │           else:
  │     │               state_patches_acc.push(AddFailedAttempt { ... })
  │     │
  │     ├─► all_state_patches.extend(output.state_patches)
  │     │
  │     └─► (on transfer_to_agent same-workspace) load target agent, loop again
  │
  ├─► [NEW] derive intent (ADR-4):
  │     if conv has no current_intent AND customer_explicit_intents non-empty:
  │         all_state_patches.insert(0, SetIntent { intent: first, confidence: 1.0 })
  │
  ├─► [NEW] fold + transfer-reset:
  │     current = conv.ai_conv_state.clone().unwrap_or_default()
  │     new_state = apply_state_patches(current, &all_state_patches)
  │     if new_state.current_step.starts_with("transferred_to_"):
  │         new_state.current_intent = None
  │         new_state.intent_confidence = None
  │
  ├─► [NEW] db.update_conversation_ai_conv_state(conv_id, Some(&new_state))
  │
  ├─► (existing) clean up ai_transfer_context
  │
  ├─► (existing) decide response_text + send to WhatsApp
  │
release_ai_dispatch_lock
```

Manual reset endpoint flow:

```
POST /v1/auth-user/whatsapp/conversations/{id}/ai-state/reset
  ↓ user_jwt_auth_middleware (existing)
  ↓ handler.reset_ai_state_handler
  ├─► claims.b_can_chat check (403 otherwise)
  ├─► find_conversation_by_id (404 otherwise)
  ├─► try_lock_ai_dispatch (409 if held)
  ├─► db.update_conversation_ai_conv_state(conv_id, None)   ◀── $unset aiConvState
  └─► release_ai_dispatch_lock
```

---

## 4. Open questions

1. **Multi-intent representation.** Should `current_intent` be a single key or a comma-separated list (e.g. `"internet,pagos"`)? Current decision: single string. The `customer_explicit_intents` in the `[turn_state]` HUD already exposes the multi-intent view to the LLM; `current_intent` represents the "primary" intent for routing decisions. If a future scenario surfaces (mixed-intent escalation logic, filters by intent), revisit.
2. **Does `failed_attempts` track success-then-fail oscillations?** No. Only failures. The 5-entry FIFO is for forensics, not state reconstruction.
3. **Should `intent_confidence` ever be `None` while `current_intent` is `Some`?** Currently no — both are set together by `SetIntent`. Reserved for the future LLM-classifier that may emit intent without a numerical score, in which case we will allow the asymmetry.
4. **What happens if a tool patch references a key like `client_id` AFTER the conv was reset mid-turn by the manual endpoint?** Cannot happen — reset endpoint acquires the same `try_lock_ai_dispatch` as the dispatch. Reset is queued after the in-flight turn finishes (or fails to acquire if dispatch holds it, returning 409 to the caller).

---

## 5. Testing approach

**Build:** `cargo check` zero new warnings.

**Manual smoke tests (post-deploy, dev environment):**

1. **Intent derivation.** Send `"quiero info del internet"` to a fresh conversation. After dispatch:
   - `db.WaConversations.findOne({_id})` shows `aiConvState.current_intent === "internet"` (or whichever Phase 1 group key matches first), `intent_confidence === 1.0`, `current_step === undefined`, `completed_actions === []` (until a tool runs).
2. **`lookup_customer` patch.** Continue the conversation, the IA calls `lookup_customer` on a known phone:
   - `aiConvState.collected_data.client_id === "<oid hex>"`.
   - `completed_actions` contains `"lookup_customer"`.
3. **`check_coverage` patch.** Customer says `"vivo en Valencia"`, IA calls `check_coverage("Valencia")`:
   - `aiConvState.collected_data.zone === "Valencia"` (or matched_zone if different).
   - `completed_actions` ⊇ `["lookup_customer", "check_coverage"]`.
4. **Transfer to ventas.** IA calls `transfer_to_agent` with the ventas agent:
   - `aiConvState.current_step === "transferred_to_ventas"` (or slugified label).
   - `aiConvState.current_intent === null` (cleared by transfer-reset).
   - `aiConvState.collected_data` preserved.
5. **Reset endpoint.** `POST /v1/auth-user/whatsapp/conversations/:id/ai-state/reset` with a staff JWT (`bCanChat=true`):
   - Response `{ok: true, conversation_id}`.
   - `db.WaConversations.findOne({_id})` → `aiConvState` is absent.
6. **Reset 409.** Hammer the reset endpoint while a dispatch is in flight (script: send rapid inbounds, immediately POST reset):
   - Some calls return 409 `ai_dispatch_in_progress` — the dispatch lock holds.
7. **Kill switch.** Set `ENABLE_AI_CONVERSATION_STATE=false`, restart, send messages:
   - No `[conversation_state]` block in the system_instruction debug log.
   - `aiConvState` field never created on new convs, never updated on existing convs.
   - Existing `aiConvState` documents remain untouched (NOT erased).
8. **Reopen flow.** Close a conv with state, reopen via `POST /reopen`:
   - `aiConvState` is unset post-reopen.
9. **Caps.**
   - Force-emit 21 distinct `SetCollectedData` keys via the LLM (impossible to script naturally; can be mock-tested in a unit test with a `Vec<StatePatch>` builder).
   - Force-emit 6 `AddFailedAttempt` — only the last 5 survive.

---

## 6. Affected files

| File | Change | Est. LOC |
|---|---|---|
| `src/models/whatsapp.rs` | new `WaConversationAiState`, `FailedAttempt`, `StatePatch` + embed in `WaConversation` and `ConversationItem` | +50 |
| `src/modules/ai_agent/tools.rs` | `ToolResult.state_patches` + `with_patches` helper + per-tool `.with_patches(...)` calls on success | +60 |
| `src/modules/ai_agent/runner.rs` | `conversation_state` parameter on `build_system_instruction` + `run_turn`; `RunnerOutput.state_patches`; per-tool patch accumulation incl. centralized `AddFailedAttempt` | +25 |
| `src/modules/ai_agent/sandbox.rs` | mirror new signature with `conversation_state: None` | +3 |
| `src/modules/ai_agent/dispatch.rs` | read state → format → pass; intent derivation; chain-loop accumulator; fold + transfer-reset + write | +55 |
| `src/modules/ai_agent/state.rs` | NEW: `apply_state_patches`, `format_conversation_state`, `slugify_label`, caps consts | +120 |
| `src/modules/ai_agent/mod.rs` | `pub mod state;` | +1 |
| `src/db/mod.rs` | `update_conversation_ai_conv_state` trait method | +10 |
| `src/db/mongo/whatsapp.rs` | trait impl + extend `reopen_conversation` and `close_conversation` `$unset` blocks | +30 |
| `src/modules/whatsapp/handler.rs` | `conv_to_item` propagation + `reset_ai_state_handler` + `ResetAiStateResponse` | +50 |
| `src/axum_router.rs` | register reset endpoint | +5 |
| `src/openapi.rs` | document new endpoint + schemas | +12 |
| `src/config.rs` | `enable_ai_conversation_state` env-only field | +6 |
| **Total** | | **~430 LOC** |

(Slightly above the original ~360 LOC estimate due to the `state.rs` module growing with format helpers and slugify, plus full handler skeleton vs. stub.)

---

## 7. Notes for the tasks phase

The tasks phase should slice this into ~10–14 atomic units roughly in this order (each compiles independently):

1. Models (struct + embed) — compiles standalone since `Default` is derived.
2. `state.rs` module — pure functions, no external deps beyond models.
3. `Config` flag.
4. DB trait + impl.
5. `ToolResult.state_patches` + `with_patches`.
6. `RunnerOutput.state_patches` + accumulator + signature changes (compiles only after caller updates).
7. `build_system_instruction` + `run_turn` signature with `conversation_state`.
8. Sandbox signature mirror.
9. Per-tool `.with_patches(...)` wiring (one tool per task is overkill — group by category: lookup/info-tools, action-tools).
10. Dispatch read-state hook.
11. Dispatch fold + intent-derive + transfer-reset + write hook.
12. Reopen `$unset aiConvState` extension.
13. Reset endpoint handler + route + OpenAPI registration.
14. `ConversationItem.ai_conv_state` propagation in `conv_to_item`.

Tests / smoke checks per §5 belong to the apply / verify phases, not tasks.
