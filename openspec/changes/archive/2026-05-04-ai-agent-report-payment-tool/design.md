# Design: AI Agent — `report_payment` tool

> Phase: design (HOW at architectural level). Tasks come next.
> Decisions locked upstream:
> - Input is `media_id`, not `image_url`.
> - `ctx.ai_user_id` already lives in `ToolContext`.
> - Audit trail via `id_creator: Option<String>` on `PaymentReport`.

## 1. Architecture decisions (ADRs)

### ADR-1 — Tool input: `media_id`, not `image_url`

**Decision**: The tool receives `media_id` (Meta's stable identifier) and resolves the binary internally.

**Why**: Meta CDN URLs from `download_media_info` are signed and expire (~5 min TTL). If the LLM passed an `image_url`, by the time the tool runs we'd risk a stale signature. `media_id` is permanent (well, as long as Meta retains it on their side, which is days/weeks for inbound media). Also, the LLM never sees URLs — it only sees the inline `MediaInput` we feed Gemini in `dispatch::build_media_inputs`. Forcing the LLM to pass back a URL it never had is a footgun.

**Rejected alternative**: Pass `image_url` (relative path the AI somehow constructed). Rejected because the AI has no way to construct a stable path; we'd be inventing one and hoping the model echoes it correctly.

**Consequence**: The tool MUST download the media from Meta inside `exec_report_payment` (after validations pass). This adds one network roundtrip per successful call but is the only correct option.

---

### ADR-2 — Reuse `WhatsAppService::download_media`

**Decision**: Build a `WhatsAppService` instance inside `exec_report_payment` mirroring the pattern in `dispatch::build_media_inputs` (lines 1091–1113 of `src/modules/ai_agent/dispatch.rs`). Call `svc.download_media(media_id)` to get `(bytes, mime, _filename)`.

**Why**: The helper already exists, handles the relay (`WA_MEDIA_RELAY_URL`/`SECRET`) for the ISP-bypass, decrypts the access token, and is battle-tested. Designing a parallel downloader would be duplication.

**Constraint**: `WhatsAppService::new(reqwest_client, phone_number_id, access_token)` requires the **decrypted** access token. The token lives encrypted in `WaSettings.access_token`, decrypted via `decrypt_payload(&ai_agent_secret(), &wa_settings.access_token)`. We resolve `WaSettings` from `ctx.state.db.find_wa_settings_by_id(&ctx.workspace_id)` — already part of `WhatsAppRepository` (verified in `src/db/mod.rs:591`).

**No new helper required**. The 20-line snippet in `dispatch.rs` is the canonical pattern; the tool inlines it. If this becomes a third site, extract to `whatsapp::service::resolve_service_for_workspace(&state, workspace_id)` in a follow-up — out of scope here.

**Function signatures used (verified in `src/modules/whatsapp/service.rs`)**:
```rust
pub async fn download_media(&self, media_id: &str) -> Result<(Vec<u8>, String, Option<String>)>;
//                                                          (bytes, mime,    file_name)
```

---

### ADR-3 — Image storage: same convention as the HTTP endpoint

**Decision**: Save to `uploads/{Uuid::new_v4()}.{ext}` and persist `image_url = "/uploads/{name}"` (leading slash, relative). Mirror `report_payment_handler` exactly (lines 313–338 of `src/modules/payments/handler.rs`).

**Why**: Filename pattern, directory, leading-slash convention — all already serving images consumed by the dashboard. Inventing a new naming scheme (`wa-payment-{conv_id}-{ts}.ext`) would create two parallel conventions for the same logical asset. The reviewer/admin UI doesn't care who uploaded — it expects `/uploads/<id>.<ext>`.

**Extension resolution**: derive from the `mime` string returned by `download_media` (NOT from a multipart `content_type`). Map:
- `image/png` → `png`
- `image/webp` → `webp`
- `image/gif` → `gif`
- `image/jpeg` (default) → `jpg`

**Rejected alternative**: Re-upload to S3 / GridFS / proper object storage. Out of scope — proposal explicitly excludes "Re-upload de la imagen a storage propio". Future work.

**Filesystem assumption**: `uploads/` exists relative to the process CWD. The HTTP handler assumes the same — if it works there, it works here.

---

### ADR-4 — Idempotency check BEFORE any network or DB write

**Decision**: Order of operations:
1. Validate args (image, amount, etc.) — pure, no I/O.
2. Sandbox short-circuit.
3. Parse `client_id`.
4. `find_client_by_id` (to confirm client exists; this also gives us `id_tax`).
5. **`check_reference(client_id, reference)`** — if `Some(_)`, short-circuit return `already_registered=true`.
6. Resolve owner → payment_method.
7. Resolve exchange_rate, iva_rate.
8. Compute amounts.
9. **Download media from Meta** (network call).
10. Build `PaymentReport` and `create_payment_report`.

**Why**: A retry from Gemini (same `(client_id, reference)`) MUST NOT hit Meta CDN. Meta media IDs are reusable but rate-limited; we don't want to spend the budget re-downloading on every retry. `check_reference` is a single Mongo query against an index — cheap.

**Note**: We do `find_client_by_id` BEFORE `check_reference` because `check_reference` needs an `ObjectId` for `id_client`, and we need to validate the client exists anyway to fetch `id_tax`. The order is tight and intentional.

**Edge case**: `check_reference` returns `Some(_)` even when the reference matches a different client. The `ReferenceMatchInfo.is_same_client` field tells us. For the AI tool, we treat ANY match as `already_registered=true` to be conservative — if a human later finds the dup, they can sort it out. We DO surface `is_same_client` in the response so the LLM can warn the customer ("ya está registrada por otro cliente").

---

### ADR-5 — Tool category: `Action`

**Decision**: `tool_category(T_REPORT_PAYMENT) -> ToolCategory::Action`.

**Why**: A successful execution mutates DB state (`PaymentReports` insert) and represents a resolution event ("the customer's claim is recorded"). `dispatch.rs` uses this to reset `no_resolution_count`. Mirror `T_CREATE_TICKET`'s arm.

**Idempotent re-call** (`already_registered=true`) is also `Action` — the resolution already happened, the counter still resets. Same as if `create_ticket` were called twice.

---

### ADR-6 — Sandbox short-circuit BEFORE downloads or DB writes

**Decision**: When `ctx.is_sandbox`, after argument validation, return synthetic payload:
```json
{
  "ok": true,
  "mode": "sandbox",
  "payment_id": "sandbox-fake-payment",
  "already_registered": false,
  "amount_bs": <echo from input or computed from input>,
  "amount_usd": <echo from input or computed from input>,
  "exchange_rate": 0.0,
  "iva_rate": 1.0
}
```

**Why**: Mirror `exec_create_ticket` (line 929). Sandbox is for end-to-end integration testing of the LLM loop without side effects. No Meta call, no DB write. Validations DO run before the short-circuit so the developer testing in sandbox sees real error codes (`image_required`, `amount_conflict`, etc.) — those are pure logic and worth exercising.

**Note**: We do NOT call `find_client_by_id` in sandbox — the test agent might use a fake `client_id`. Validations stop at "args parse correctly".

---

### ADR-7 — No new trait methods

**Decision**: All needed repo methods already exist on traits used by other tools or by `dispatch.rs`. Specifically (verified in `src/db/mod.rs`):

| Method | Trait | Line | Used by |
|---|---|---|---|
| `find_client_by_id` | `ProfileRepository` | 115 | tools.rs (existing tools) |
| `find_tax_by_id` | `ProfileRepository` | 124 | tools.rs (existing tools) |
| `check_reference` | `SalesRepository` | 241 | payments handler |
| `find_client_owner_by_id` | `SalesRepository` | 193 | payments handler |
| `find_user_payment_info_by_id` | `SalesRepository` | 199 | payments handler |
| `create_payment_report` | `SalesRepository` | 207 | payments handler |
| `get_latest_exchange_rate` | `SalesRepository` | 179 | payments handler |
| `find_wa_settings_by_id` | `WhatsAppRepository` | 591 | dispatch.rs media download |

All are accessible via `ctx.state.db` because `Db` is the master trait combining all of them. The existing `tools.rs` already imports `SalesRepository` (line 22). We add `WhatsAppRepository` to the imports if not already there — verified: it IS already imported (line 23).

**Conclusion**: zero trait surface changes. Pure additive code in `tools.rs`.

---

### ADR-8 — Schema change: `PaymentReport.id_creator: Option<String>`

**Decision**: Add field to `src/models/payment.rs`:
```rust
#[serde(rename = "idCreator", skip_serializing_if = "Option::is_none", default)]
pub id_creator: Option<String>,
```

**Why**:
- `Option<String>` + `#[serde(default)]` → existing docs in `PaymentReports` (without the field) deserialize as `None`. Backwards compatible.
- `skip_serializing_if = "Option::is_none"` → human-created reports (from the multipart endpoint) keep writing without the field, no change to existing payloads.
- `String` (UUID) — matches the `idCreator` convention used elsewhere (e.g., `idEditor`, `idCreator` on `Clients`, `WaTicket.created_by_id`). The AI synthetic user has a UUID.

**Migration**: None required. New field, optional, default-aware.

**Rejected alternative**: Discriminator field like `created_by_ai: bool`. Rejected because the existing convention across the codebase is `idCreator: String` everywhere. Bool flags don't tell you WHICH agent created it (which we'll want for audit later). UUID does.

**Audit consequence**: Filtering "reports created by AI" becomes a query against `idCreator` matching the AI synthetic user UUID(s). We can build a per-agent dashboard tile from this.

---

## 2. Code-level design

### 2.1 Schema change

`src/models/payment.rs` — add a single field to `PaymentReport`:

```rust
#[serde(rename = "idCreator", skip_serializing_if = "Option::is_none", default)]
pub id_creator: Option<String>,
```

The HTTP endpoints (`report_payment_handler`, `report_payment_user_handler`) will need to set this to `None` explicitly when constructing `PaymentReport` since they don't currently. **This is the ONE side effect on existing code**.

### 2.2 Tool registry entries

In `src/modules/ai_agent/tools.rs`:

```rust
pub const T_REPORT_PAYMENT: &str = "report_payment";
```

`tool_default()` arm:

```rust
T_REPORT_PAYMENT => Some((
    "Registra un reporte de pago del cliente (referencia + monto + comprobante). \
     PRECONDICIONES: (1) llamá `lookup_customer` ANTES y confirmá con el cliente \
     cuál servicio si hay varios. (2) Pedile la foto del comprobante por WhatsApp \
     — sin imagen el tool falla. (3) Pasá `amount_bs` O `amount_usd`, NUNCA ambos: \
     el sistema deriva el otro con la tasa BCV vigente.",
    json!({
        "type": "object",
        "properties": {
            "client_id":    { "type": "string", "description": "ObjectId hex devuelto por lookup_customer." },
            "reference":    { "type": "string", "description": "Referencia bancaria del comprobante." },
            "media_id":     { "type": "string", "description": "ID del media de WhatsApp (foto del comprobante). Lo recibís en el contexto del mensaje del cliente." },
            "amount_bs":    { "type": "number", "description": "Monto en bolívares. Mutuamente excluyente con amount_usd." },
            "amount_usd":   { "type": "number", "description": "Monto en dólares. Mutuamente excluyente con amount_bs." },
            "bank":         { "type": "string", "description": "Nombre del banco origen del pago. Opcional." },
            "phone":        { "type": "string", "description": "Teléfono asociado al pago móvil. Opcional." },
            "debt_id":      { "type": "string", "description": "ObjectId hex de la deuda específica si el cliente la mencionó. Opcional — si falta, el reporte queda como abono a cuenta." },
            "payment_date": { "type": "string", "description": "Fecha del pago en RFC3339 (ej: 2026-05-04T15:30:00Z). Opcional — default: ahora." }
        },
        "required": ["client_id", "reference", "media_id"]
    }),
)),
```

`tool_category()` arm: add `T_REPORT_PAYMENT` to the `ToolCategory::Action` group.

`execute_tool()` arm:

```rust
T_REPORT_PAYMENT => exec_report_payment(args, ctx, started).await,
```

### 2.3 `ReportPaymentArgs`

```rust
#[derive(Deserialize)]
struct ReportPaymentArgs {
    client_id: String,
    reference: String,
    media_id: String,
    #[serde(default)]
    amount_bs: Option<f64>,
    #[serde(default)]
    amount_usd: Option<f64>,
    #[serde(default)]
    bank: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    debt_id: Option<String>,
    #[serde(default)]
    payment_date: Option<String>,
}
```

### 2.4 `exec_report_payment` skeleton

Refer to the full design document for the 15-step implementation skeleton with imports, error handling, and the complete function body.

---

## 3. Sequence diagram

```
Customer (WhatsApp): "Pagué 50 USD por banesco, ref ABC123"
  + sends receipt image (Meta media_id: <X>)
        │
        ▼
Webhook (POST /v1/webhook/whatsapp)
        │
        ▼
dispatch.rs (run_dispatch_for_inbound)
  ├─ resolve workspace_id from business_phone
  ├─ load AiAgent (active for conv)
  ├─ build_media_inputs() → downloads media, feeds Gemini as MediaInput
  └─ runner.run_turn() → Gemini sees image + text + tool registry
        │
        ▼
Gemini → functionCall { name: "report_payment",
                        args: { client_id, reference, media_id, amount_usd, bank, ... } }
        │
        ▼
runner.execute_tool(name, args, ctx)
        │
        ▼
exec_report_payment(args, ctx, started)
```

---

## 4. Affected files (recap)

| File | Change | Notes |
|---|---|---|
| `src/models/payment.rs` | +`id_creator: Option<String>` on `PaymentReport` | One line. Backwards compat via `default`. |
| `src/modules/payments/handler.rs` | Set `id_creator: None` in 2 `PaymentReport` literals | Mechanical — required because struct gained a field. |
| `src/modules/ai_agent/tools.rs` | +const, +tool_default arm, +tool_category arm, +execute_tool arm, +ReportPaymentArgs, +exec_report_payment | The bulk of the change — ~200 lines. |
| `src/modules/ai_agent/dispatch.rs` | Make `ai_agent_secret` `pub(super)` | Single visibility tweak. |
| `src/db/mod.rs` | No change | All trait methods already exist. |
| `src/modules/whatsapp/service.rs` | No change | Reuse existing `download_media`. |
| `openspec/specs/ai-agent/spec.md` | Spec delta merged in archive phase | Out of scope for this design doc. |

---

## 5. Testing approach

Per project conventions: `cargo check` only. No automated tests added — consistent with the rest of `tools.rs`.
