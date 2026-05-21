# Design: AI Agent — `calculate_amount_bs` tool

> Phase: design (HOW at architectural level)
> Related: [proposal.md](./proposal.md)

## 1. Architecture decisions

### ADR-1: Place `find_tax_by_target` in the existing `ProfileRepository` trait
- **Decision**: Add `async fn find_tax_by_target(&self, target: &str) -> Result<Option<Tax>, String>` to `ProfileRepository` (`src/db/mod.rs`), implemented in `src/db/mongo/profile.rs`.
- **Rationale**: `find_tax_by_id` already lives in `ProfileRepository` and reads from the same out-of-database collection (`BCV.IVA`). Splitting "tax lookups" into a new trait or a new module would fragment a 2-method concern across the codebase without payoff. The trait is already a grab-bag of "client + tax + customer" concerns; one more 1-liner read is consistent.
- **Rejected alternatives**:
  - **New `TaxRepository` trait**: pure-overhead refactor for one extra method. Defer until a third tax-related method shows up.
  - **Reuse `find_tax_by_id` and synthesize a fake `tax_id`**: brittle (the doc's `_id` is unknown to the caller) and dishonest about the lookup key.
  - **Inline the Mongo query inside `tools.rs`**: would couple the AI module to MongoDB types and bypass the trait abstraction the rest of the codebase respects.

### ADR-2: Hardcode `EMPRESARIAL` as a string constant inside `tools.rs`
- **Decision**: Define `const TAX_TARGET_EMPRESARIAL: &str = "EMPRESARIAL";` at the top of the new tool section in `tools.rs` and pass it to `find_tax_by_target` directly.
- **Rationale**: This is the only AI-driven calc today, the proposal explicitly defers per-agent configurability, and there is no evidence other consumers want a different segment. Promoting it to env var or `AiToolConfig` would create surface area we can't justify yet.
- **Rejected alternatives**:
  - **Per-agent toggle in `AiToolConfig.config`**: adds schema, validation, UI work for a knob nobody asked for. Open a follow-up change if/when needed.
  - **Env var (`AI_AGENT_TAX_TARGET`)**: same overhead, plus drift risk between environments. The constant is grep-able and trivially editable.

### ADR-3: Rate resolution order is Redis → DB fallback → typed errors
- **Decision**:
  1. `state.redis.get_exchange_rate()` — `Ok(Some(rate))` short-circuits.
  2. On `Ok(None)` or `Err(_)`, fall back to `state.db.get_latest_exchange_rate()`.
  3. If both fail → `ToolResult::err("exchange_rate_unavailable", started)`.
  4. If `rate == 0.0` (any source) → `ToolResult::err("exchange_rate_zero", started)`.
- **Rationale**: Mirrors the v2 `calculate_handler` (`src/modules/calculations/handler.rs:137-147`). Using a different policy here would be surprising for ops (two endpoints quoting the same conversion behaving differently). Redis is hot path (5 min TTL); DB is the cold-start safety net; both-fail is a hard error worth surfacing to the LLM so it can apologize coherently instead of inventing a number.
- **Rejected alternatives**:
  - **DB only**: ignores the Redis cache the rest of the system relies on; adds load.
  - **Redis only**: cold start after Redis flush would break the tool until the next BCV cron tick.
  - **Silent default to a stale value**: violates the "never invent numbers" guarantee that motivates the tool.

### ADR-4: Compute `bs_base` and `bs_with_iva` from the original `amount_usd`, no chained rounding
- **Decision**:
  ```rust
  let bs_base     = round2(amount_usd * rate);
  let bs_with_iva = round2(amount_usd * rate * iva_factor);
  let iva_percent = round2((iva_factor - 1.0) * 100.0);
  fn round2(x: f64) -> f64 { (x * 100.0).round() / 100.0 }
  ```
- **Rationale**: Chaining (`round2(round2(bs_base) * iva_factor)`) introduces a cumulative rounding error of up to 0.5 cents that diverges from the v2 handler. Calculating both legs from the original USD keeps the tool numerically aligned with `/v2/utils/calculate`, which is the reference any human auditor will compare against.
- **Rejected alternatives**:
  - **Single rounded `bs_with_iva` only**: loses the auditable breakdown the proposal requires.
  - **Chained rounding**: matches no other handler and accumulates error.

### ADR-5: NO renaming of existing `T_*` constants or category renumbering
- **Decision**: Append `T_CALCULATE_AMOUNT_BS` after `T_CHECK_COVERAGE` in the existing constants block. Add the new arm to `tool_default`, `execute_tool`, `tool_category` without re-ordering existing entries.
- **Rationale**: Pure-additive change keeps the diff reviewable, avoids merge conflicts with parallel work, and means `description_override` rows in production (keyed by tool name) keep matching.
- **Rejected alternatives**: Reordering for "logical grouping" — gratuitous churn with no functional benefit.

### ADR-6: Tool category is `InfoLookup`
- **Decision**: `tool_category(T_CALCULATE_AMOUNT_BS) → ToolCategory::InfoLookup`.
- **Rationale**: The tool reads BCV rate and IVA config — no external state mutation, no human handoff. This matches the contract documented at `tools.rs:128-141`: `Action` is reserved for tools that change state or transfer to a human. Marking it `Action` would erroneously reset `no_resolution_count` whenever the AI quotes a price, defeating the timeout-to-human fallback.
- **Rejected alternatives**:
  - **`Action`**: would break the no-resolution counter semantics.
  - **New `Calculation` category**: premature; one calc tool doesn't justify a new branch in dispatch logic.

---

## 2. Code-level design

### 2.1 New trait method (`src/db/mod.rs`)

In the `ProfileRepository` trait, append after `find_tax_by_id` (line 124):

```rust
/// Lookup de configuración de IVA por `sTarget` (segmento).
/// Usado por el AI Agent para resolver el IVA empresarial sin pasar
/// por el `idTax` de un cliente concreto. Devuelve `Ok(None)` si el
/// segmento no está configurado en `BCV.IVA`.
async fn find_tax_by_target(&self, target: &str) -> Result<Option<Tax>, String>;
```

### 2.2 Mongo implementation (`src/db/mongo/profile.rs`)

Insert after the `find_tax_by_id` impl (after line 261). The pattern mirrors the existing `DEFAULT` fallback branch of `find_tax_by_id` — but without any fallback (the tool wants an exact `EMPRESARIAL` hit or `None`).

```rust
async fn find_tax_by_target(&self, target: &str) -> Result<Option<Tax>, String> {
    let db_bcv = self.client.database("BCV");
    let collection: Collection<Tax> = db_bcv.collection("IVA");
    let filter = doc! { "sTarget": target };
    collection
        .find_one(filter)
        .await
        .map_err(|e| e.to_string())
}
```

Notes:
- Same database/collection (`BCV.IVA`) as `find_tax_by_id` — reuse confirmed.
- No projection: `Tax` is a small struct (3 fields) and we need `iva`.
- Error path returns `Err(String)` to match the trait's `Result<_, String>` convention used everywhere in `ProfileRepository`.

### 2.3 New tool in `src/modules/ai_agent/tools.rs`

#### 2.3.1 Constant (next to existing `T_*`, after `T_CHECK_COVERAGE` on line 122)

```rust
pub const T_CALCULATE_AMOUNT_BS: &str = "calculate_amount_bs";

/// Segmento de IVA aplicado por el tool `calculate_amount_bs`. Hardcoded
/// porque hoy todos los quotes públicos por WhatsApp se cotizan en
/// EMPRESARIAL. Para cambiar a multi-segmento, abrir un change separado.
const TAX_TARGET_EMPRESARIAL: &str = "EMPRESARIAL";
```

#### 2.3.2 Entry in `tool_default` (after `T_CHECK_COVERAGE` arm, before `T_TRANSFER_AGENT`)

```rust
T_CALCULATE_AMOUNT_BS => Some((
    "Calcula cuánto sale en bolívares un monto en USD aplicando la tasa BCV \
     vigente más IVA del 16% (segmento EMPRESARIAL). Llamar SIEMPRE que el \
     cliente pregunte un precio en Bs — NUNCA inventes la tasa ni el total. \
     La respuesta incluye el desglose: tasa, base sin IVA y monto final con IVA.",
    json!({
        "type": "object",
        "properties": {
            "amount_usd": {
                "type": "number",
                "description": "Monto en dólares a convertir. Debe ser mayor a 0."
            }
        },
        "required": ["amount_usd"]
    }),
)),
```

#### 2.3.3 Dispatch arm in `execute_tool` (after `T_CHECK_COVERAGE` on line 419)

```rust
T_CALCULATE_AMOUNT_BS => exec_calculate_amount_bs(args, ctx, started).await,
```

#### 2.3.4 Category arm in `tool_category` (line 147 block)

Add `T_CALCULATE_AMOUNT_BS` to the `InfoLookup` arm:

```rust
T_LOOKUP_CUSTOMER
| T_LIST_PLANS
| T_CHECK_COVERAGE
| T_GET_INVOICES
| T_CALCULATE_AMOUNT_BS => ToolCategory::InfoLookup,
```

#### 2.3.5 Implementation skeleton (append to the bottom of `tools.rs`, in its own banner section)

```rust
// ============================================
// Tool: calculate_amount_bs
// ============================================

#[derive(Deserialize)]
struct CalculateAmountBsArgs {
    amount_usd: f64,
}

#[inline]
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

async fn exec_calculate_amount_bs(
    args: Value,
    ctx: &ToolContext,
    started: Instant,
) -> ToolResult {
    // 1. Parse args
    let parsed: CalculateAmountBsArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };
    let amount_usd = parsed.amount_usd;

    // 2. Validate amount
    if !(amount_usd > 0.0) {  // catches 0, negatives, NaN
        return ToolResult::err("invalid_amount", started);
    }

    // 3. Resolve BCV rate (Redis → DB fallback)
    let rate: f64 = match ctx.state.redis.get_exchange_rate().await {
        Ok(Some(r)) => r,
        _ => match ctx.state.db.get_latest_exchange_rate().await {
            Ok(r) => r,
            Err(_) => return ToolResult::err("exchange_rate_unavailable", started),
        },
    };
    if rate == 0.0 {
        return ToolResult::err("exchange_rate_zero", started);
    }

    // 4. Resolve EMPRESARIAL tax (NO DEFAULT fallback)
    let tax = match ctx.state.db.find_tax_by_target(TAX_TARGET_EMPRESARIAL).await {
        Ok(Some(t)) => t,
        Ok(None) => return ToolResult::err("tax_config_missing", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let iva_factor = tax.iva;

    // 5. Compute (no chained rounding)
    let bs_base     = round2(amount_usd * rate);
    let bs_with_iva = round2(amount_usd * rate * iva_factor);
    let iva_percent = round2((iva_factor - 1.0) * 100.0);

    // 6. Date stamp (Caracas TZ — coherent with the cron BCV daily key).
    let rate_date = crate::utils::timezone::VenezuelaDateTime::now()
        .date_string_venezuela();

    // 7. Result
    ToolResult::ok(
        json!({
            "amount_usd": amount_usd,
            "bcv_rate": rate,
            "rate_date": rate_date,
            "iva_factor": iva_factor,
            "iva_percent": iva_percent,
            "amount_bs_base": bs_base,
            "amount_bs_with_iva": bs_with_iva,
        }),
        started,
    )
}
```

### 2.4 Error code reference

| Code | Cause |
|------|-------|
| `invalid_args:<serde_msg>` | `amount_usd` missing or wrong type |
| `invalid_amount` | `amount_usd <= 0` or `NaN` |
| `exchange_rate_unavailable` | Redis err **and** DB err |
| `exchange_rate_zero` | Resolved rate is exactly `0.0` |
| `tax_config_missing` | No `BCV.IVA` doc with `sTarget="EMPRESARIAL"` |
| `db_error:<msg>` | Mongo error during tax lookup |

---

## 3. Sequence diagram

```
Client (WhatsApp): "¿cuánto es 10 USD?"
        │
        ▼
WaWebhook → dispatch.rs → runner.execute_loop
        │
        ▼
runner → Gemini (with tools registry, including calculate_amount_bs)
        │
        ▼  functionCall { name: "calculate_amount_bs", args: { amount_usd: 10 } }
runner.execute_tool("calculate_amount_bs", args, ctx)
        │
        ▼
tools.exec_calculate_amount_bs(args, ctx, started)
        │
        ├── parse + validate amount_usd > 0
        │
        ├── ctx.state.redis.get_exchange_rate()
        │     ├── Ok(Some(rate)) → use it
        │     └── Ok(None) | Err → fallback ⤵
        │
        ├── ctx.state.db.get_latest_exchange_rate()
        │     ├── Ok(rate) → use it
        │     └── Err → ToolResult::err("exchange_rate_unavailable")
        │
        ├── if rate == 0.0 → ToolResult::err("exchange_rate_zero")
        │
        ├── ctx.state.db.find_tax_by_target("EMPRESARIAL")
        │     ├── Ok(Some(tax)) → iva_factor = tax.iva
        │     ├── Ok(None)      → ToolResult::err("tax_config_missing")
        │     └── Err(e)        → ToolResult::err("db_error:...")
        │
        ├── compute bs_base, bs_with_iva, iva_percent (no chained rounding)
        │
        ├── rate_date = VenezuelaDateTime::now().date_string_venezuela()
        │
        └── ToolResult::ok(json!{ 7 fields })
        │
        ▼
runner → Gemini next turn with tool result as functionResponse
        │
        ▼
Gemini renders natural language: "10 USD son Bs. 1.060,82 (tasa 91,45 + IVA 16%)..."
        │
        ▼
Client receives WhatsApp message
```

Side-effect note: this tool does **not** consult `is_sandbox`. It is a pure read of cache + DB; sandbox vs live behaves identically. `request_human` and `create_ticket` short-circuit on sandbox; `calculate_amount_bs` follows the `lookup_customer` / `get_invoices` / `list_plans` pattern (always live read, safe for sandbox).

---

## 4. Open questions

### Q1: `rate_date` source — `VenezuelaDateTime::now()` vs. a date stamp on the rate document?
- **Recommendation**: `VenezuelaDateTime::now().date_string_venezuela()`.
- **Rationale**:
  - The Redis key for the exchange rate has a 5-minute TTL refreshed by `cron_bcv.rs` (daily). The rate is conceptually "today's rate" by construction.
  - Deriving the date from the rate document (`Rates.dDate` or similar) would mean an extra DB read on every tool call **and** a divergence whenever Redis serves a value the cron pushed today (the document is from yesterday's BCV publication but valid for today's quote).
  - The downside ("stale rate after midnight before cron runs") is bounded: the cron runs every day; the gap is operational, not a data-correctness issue. The system prompt can ask the AI to disclose "tasa BCV de hoy" — not a notarial timestamp.
- **Action**: ship with `VenezuelaDateTime::now()`. If accounting-grade auditability is required later, swap by pulling `dDate` from the latest `Rates` doc — single-line change in step 6 of the impl.

There are no other unresolved questions blocking the implementation.

---

## 5. Testing approach

The project does **not** practice TDD (`openspec/config.yaml`). The verify phase will rely on:

1. **`cargo check`** — must pass with zero new warnings.
2. **Manual smoke test** during apply: invoke `execute_tool("calculate_amount_bs", json!({"amount_usd": 10.0}), &ctx)` from a quick repl/test scaffold or a temporary debug endpoint and assert all 7 output keys are present with coherent values.
3. **Error-path smoke**: confirm `invalid_amount` for `amount_usd: 0`, `tax_config_missing` after temporarily renaming the `EMPRESARIAL` doc in a sandbox Mongo.

No new `tests/` file is required for this change. Existing tools have no per-tool unit tests either; consistency wins over a one-off.

---

## 6. Affected files (recap from proposal)

| File | Change |
|------|--------|
| `src/db/mod.rs` | +1 trait method on `ProfileRepository` |
| `src/db/mongo/profile.rs` | +1 method impl (`find_tax_by_target`) |
| `src/modules/ai_agent/tools.rs` | +1 const, +1 internal const, +1 entry in `tool_default`, +1 arm in `execute_tool`, extend `tool_category` InfoLookup arm, +`exec_calculate_amount_bs` fn (~50 LOC) |
| `openspec/specs/ai-agent/spec.md` | +1 row in tools registry (delta — handled by spec phase, not here) |

No data migration. No env var. No new dependency.
