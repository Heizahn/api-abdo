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

```rust
async fn exec_report_payment(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    use chrono::{DateTime, Utc};
    use uuid::Uuid;
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;

    // 1. Parse args
    let parsed: ReportPaymentArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };

    // 2. Validate (pure logic — no I/O)
    if parsed.media_id.trim().is_empty() {
        return ToolResult::err("image_required", started);
    }
    if parsed.reference.trim().is_empty() {
        return ToolResult::err("reference_required", started);
    }
    let (amount_input_bs, amount_input_usd) = match (parsed.amount_bs, parsed.amount_usd) {
        (None, None)            => return ToolResult::err("amount_required", started),
        (Some(_), Some(_))      => return ToolResult::err("amount_conflict", started),
        (Some(b), None) if b > 0.0 => (Some(b), None),
        (None, Some(u)) if u > 0.0 => (None, Some(u)),
        _                       => return ToolResult::err("invalid_amount", started),
    };

    // 3. Sandbox short-circuit (after validation, before any side effect)
    if ctx.is_sandbox {
        return ToolResult::ok(
            json!({
                "ok": true,
                "mode": "sandbox",
                "payment_id": "sandbox-fake-payment",
                "already_registered": false,
                "amount_bs": amount_input_bs,
                "amount_usd": amount_input_usd,
                "exchange_rate": 0.0,
                "iva_rate": 1.0,
            }),
            started,
        );
    }

    // 4. Parse client_id
    let client_oid = match ObjectId::parse_str(parsed.client_id.trim()) {
        Ok(o) => o,
        Err(_) => return ToolResult::err("invalid_client_id", started),
    };

    // 5. Find client (need id_tax)
    let client = match ctx.state.db.find_client_by_id(&client_oid.to_hex()).await {
        Ok(c) => c,
        Err(_) => return ToolResult::err("client_not_found", started),
    };
    // NOTE: find_client_by_id returns Result<Client, String>; if the impl returns
    // Err on missing, we map to client_not_found. Confirm impl does NOT panic on
    // missing — it should propagate "not found" as Err.

    // 6. Idempotency check — BEFORE any network or write
    let trimmed_ref = parsed.reference.trim().to_string();
    match ctx.state.db.check_reference(&client_oid, &trimmed_ref).await {
        Ok(Some(match_info)) => {
            return ToolResult::ok(
                json!({
                    "ok": true,
                    "mode": "live",
                    "already_registered": true,
                    "source": match_info.source,
                    "is_same_client": match_info.is_same_client,
                    "matched_reference": match_info.s_reference,
                    "matched_state": match_info.s_state,
                    "matched_amount_bs": match_info.n_bs,
                    "matched_amount_usd": match_info.n_amount,
                }),
                started,
            );
        }
        Ok(None) => {} // proceed
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    }

    // 7. Resolve owner → payment_method
    let owner = match ctx.state.db.find_client_owner_by_id(&client_oid).await {
        Ok(Some(o)) => o,
        Ok(None) => return ToolResult::err("client_owner_not_found", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let user_info = match ctx.state.db.find_user_payment_info_by_id(&owner.id_owner).await {
        Ok(Some(u)) => u,
        Ok(None) => return ToolResult::err("payment_method_not_configured", started),
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let id_payment_method = match user_info.id_payment_method {
        Some(id) => id,
        None => return ToolResult::err("payment_method_not_configured", started),
    };

    // 8. Resolve exchange rate (Redis → DB)
    let exchange_rate: f64 = match ctx.state.redis.get_exchange_rate().await {
        Ok(Some(r)) => r,
        _ => match ctx.state.db.get_latest_exchange_rate().await {
            Ok(r) => r,
            Err(_) => return ToolResult::err("exchange_rate_unavailable", started),
        },
    };
    if exchange_rate <= 0.0 {
        return ToolResult::err("exchange_rate_zero", started);
    }

    // 9. Resolve iva_rate (default 1.0 if id_tax missing/not found)
    let iva_rate: f64 = if let Some(tax_id) = client.id_tax {
        match ctx.state.db.find_tax_by_id(Some(tax_id)).await {
            Ok(Some(t)) => t.iva,
            _ => 1.0,
        }
    } else {
        1.0
    };

    // 10. Compute the missing amount
    let (amount_bs, amount_usd) = match (amount_input_bs, amount_input_usd) {
        (Some(bs), None) => {
            let bs_neto = bs / iva_rate;
            let usd = round2(bs_neto / exchange_rate);
            (round2(bs), usd)
        }
        (None, Some(usd)) => {
            let bs_neto = usd * exchange_rate;
            let bs = round2(bs_neto * iva_rate);
            (bs, round2(usd))
        }
        _ => unreachable!("amounts validated above"),
    };

    // 11. Resolve WaSettings → build WhatsAppService → download media
    let wa_settings = match ctx.state.db.find_wa_settings_by_id(&ctx.workspace_id).await {
        Ok(Some(s)) => s,
        _ => return ToolResult::err("wa_settings_not_found", started),
    };
    let token = match decrypt_payload(&ai_agent_secret(), &wa_settings.access_token) {
        Some(t) => t,
        None => return ToolResult::err("wa_token_decrypt_failed", started),
    };
    let mut svc = crate::modules::whatsapp::service::WhatsAppService::new(
        ctx.state.reqwest_client.clone(),
        wa_settings.phone_number_id.clone(),
        token,
    );
    if let (Some(url), Some(secret)) = (
        ctx.state.config.wa_media_relay_url.as_ref(),
        ctx.state.config.wa_media_relay_secret.as_ref(),
    ) {
        svc = svc.with_media_relay(crate::modules::whatsapp::service::MediaRelay {
            url: url.clone(),
            secret: secret.clone(),
        });
    }
    let (bytes, mime, _filename) = match svc.download_media(&parsed.media_id).await {
        Ok(t) => t,
        Err(e) => return ToolResult::err(format!("image_download_failed:{}", e), started),
    };
    if bytes.is_empty() {
        return ToolResult::err("image_empty", started);
    }

    // 12. Save to uploads/ (mirror payments::handler convention)
    let ext = match mime.as_str() {
        "image/png"  => "png",
        "image/webp" => "webp",
        "image/gif"  => "gif",
        _            => "jpg",
    };
    let unique_name = format!("{}.{}", Uuid::new_v4(), ext);
    let file_path = format!("uploads/{}", unique_name);
    if let Err(e) = async {
        let mut file = File::create(&file_path).await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        Ok::<_, std::io::Error>(())
    }.await {
        return ToolResult::err(format!("image_save_failed:{}", e), started);
    }
    let image_url = format!("/uploads/{}", unique_name);

    // 13. Parse optional debt_id and payment_date
    let id_debt_oid: Option<ObjectId> = match parsed.debt_id.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(s) => match ObjectId::parse_str(s) {
            Ok(o) => Some(o),
            Err(_) => return ToolResult::err("invalid_debt_id", started),
        },
        None => None,
    };
    let payment_date: DateTime<Utc> = parsed.payment_date
        .as_deref()
        .and_then(|d| d.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);

    // 14. Build PaymentReport
    let report = crate::models::payment::PaymentReport {
        id: None,
        id_client: Some(client_oid),
        id_debt: id_debt_oid,
        id_payment_method: Some(id_payment_method),
        reference: trimmed_ref,
        payment_date,
        amount_bs,
        bank_origin: parsed.bank.unwrap_or_default(),
        phone_number: parsed.phone.unwrap_or_default(),
        image_url,
        amount_usd,
        exchange_rate,
        state: "Pendiente".to_string(),
        rejection_reason: None,
        id_creator: Some(ctx.ai_user_id.clone()),
        created_at: Utc::now(),
    };

    // 15. Persist
    let inserted = match ctx.state.db.create_payment_report(report).await {
        Ok(r) => r,
        Err(e) => return ToolResult::err(format!("db_error:{}", e), started),
    };
    let payment_id = inserted
        .inserted_id
        .as_object_id()
        .map(|o| o.to_hex())
        .unwrap_or_default();

    ToolResult::ok(
        json!({
            "ok": true,
            "mode": "live",
            "payment_id": payment_id,
            "already_registered": false,
            "amount_bs": amount_bs,
            "amount_usd": amount_usd,
            "exchange_rate": exchange_rate,
            "iva_rate": iva_rate,
            "is_advance": id_debt_oid.is_none(),
        }),
        started,
    )
}
```

**Imports to add to `tools.rs`** (top of file):
- `use crate::crypto::aes::decrypt_payload;` (already used in dispatch — fresh import in tools.rs)
- The `ai_agent_secret()` helper currently lives in `dispatch.rs:53` and `sandbox.rs:58`. Since both copies do the same thing (read `JWT_SECRET` env), tools.rs needs its own copy or one of them needs to be `pub` in a shared mod. **Recommendation**: make `dispatch::ai_agent_secret` `pub(super)` and import it in tools.rs as `use super::dispatch::ai_agent_secret;`. Avoids a third duplicate. Document this in the tasks phase.

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
  │
  ├─ [pure] parse args                        → invalid_args?
  ├─ [pure] validate media_id non-empty       → image_required?
  ├─ [pure] validate reference non-empty      → reference_required?
  ├─ [pure] validate exactly one amount       → amount_required / amount_conflict / invalid_amount?
  ├─ if ctx.is_sandbox → return synthetic; STOP
  │
  ├─ [DB]   parse client_id                   → invalid_client_id?
  ├─ [DB]   find_client_by_id(client_id)      → client_not_found?
  ├─ [DB]   check_reference(client_id, ref)
  │           └─ if Some → return already_registered=true; STOP
  │
  ├─ [DB]   find_client_owner_by_id           → client_owner_not_found?
  ├─ [DB]   find_user_payment_info_by_id      → payment_method_not_configured?
  ├─ [Redis→DB] get exchange rate             → exchange_rate_unavailable / _zero?
  ├─ [DB]   find_tax_by_id (default 1.0)
  ├─ [pure] compute missing amount with round2
  │
  ├─ [DB]   find_wa_settings_by_id(workspace) → wa_settings_not_found?
  ├─ [crypto] decrypt access_token            → wa_token_decrypt_failed?
  ├─ [HTTP] WhatsAppService::download_media   → image_download_failed?
  ├─ [FS]   save bytes to uploads/<uuid>.<ext> → image_save_failed?
  │
  ├─ [pure] parse optional debt_id/payment_date
  ├─ [pure] build PaymentReport { id_creator: Some(ai_user_id), state: "Pendiente", ... }
  ├─ [DB]   create_payment_report             → db_error?
  └─ ToolResult::ok({ payment_id, mode: "live", already_registered: false, ... })
        │
        ▼
runner → next Gemini turn with tool result
        │
        ▼
Gemini → text response: "Listo, registramos tu pago de 50 USD..."
        │
        ▼
dispatch.rs → send_live_response → Meta API → customer receives reply
```

---

## 4. Open questions (from input — resolved or flagged)

### Q1: Does a WhatsApp media download helper exist?
**Resolved**. Yes:
- `WhatsAppService::download_media(media_id) -> Result<(Vec<u8>, String, Option<String>)>` at `src/modules/whatsapp/service.rs:335`.
- Handles relay automatically when `with_media_relay(...)` is set.
- Canonical usage pattern at `src/modules/ai_agent/dispatch.rs:1091-1113` (lines that build `WhatsAppService` from `WaSettings`).

### Q2: Are `find_client_owner_by_id` / `find_user_payment_info_by_id` accessible from AI Agent module?
**Resolved**. Yes — both are on `SalesRepository` (`src/db/mod.rs:193, 199`), and `tools.rs` already imports `SalesRepository` (line 22). The master `Db` trait wraps them. Access via `ctx.state.db.find_client_owner_by_id(...)` works as-is.

### Q3: What filename pattern does the existing endpoint use?
**Resolved**. `uploads/{Uuid::new_v4()}.{ext}` (no prefix, no conv_id). Persisted as `image_url = "/uploads/{name}"`. Mirrored exactly in the tool — see ADR-3.

### Q4 (new, surfaced during design): Is `ai_agent_secret()` shared or duplicated?
**Flagged for tasks phase**. Currently duplicated in `dispatch.rs:53` and `sandbox.rs:58`. Recommend making `dispatch::ai_agent_secret` `pub(super)` (or moving to a shared `super::secrets` module) so `tools.rs` doesn't add a third copy. Trivial refactor; not blocking.

### Q5 (new): Does `find_client_by_id` return `Err` or panic on missing?
**Flagged**. The signature is `Result<Client, String>` (no `Option`). Need to confirm in `src/db/mongo/profile.rs` impl that "not found" is `Err(...)` not panic. The tasks phase MUST verify this — if it panics, we add `.find_client_opt_by_id` or guard. **Likely safe** because the existing payments handler relies on it the same way and works in prod, but this is a low-cost check worth doing in apply.

### Q6 (new): Concurrent file naming collision?
**Resolved**. `Uuid::new_v4()` collision probability is astronomically low. The existing endpoint uses the same scheme without locks — no change.

---

## 5. Testing approach

Per project conventions (`CLAUDE.md` — "Never build after changes" and no TDD for this codebase):

**Static**: `cargo check` only. Confirms the code compiles, types align, and imports resolve.

**Manual smoke tests** (post-deploy, in shadow agent first):

| Scenario | Args | Expected |
|---|---|---|
| Happy path live | client_id valid, reference="REF1", media_id valid, amount_usd=50 | `payment_id` non-empty, `already_registered=false`, doc in `PaymentReports` with `idCreator=<ai_user_id>` |
| Idempotency | Same args twice | Second call: `already_registered=true`, no new doc, no Meta download |
| Missing image | media_id="" | `image_required` |
| Both amounts | amount_bs=100, amount_usd=2 | `amount_conflict` |
| Negative amount | amount_usd=-5 | `invalid_amount` |
| Bad client | client_id="aaaa" | `invalid_client_id` |
| Sandbox | ctx.is_sandbox=true, valid args | `mode: "sandbox"`, `payment_id: "sandbox-fake-payment"`, no DB write |

No automated tests added — consistent with the rest of `tools.rs` (none of the existing 8 tools have unit tests).

---

## 6. Affected files (recap)

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

## 7. Architectural risks

| Risk | Severity | Mitigation |
|---|---|---|
| Meta CDN download is slow/fails under load | Med | The relay (Cloudflare Worker) absorbs latency. Tool returns `image_download_failed` cleanly; LLM can retry the call (idempotency check kicks in if reference already saved). |
| `uploads/` directory not writable in some envs | Low | Same risk as the HTTP endpoint — if it works for the dashboard, it works here. Surface as `image_save_failed`. |
| AI calls tool with `media_id` from a stale message | Low | Meta retains inbound media for several days. Failure mode is `image_download_failed` — graceful. |
| `find_client_by_id` returns `Err` on EVERY error not just "not found" | Med | Tasks phase MUST verify the impl distinguishes. Acceptable workaround: log the error string and return `client_not_found` — the LLM doesn't need DB internals. |
| Two AI agents writing to same `(client_id, reference)` concurrently | Low | `check_reference` is the lock. Race window is tiny (DB query → DB insert). Worst case: 2 reports created, accountant reviews and rejects one. Not data corruption. |
| `id_creator` collision with future schema additions | Low | The field name `idCreator` follows existing convention (`Clients`, `WaTicket`). No collision risk. |
