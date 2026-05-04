# Tasks: AI Agent — `calculate_amount_bs` tool

> Artifact store: openspec + engram (hybrid)
> Sequential constraint: Phase 1 must complete before Phase 2 (trait method must exist before it is called).

---

## Phase 1 — DB Layer (trait + impl)

- [x] 1.1 In `src/db/mod.rs`, add `async fn find_tax_by_target(&self, target: &str) -> Result<Option<Tax>, String>;` to `ProfileRepository` trait, after `find_tax_by_id` (line 124).
  _Spec ref: Requirement — Tool MUST call `find_tax_by_target`, not `find_tax_by_id` (no fallback to DEFAULT)._

- [x] 1.2 In `src/db/mongo/profile.rs`, implement `find_tax_by_target` after `find_tax_by_id` (after line 261). Use `find_one(doc!{ "sTarget": target })` on `BCV.IVA`, return `Ok(None)` if absent. No DEFAULT fallback.
  _Spec ref: Scenario 6 — MUST NOT fall back to `sTarget = "DEFAULT"` or any other value._

- [x] 1.3 Run `cargo check` — must compile with zero new warnings before proceeding to Phase 2.

---

## Phase 2 — AI Agent Tool

- [x] 2.1 In `src/modules/ai_agent/tools.rs`, add two constants after `T_CHECK_COVERAGE` (line 122):
  ```rust
  pub const T_CALCULATE_AMOUNT_BS: &str = "calculate_amount_bs";
  const TAX_TARGET_EMPRESARIAL: &str = "EMPRESARIAL";
  ```
  _Spec ref: Requirement — tool named `calculate_amount_bs`, `sTarget` hardcoded to `EMPRESARIAL`._

- [x] 2.2 In `tool_default()`, add arm for `T_CALCULATE_AMOUNT_BS` after `T_CHECK_COVERAGE` arm. Include description (must say "NUNCA inventes la tasa") and JSON schema with `amount_usd: number, required`.
  _Spec ref: Scenario 3 — missing `amount_usd` must yield `invalid_args`; the schema enforces the field._

- [x] 2.3 In `tool_category()`, extend the `InfoLookup` arm to include `T_CALCULATE_AMOUNT_BS`:
  ```rust
  T_LOOKUP_CUSTOMER | T_LIST_PLANS | T_CHECK_COVERAGE | T_GET_INVOICES
  | T_CALCULATE_AMOUNT_BS => ToolCategory::InfoLookup,
  ```
  _Spec ref: Scenario 8 — `tool_category("calculate_amount_bs")` MUST return `InfoLookup`._

- [x] 2.4 In `execute_tool()` dispatch `match`, add arm after `T_CHECK_COVERAGE`:
  ```rust
  T_CALCULATE_AMOUNT_BS => exec_calculate_amount_bs(args, ctx, started).await,
  ```
  _Spec ref: all Scenarios — routing prerequisite._

- [x] 2.5 Append `exec_calculate_amount_bs` function and `CalculateAmountBsArgs` struct to the bottom of `tools.rs` (use design §2.3.5 skeleton verbatim). Steps inside:
  - Parse args → `invalid_args` on failure (Scenario 3)
  - Validate `amount_usd > 0.0` → `invalid_amount` (Scenario 2)
  - Redis → DB rate resolution → `exchange_rate_unavailable` (Scenario 4); `exchange_rate_zero` if rate == 0.0 (Scenario 5)
  - `find_tax_by_target(TAX_TARGET_EMPRESARIAL)` → `tax_config_missing` on `Ok(None)` (Scenario 6)
  - Compute `bs_base`, `bs_with_iva`, `iva_percent` from original `amount_usd` (no chained rounding) (Scenario 1)
  - `rate_date` from `VenezuelaDateTime::now().date_string_venezuela()`
  - Return `ToolResult::ok(json!{...})` with all 7 fields

- [x] 2.6 Add `round2` inline helper in the same section (check first: `round2` does NOT exist in `calculations/handler.rs` — confirmed absent; safe to add locally in `tools.rs`).
  _Spec ref: Scenario 1 — rounding MUST be `(x * 100.0).round() / 100.0`._

- [x] 2.7 Run `cargo check` — must pass with zero new warnings.

---

## Phase 3 — Verification

- [x] 3.1 Pre-implementation check (done during design review — confirming for implementer):
  - `VenezuelaDateTime::now().date_string_venezuela()` EXISTS at `src/utils/timezone.rs:41` — confirmed.
  - `RedisClient::get_exchange_rate()` returns `Result<Option<f64>, RedisError>` — confirmed at `src/cache/redis_client.rs:28`.
  - `SalesRepository::get_latest_exchange_rate()` returns `Result<f64, MongoError>` — confirmed at `src/db/mod.rs:173`. The `Err(_)` arm in the design's match is correct.

- [x] 3.2 `cargo check` final pass — zero warnings, no new errors.

- [ ] 3.3 Smoke test (manual, post-deploy via existing AI Agent debug endpoint or temporary scaffold):
  - Happy path: call `execute_tool("calculate_amount_bs", json!({"amount_usd": 10.0}), &ctx)` → verify all 7 keys present with coherent numeric values.
  - `amount_usd: 0` → `ToolResult::err` with code `invalid_amount`.
  - `amount_usd: -5` → `ToolResult::err` with code `invalid_amount`.
  - Missing field `{}` → `ToolResult::err` with code starting `invalid_args:`.
  - Temporarily rename `EMPRESARIAL` doc in sandbox Mongo → `ToolResult::err` with code `tax_config_missing`.
  _Spec ref: Scenarios 1-6 — smoke coverage without unit tests (project convention: no per-tool test files)._

- [ ] 3.4 Regression check: call `tool_category` on all existing tool names (`lookup_customer`, `get_invoices`, `request_human`, `create_ticket`, `transfer_to_agent`, `list_plans`, `check_coverage`) — confirm none changed.
  _Spec ref: Modified Requirement — categorization table must remain exhaustive._

---

## Phase 4 — OpenAPI / Cleanup

- [x] 4.1 NO `openapi.rs` update needed. `calculate_amount_bs` is an internal AI Agent tool invoked by the LLM runtime, not a public HTTP endpoint. No new route, no new schema to register. This is intentionally omitted.

---

## Notes

- `get_latest_exchange_rate()` returns `Result<f64, MongoError>` (not `Option`) — the `Ok(None)` / `Ok(0.0)` ambiguity does not exist. DB error maps to `exchange_rate_unavailable`; DB returns a valid float or errors.
- The design's `match ctx.state.db.get_latest_exchange_rate().await { Ok(r) => r, Err(_) => return ToolResult::err(...) }` is exactly correct.
- `round2` is a module-private helper — name collision risk is zero (only `calculations` module uses a similar inline expression, not a named function).
- No data migration, no env var, no new Cargo dependency.
