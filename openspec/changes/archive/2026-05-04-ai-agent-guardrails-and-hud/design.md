# Design: AI Agent Guardrails (server-side) + Turn-State HUD — Phase 1

> Status: locked. Implements the proposal in `./proposal.md`. All ADRs below have been pre-decided in the proposal stage; they are recorded here with rationale and rejected alternatives so the implementer can't relitigate them mid-PR.

---

## 1. ADRs

### ADR-1 — Zone validation algorithm: bidirectional normalized substring match

**Decision.** `validate_zone_mentioned(claimed: &str, customer_zones: &[String]) -> bool`:

```rust
let n_claimed = normalize_zone(claimed);
if n_claimed.is_empty() { return false; }
customer_zones.iter().any(|raw| {
    let n_cust = normalize_zone(raw);
    if n_cust.is_empty() { return false; }
    n_claimed.contains(&n_cust) || n_cust.contains(&n_claimed)
})
```

`normalize_zone` already exists in `tools.rs:181` (lowercase, strip accents, trim). Reuse it via `pub(crate)` — no new normalization layer.

**Why bidirectional substring.**
- Customer says "vivo en valencia" → `customer_zones = ["vivo en valencia"]`. AI claims `zone="Valencia"`. Normalized: `"valencia"` is contained in `"vivo en valencia"`. ✅
- Customer says "san diego centro" → AI claims `zone="san diego"`. `"san diego"` ⊂ `"san diego centro"`. ✅
- Customer says "diego" (incomplete) → AI claims `zone="San Diego"`. `"diego"` ⊂ `"san diego"` via the reverse direction. ✅ (acceptable false-pass; tool then resolves the actual zone catalog).
- Customer says "Naguanagua" was NEVER uttered → `customer_zones = ["quiero info del internet"]`. AI hallucinates `zone="Naguanagua"`. `"naguanagua"` ⊄ `"quiero info del internet"` and vice versa. ❌ → guardrail fires.

**Why NOT a static municipality whitelist.**
The proposal is explicit: relying on a Venezuelan-municipality dictionary creates a maintenance treadmill (new sectors, abbreviations, colloquialisms) and false negatives when the customer uses a sector name we don't track. Substring matching against the customer's own raw text is self-calibrating: if the customer wrote it, the AI is allowed to pass it through.

**Rejected alternatives.**
- Levenshtein distance — overkill, requires threshold tuning, adds dependency.
- Token-level Jaccard — over-engineered for a guardrail; the substring check is adequate and trivially auditable.
- Trigram fuzzy match — same as Levenshtein.
- LLM self-check — defeats the point (we're guarding *against* the LLM).

---

### ADR-2 — Compute `customer_explicit_zones` and `recent_media_ids` ONCE per turn in `dispatch.rs`

**Decision.** Compute in `dispatch.rs::handle_inbound_message`, immediately after `recent` is loaded (`dispatch.rs:295-298`), and pass them through `ToolContext`. Tools NEVER recompute.

**Why.** `recent` is already loaded once per turn (single Mongo query). Extracting zones / media_ids is a `O(n)` walk over `recent` (`n ≈ RECENT_WINDOW`, currently small). Recomputing inside each tool would (a) duplicate work if multiple tools fire in the same turn, and (b) require tools to know about `state.db` / message-history concerns — leaks dispatch's responsibility into tool code.

The `recent` slice is also the same one used for the burst-detection logic (lines 305-323) and `is_ai_first_turn_with_prior_history` flag (line 337). Reusing it is consistent with the existing pattern.

**Rejected alternatives.**
- Recompute inside `exec_check_coverage` — would require fetching messages from DB inside the tool. No.
- Pass the entire `&[WaMessage]` slice via `ToolContext` — leaks the WhatsApp model into the tool layer. The pre-extracted `Vec<String>` is the minimal surface.
- Compute lazily on first guardrail check — adds branching for negligible savings; the extraction is microseconds.

---

### ADR-3 — HUD format: `[turn_state]` block, neutral key/value lines

**Decision.** Mirror the existing `[agent_state]` block style. Multi-line, neutral key/value lines, no imperative instructions:

```
[turn_state]
turn_number: 3
customer_explicit_zones: valencia, san diego centro
customer_explicit_intents: payment, billing
```

Rules:
- `turn_number`: always present, `1`-indexed (`history.iter().filter(role == User).count() + 1`).
- `customer_explicit_zones`: comma-separated raw zone strings (already lowercase / no-accent / trimmed). Omit the line entirely when the vec is empty.
- `customer_explicit_intents`: comma-separated GROUP KEYS (see ADR-5). Omit the line entirely when the vec is empty.

**Ordering inside `system_instruction`.** Block goes immediately after `[agent_state]` and before `[faqs]`. Same chunk-vector pattern (`format!("[turn_state]\n{}", body.trim())`). This places the two state HUDs adjacent so the model reads them as a coherent state block.

**Why no imperative text.** The existing `system_instruction` discipline is "back passes DATOS etiquetados; SUPERADMIN decides comportamiento desde `system_prompt`" (see comment at `runner.rs:185`). The HUD respects that contract — it states facts, not orders.

**Rejected alternatives.**
- JSON blob — harder for the SUPERADMIN to reference from `system_prompt`. `[label]\nkey: value` matches the rest.
- Always emit empty fields (`customer_explicit_zones: (none)`) — adds noise to the prompt; the omit-when-empty rule is cheaper.

---

### ADR-4 — Kill switch: `Config` field built from env, NOT serde-defaulted

**Decision.** Add `pub enable_ai_guardrails: bool` to `Config`. Populated in `Config::from_env()` from `ENABLE_AI_GUARDRAILS` env var, defaulting to `true` when unset.

```rust
// in Config struct
pub enable_ai_guardrails: bool,

// in Config::from_env()
enable_ai_guardrails: env::var("ENABLE_AI_GUARDRAILS")
    .map(|v| !matches!(v.trim().to_lowercase().as_str(), "false" | "0" | "no"))
    .unwrap_or(true),
```

**Why NOT `#[serde(default = "...")]`.** The current `Config` is NOT a `Deserialize`-derived struct (see `src/config.rs:4` — plain `#[derive(Debug, Clone)]`). It's hand-built from `env::var` calls. Introducing serde just for this one field would be inconsistent with the rest of the file. The env-only pattern matches every other `Option`/`bool` field already there.

**Why `Config` and not a magic env-read at the call site.** Rest of the project resolves runtime toggles through `state.config.*`. Centralizing the kill switch here keeps the access pattern uniform and makes it discoverable in one place.

**Rollback.** Set `ENABLE_AI_GUARDRAILS=false` and restart the API. Tools resume the pre-change behavior (no zone / no media_id check). No DB migration involved.

**Rejected alternatives.**
- Per-agent toggle in `AiAgent.limits` — overkill for an emergency switch. If we later want fine-grained tuning, that's a separate change.
- Hardcoded `const ENABLE_GUARDRAILS: bool = true;` — defeats the point (kill switch needs to be settable without redeploy).

---

### ADR-5 — Intent keyword extraction: case-insensitive substring vs canonical groups, returns GROUP KEY

**Decision.** Define a static `INTENT_KEYWORDS: &[(&str, &[&str])]` mapping group key → list of trigger substrings. For each inbound message in the recent window, normalize the body (`normalize_zone` reuse — lowercase + strip accents + trim) and check each substring. If ANY substring of group `G` is found in the body, push `G` into the result. The result is `Vec<String>` of unique GROUP KEYS in detection order.

Static table (deterministic, conservative):

```rust
const INTENT_KEYWORDS: &[(&str, &[&str])] = &[
    // payment-related
    ("payment", &["pagar", "pago", "abono", "transferi", "deposit", "comprobante", "referencia"]),
    // billing / debt-related
    ("billing", &["factura", "facturacion", "deuda", "saldo", "cuanto debo", "cuanto sale"]),
    // coverage / sales onboarding
    ("coverage", &["cobertura", "cubren", "llegan", "instalacion", "instalan"]),
    ("plans", &["plan", "planes", "mbps", "megas", "velocidad"]),
    // technical issues
    ("support", &["no tengo internet", "sin internet", "se cayo", "no me anda", "lento", "no funciona", "no carga", "falla", "averia", "problema"]),
    // human escalation
    ("human", &["agente", "humano", "persona", "asesor", "operador", "supervisor"]),
    // billing changes / plan management
    ("plan_change", &["cambiar de plan", "subir plan", "bajar plan", "upgrade", "downgrade"]),
    // account changes
    ("account", &["actualizar", "cambiar datos", "mi correo", "mi telefono", "mi direccion"]),
    // service termination
    ("cancel", &["cancelar", "dar de baja", "retirar"]),
];
```

The trigger strings are stored ALREADY normalized (lowercase, no accents). The matcher applies `normalize_zone` to the message body once and substring-matches.

**Why GROUP KEY (not the matched text).** The model needs a stable categorical signal. "factura", "facturación" and "deuda" all map to the same conversational intent (`billing`); inflating the HUD with raw matches would make the model overfit on surface form. Group keys are a fixed vocabulary the SUPERADMIN can reference from `system_prompt` ("if `customer_explicit_intents` includes `payment`, …").

**Why substrings (not exact words / regex).** A regex `\bpago\b` would miss "pagaré" / "pagaron"; `pag` alone is too greedy ("paginas"). Multi-character substrings like `"pago"`, `"pagar"`, `"abono"` strike the balance. Normalized matching also dodges the accent issue ("línea" vs "linea").

**Tied-order stability.** Iterate `INTENT_KEYWORDS` in declared order; for each group, if any trigger hits in any inbound, push the group key once and break to the next group. Then de-dup across messages. This gives the SUPERADMIN a deterministic, reviewable HUD line.

**Rejected alternatives.**
- LLM-based intent classifier — adds latency, cost, and failure modes. Defeats the purpose of a deterministic HUD.
- Tokenization + lemmatization — overkill for ~50 trigger strings.
- Returning matched substrings — leaks vocabulary noise to the model (already covered above).

---

### ADR-6 — HUD omission on empty turn 1: `Option<String>` from builder, runner skips on `None`

**Decision.** `build_turn_state` returns `Option<String>`:

```rust
pub fn build_turn_state(
    history: &[ConvTurn],
    customer_zones: &[String],
    customer_intents: &[String],
) -> Option<String> {
    let turn_number = history.iter().filter(|t| t.role == ConvRole::User).count() + 1;
    if turn_number == 1 && customer_zones.is_empty() && customer_intents.is_empty() {
        return None;
    }
    let mut lines = vec![format!("turn_number: {}", turn_number)];
    if !customer_zones.is_empty() {
        lines.push(format!("customer_explicit_zones: {}", customer_zones.join(", ")));
    }
    if !customer_intents.is_empty() {
        lines.push(format!("customer_explicit_intents: {}", customer_intents.join(", ")));
    }
    Some(lines.join("\n"))
}
```

Runner injection treats `None` as "skip the chunk" (mirrors how `agent_state: Option<&str>` already behaves at `runner.rs:255-259`).

**Why omit on empty turn 1.** First-turn cold opens (e.g. customer's literal first message is "hola") provide no extractable signal. Injecting a HUD with only `turn_number: 1` adds noise without value and subtly biases the model into thinking the HUD always fires (we want it to be informative when it appears).

**Why include even on turn 1 when zones/intents exist.** Customer's first message is "vivo en valencia, hay cobertura?" → HUD MUST fire on turn 1 (zones populated, intents populated). The omission rule is strictly the all-empty-turn-1 case.

**Why `Option<String>` (owned), not `Option<&str>`.** The string is computed inside `dispatch.rs` and lives in a local; passing as `Option<&str>` to `run_turn` requires an `as_deref()` at the call site, which is the same pattern already used for `agent_state_owned.as_deref()` (line 642). Symmetric.

**Rejected alternatives.**
- Always emit, with empty lines as `(none)` placeholders — see ADR-3, adds noise.
- Return `Result<String, ()>` — meaningless; `Option` already encodes "no HUD".

---

### ADR-7 — Backward compatibility: `ToolContext` constructed in 2 sites, both must mirror

**Decision.** Both call sites (`dispatch.rs:547` and `sandbox.rs:265`) receive the new fields. Sandbox uses `Vec::new()` for both because it has no live conversation history slice.

```rust
// in dispatch.rs construction
customer_explicit_zones,   // local computed above
recent_media_ids,          // local computed above

// in sandbox.rs construction
customer_explicit_zones: Vec::new(),
recent_media_ids: Vec::new(),
```

**Why this matters.** Adding a field to a non-`Default` struct is a compile-time break in Rust. Forgetting the sandbox site fails `cargo check` with a clear error — the type system is the safety net. Documenting both sites here ensures the implementer doesn't push without updating sandbox tests.

**Effect on guardrails when running in sandbox.** With empty `customer_explicit_zones`, ANY `check_coverage` call in sandbox fails with `zone_not_mentioned_by_customer`. Likewise, `report_payment` always fails with `media_id_not_in_conversation`. This is the WRONG behavior for sandbox (where the SUPERADMIN tests the model end-to-end without a real conversation).

**Resolution.** The guardrails are gated on `ctx.state.config.enable_ai_guardrails`. For sandbox runs, the SUPERADMIN sets `is_sandbox: true` already; we ALSO short-circuit the guardrails when `is_sandbox == true`:

```rust
if ctx.state.config.enable_ai_guardrails && !ctx.is_sandbox {
    if !validate_zone_mentioned(&args.zone, &ctx.customer_explicit_zones) {
        return ToolResult::err("zone_not_mentioned_by_customer", started);
    }
}
```

Same pattern for `media_id_not_in_conversation`. This is the SIMPLEST way to keep sandbox tests green without making sandbox aware of the guardrail mechanic.

**Rejected alternatives.**
- Default trait on `ToolContext` — would silently default new fields and let dispatch forget to populate them. Worse than the compile-time break.
- Sandbox-specific `ToolContext` variant — explosion of types for one case.

---

## 2. Code-level design

### 2.1 `src/modules/ai_agent/guardrails.rs` (NEW, ~120 LOC)

Pure helpers, no I/O. Module-private intent table. Tests trivially possible (out of scope for Phase 1 per proposal).

```rust
//! Server-side guardrails para tool calls del AI Agent + bloque [turn_state]
//! del prompt. Pure helpers — sin I/O. Toda la data viene precomputada por
//! `dispatch.rs` desde la `recent` slice del turno.

use crate::models::whatsapp::WaMessage;
use super::runner::{ConvRole, ConvTurn};
use super::tools::normalize_zone;

/// Mapping intent group → trigger substrings. Substrings ya normalizados
/// (lowercase, sin tildes). Modificar con cuidado: cada cambio impacta el
/// HUD que lee Gemini en CADA turno.
const INTENT_KEYWORDS: &[(&str, &[&str])] = &[
    ("payment",      &["pagar", "pago", "abono", "transferi", "deposit", "comprobante", "referencia"]),
    ("billing",      &["factura", "facturacion", "deuda", "saldo", "cuanto debo", "cuanto sale"]),
    ("coverage",     &["cobertura", "cubren", "llegan", "instalacion", "instalan"]),
    ("plans",        &["plan", "planes", "mbps", "megas", "velocidad"]),
    ("support",      &["no tengo internet", "sin internet", "se cayo", "no me anda",
                       "lento", "no funciona", "no carga", "falla", "averia", "problema"]),
    ("human",        &["agente", "humano", "persona", "asesor", "operador", "supervisor"]),
    ("plan_change",  &["cambiar de plan", "subir plan", "bajar plan", "upgrade", "downgrade"]),
    ("account",      &["actualizar", "cambiar datos", "mi correo", "mi telefono", "mi direccion"]),
    ("cancel",       &["cancelar", "dar de baja", "retirar"]),
];

/// Devuelve los bodies normalizados (lowercase + sin tildes + trim) de los
/// mensajes inbound del cliente. Ignora mensajes sin body o vacíos. Cada
/// elemento es el mensaje completo — el matching es substring bidireccional
/// contra la `zone` que mande Gemini (ver `validate_zone_mentioned`).
pub fn extract_customer_explicit_zones(messages: &[WaMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.direction == "in")
        .filter_map(|m| m.body.as_deref())
        .map(|s| normalize_zone(s))
        .filter(|s| !s.is_empty())
        .collect()
}

/// media_ids únicos (en orden de aparición) de los mensajes inbound del
/// cliente con archivo adjunto. La unicidad es defensiva: Meta no debería
/// duplicar pero el dedupe local protege contra retries del webhook.
pub fn extract_recent_media_ids(messages: &[WaMessage]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for m in messages.iter().filter(|m| m.direction == "in") {
        if let Some(mid) = m.media_id.as_deref() {
            let mid = mid.trim();
            if !mid.is_empty() && !seen.iter().any(|s| s == mid) {
                seen.push(mid.to_string());
            }
        }
    }
    seen
}

/// Bidirectional substring match con `normalize_zone`. true si la zona
/// reclamada por la IA está mencionada (literal o como parte de un texto
/// más largo) por el cliente, o viceversa.
pub fn validate_zone_mentioned(claimed_zone: &str, customer_zones: &[String]) -> bool {
    let n_claimed = normalize_zone(claimed_zone);
    if n_claimed.is_empty() { return false; }
    customer_zones.iter().any(|raw| {
        // raw ya viene normalizado desde extract_customer_explicit_zones,
        // pero re-normalizamos por defensiva (función puede ser llamada
        // con datos crudos en otro contexto).
        let n_cust = normalize_zone(raw);
        if n_cust.is_empty() { return false; }
        n_claimed.contains(&n_cust) || n_cust.contains(&n_claimed)
    })
}

/// Scanea bodies de mensajes inbound y devuelve los GROUP KEYS detectados
/// (sin duplicados, en orden estable de declaración del table). Ver
/// `INTENT_KEYWORDS`.
pub fn extract_customer_explicit_intents(messages: &[WaMessage]) -> Vec<String> {
    // Unimos todos los bodies inbound en un buffer normalizado para barrer
    // INTENT_KEYWORDS una sola vez (n_groups × n_triggers) en lugar de
    // n_messages × n_groups × n_triggers.
    let buffer: String = messages
        .iter()
        .filter(|m| m.direction == "in")
        .filter_map(|m| m.body.as_deref())
        .map(|s| normalize_zone(s))
        .collect::<Vec<_>>()
        .join(" ");
    if buffer.is_empty() { return Vec::new(); }

    let mut hits: Vec<String> = Vec::new();
    for (group, triggers) in INTENT_KEYWORDS {
        if triggers.iter().any(|t| buffer.contains(t)) {
            hits.push((*group).to_string());
        }
    }
    hits
}

/// Construye el bloque `[turn_state]` body (sin la cabecera `[turn_state]`,
/// que la pega `runner::build_system_instruction`). Devuelve `None` cuando
/// es turn_number 1 y no hay zones ni intents — evitar inyectar HUD vacío.
pub fn build_turn_state(
    history: &[ConvTurn],
    customer_zones: &[String],
    customer_intents: &[String],
) -> Option<String> {
    let turn_number = history.iter().filter(|t| t.role == ConvRole::User).count() + 1;
    if turn_number == 1 && customer_zones.is_empty() && customer_intents.is_empty() {
        return None;
    }
    let mut lines = vec![format!("turn_number: {}", turn_number)];
    if !customer_zones.is_empty() {
        lines.push(format!("customer_explicit_zones: {}", customer_zones.join(", ")));
    }
    if !customer_intents.is_empty() {
        lines.push(format!("customer_explicit_intents: {}", customer_intents.join(", ")));
    }
    Some(lines.join("\n"))
}
```

**Visibility note.** `normalize_zone` is `pub(crate)` in `tools.rs:181` already; `guardrails.rs` consumes it directly. `ConvRole::User` is `pub` in `runner.rs:92`.

---

### 2.2 `src/modules/ai_agent/mod.rs` — register module

Add a single line:

```rust
pub mod guardrails;
```

Place alphabetically near `pub mod escalation;` / `pub mod gemini;`.

---

### 2.3 `src/modules/ai_agent/tools.rs` — extend `ToolContext` + 2 guardrail blocks

**Field additions** (struct already at lines 46-77):

```rust
pub struct ToolContext {
    // ... existing fields unchanged ...
    pub default_ticket_category_id: Option<String>,

    /// Zonas (textos crudos normalizados) que el cliente mencionó en sus
    /// mensajes inbound recientes. Precomputado en dispatch desde el slice
    /// `recent`. Vacío en sandbox (los guardrails se gatean por
    /// `is_sandbox` antes de leer este campo).
    pub customer_explicit_zones: Vec<String>,

    /// media_ids de mensajes inbound recientes con archivo adjunto.
    /// Precomputado en dispatch. Vacío en sandbox.
    pub recent_media_ids: Vec<String>,
}
```

**Guardrail in `exec_check_coverage`** (insert at line 689, immediately AFTER `parse_args` returns OK and `raw` is computed, BEFORE `if raw.is_empty()`):

```rust
async fn exec_check_coverage(args: Value, ctx: &ToolContext, started: Instant) -> ToolResult {
    let parsed: CheckCoverageArgs = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("invalid_args:{}", e), started),
    };
    let raw = parsed.zone.trim();
    if raw.is_empty() {
        return ToolResult::err("missing_zone", started);
    }

    // ── GUARDRAIL: zona debe haber sido mencionada por el cliente ──────
    if ctx.state.config.enable_ai_guardrails && !ctx.is_sandbox {
        if !crate::modules::ai_agent::guardrails::validate_zone_mentioned(
            raw,
            &ctx.customer_explicit_zones,
        ) {
            return ToolResult::err("zone_not_mentioned_by_customer", started);
        }
    }
    // ── /GUARDRAIL ──────────────────────────────────────────────────────

    let zones = match load_active_zones(ctx).await {
        // ... unchanged ...
```

**Guardrail in `exec_report_payment`** (insert at the location of the existing media_id non-empty check, line 1165, REPLACING that single check):

```rust
// 2. Validate media_id non-empty
if parsed.media_id.trim().is_empty() {
    return ToolResult::err("image_required", started);
}

// 2.b GUARDRAIL: media_id debe ser uno que el cliente haya enviado en
// los mensajes recientes (evita que la IA invente un ID).
if ctx.state.config.enable_ai_guardrails && !ctx.is_sandbox {
    let mid = parsed.media_id.trim();
    if !ctx.recent_media_ids.iter().any(|m| m == mid) {
        return ToolResult::err("media_id_not_in_conversation", started);
    }
}
```

Both guardrails share the same gating pattern: `enable_ai_guardrails && !is_sandbox` (per ADR-7).

---

### 2.4 `src/modules/ai_agent/dispatch.rs` — precompute + pass through

**Step A.** After the `recent` load (line 298) and before the `agent_state_owned` block (line 602), compute the three derived values ONCE at the dispatch top level (NOT inside the `loop chain` — they don't change between chain iterations):

```rust
// (insert after line 298, before line 304's full_history construction)

// ── Guardrail data + turn-state HUD (precomputado por turno) ────────────
// Vive a este nivel porque NO cambia entre iteraciones del chain de
// transfer (zones / media_ids / intents son función del cliente, no del
// agente activo). turn_number también se computa una vez por turno.
let customer_explicit_zones = guardrails::extract_customer_explicit_zones(&recent);
let recent_media_ids = guardrails::extract_recent_media_ids(&recent);
let customer_explicit_intents = guardrails::extract_customer_explicit_intents(&recent);
```

**Step B.** Build the `turn_state_owned` once `full_history` exists (right after line 323):

```rust
// El history que ve el modelo arranca desde full_history (puede recortarse
// adelante por fresh-start). Para turn_number contamos `User` turns en
// ESTE history — que es lo que efectivamente ve Gemini.
let turn_state_owned: Option<String> = guardrails::build_turn_state(
    &full_history,
    &customer_explicit_zones,
    &customer_explicit_intents,
);
```

**Step C.** In the `ToolContext` construction (line 547), add the two new fields. They're moved (not cloned) — the dispatch loop only constructs `tool_ctx` once per chain iteration; the precomputed Vecs are reused across chain iterations via `.clone()`:

```rust
let tool_ctx = ToolContext {
    // ... existing fields ...
    default_ticket_category_id: active_agent.escalation.default_ticket_category_id.clone(),
    customer_explicit_zones: customer_explicit_zones.clone(),
    recent_media_ids: recent_media_ids.clone(),
};
```

**Step D.** In the `run_turn` call (line 629), pass the new arg in the existing positional list. ADR placement: between `agent_state_owned.as_deref()` and `Some(&active_prompt_vars)`:

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
    turn_state_owned.as_deref(),       // ← NEW
    Some(&active_prompt_vars),
    &tool_ctx,
)
```

**Step E.** Add the `use` for `guardrails` at the top of `dispatch.rs`:

```rust
use super::guardrails;
```

(adjacent to existing `use super::escalation;` etc.)

---

### 2.5 `src/modules/ai_agent/runner.rs` — new param + injection

**Signature change.** Add `turn_state: Option<&str>` at line 309, immediately after `agent_state` and before `prompt_vars`:

```rust
pub async fn run_turn(
    http: &reqwest::Client,
    agent: &AiAgent,
    api_key_decrypted: &str,
    relay: Option<&AiRelay>,
    base_url_override: Option<&str>,
    history: &[ConvTurn],
    user_message: &str,
    user_media: &[MediaInput],
    faqs_inline: Option<&str>,
    customer_context: Option<&str>,
    transfer_context: Option<&str>,
    first_turn_note: Option<&str>,
    agent_state: Option<&str>,
    turn_state: Option<&str>,           // ← NEW
    prompt_vars: Option<&PromptVariables>,
    tool_ctx: &ToolContext,
) -> Result<RunnerOutput, ApiError> {
```

**Forwarding.** Pass it through to `build_system_instruction`:

```rust
let system_instruction = build_system_instruction(
    agent,
    faqs_inline,
    customer_context,
    transfer_context,
    first_turn_note,
    agent_state,
    turn_state,                         // ← NEW
    prompt_vars,
);
```

**Builder change.** Add a `turn_state: Option<&str>` param to `build_system_instruction` (line 176) immediately after `agent_state`. Inject the chunk between `[agent_state]` and `[faqs]`:

```rust
fn build_system_instruction(
    agent: &AiAgent,
    faqs_inline: Option<&str>,
    customer_context: Option<&str>,
    transfer_context: Option<&str>,
    first_turn_note: Option<&str>,
    agent_state: Option<&str>,
    turn_state: Option<&str>,           // ← NEW
    vars: Option<&PromptVariables>,
) -> SystemInstruction {
    // ... existing chunks: prompt → personality → customer_context → transfer_context → first_turn_note → agent_state ...

    if let Some(state) = agent_state {
        if !state.trim().is_empty() {
            chunks.push(format!("[agent_state]\n{}", state.trim()));
        }
    }

    // ← NEW chunk, between [agent_state] and [faqs]
    if let Some(ts) = turn_state {
        if !ts.trim().is_empty() {
            chunks.push(format!("[turn_state]\n{}", ts.trim()));
        }
    }

    if let Some(faqs) = faqs_inline {
        // ... unchanged ...
    }

    SystemInstruction { parts: vec![Part::text(chunks.join("\n\n"))] }
}
```

**Cosmetic note.** `run_turn` now has 16 positional params. The proposal's "Out of Scope" explicitly defers a struct-based refactor (`RunTurnArgs`); we accept the cosmetic debt for Phase 1.

---

### 2.6 `src/modules/ai_agent/sandbox.rs` — mirror `ToolContext` + new `run_turn` arg

**Construction (line 265-278).** Add the two new fields with empty Vecs:

```rust
let tool_ctx = ToolContext {
    state: state.clone(),
    workspace_id: workspace_oid,
    business_phone: wa_setting.phone.clone(),
    agent_id: agent_oid,
    conversation_id: None,
    ai_user_id: agent.ai_user_id.clone(),
    ai_user_name: agent.personality.assistant_name.clone(),
    is_sandbox: true,
    allowed_transfer_targets,
    transfer_target_labels,
    agent_snapshot: agent_snapshot.clone(),
    default_ticket_category_id: agent.escalation.default_ticket_category_id.clone(),
    customer_explicit_zones: Vec::new(),    // ← NEW
    recent_media_ids: Vec::new(),           // ← NEW
};
```

**`run_turn` call (line 307).** Add `None` for `turn_state` between `agent_state` (last `None` before `Some(&prompt_vars)`) and `prompt_vars`:

```rust
let output = run_turn(
    &state.reqwest_client,
    &agent,
    &api_key,
    relay,
    endpoint_override,
    &history,
    &message,
    &[],
    faqs_inline.as_deref(),
    None,            // customer_context
    None,            // transfer_context
    None,            // first_turn_note
    None,            // agent_state
    None,            // turn_state          ← NEW
    Some(&prompt_vars),
    &tool_ctx,
)
.await?;
```

(Sandbox doesn't need turn_state. We could compute a synthetic one for SUPERADMIN debugging, but that's polish — not Phase 1.)

---

### 2.7 `src/config.rs` — kill switch

Per ADR-4:

**Struct addition** (after `gemini_base_url`):

```rust
/// Server-side guardrails on AI Agent tool calls. When `true`, blocks:
///   - `check_coverage` calls with zones the customer never mentioned.
///   - `report_payment` calls with media_ids never seen in this conversation.
/// Set `ENABLE_AI_GUARDRAILS=false` (or `0` / `no`) to bypass — emergency
/// kill switch only; production should keep this `true`.
pub enable_ai_guardrails: bool,
```

**`from_env`** (after `gemini_base_url` parsing):

```rust
enable_ai_guardrails: env::var("ENABLE_AI_GUARDRAILS")
    .map(|v| !matches!(v.trim().to_lowercase().as_str(), "false" | "0" | "no"))
    .unwrap_or(true),
```

---

## 3. Sequence flow

```
WhatsApp message arrives
  │
  ▼
dispatch.rs::handle_inbound_message
  │
  ▼
list_recent_messages_for_conversation(&conv_id, RECENT_WINDOW)  ── existing, single Mongo query
  │
  ▼  recent: Vec<WaMessage>
  │
  ├─[NEW]─► guardrails::extract_customer_explicit_zones(&recent) ──► Vec<String>
  ├─[NEW]─► guardrails::extract_recent_media_ids(&recent)         ──► Vec<String>
  ├─[NEW]─► guardrails::extract_customer_explicit_intents(&recent)──► Vec<String>
  │
  ├──────► full_history filtered + ConvTurn vec    ── existing
  │
  ├─[NEW]─► guardrails::build_turn_state(&full_history, &zones, &intents) ──► Option<String>
  │
  ▼
loop chain (transfer chain — typically 1 iteration):
  │
  ├──────► build ToolContext (with NEW: customer_explicit_zones, recent_media_ids)
  │
  ├──────► run_turn(... agent_state, turn_state, prompt_vars, tool_ctx)
  │           │
  │           ▼
  │        build_system_instruction injects chunks in order:
  │           prompt → personality → customer_context → transfer_context
  │           → first_turn_note → [agent_state] → [turn_state] (NEW) → [faqs]
  │           │
  │           ▼
  │        Gemini sees the HUD and decides tool calls
  │           │
  │           ▼
  │        If Gemini → check_coverage(zone="Naguanagua"):
  │           │
  │           ▼
  │        exec_check_coverage:
  │           │
  │           ├── parse_args, trim raw zone
  │           │
  │           ├── if config.enable_ai_guardrails && !ctx.is_sandbox:
  │           │      validate_zone_mentioned("Naguanagua", ctx.customer_explicit_zones)
  │           │       │
  │           │       ├─ false ──► return ToolResult::err("zone_not_mentioned_by_customer")
  │           │       └─ true  ──► continue with original logic (DB lookup)
  │           │
  │           └── ... unchanged downstream ...
  │
  └── (transfer? loop. else: send response, persist AiInteraction.)
```

For `report_payment` the flow is identical, swapping `validate_zone_mentioned` → "is `args.media_id` in `ctx.recent_media_ids`?".

---

## 4. Open questions

None as of this design. All 7 ADRs were resolved in proposal stage or by the implementer's design call (kill-switch storage, sandbox bypass).

Two flags for the implementer to keep in mind (not blockers):

- **`run_turn` param explosion.** 16 positional params. Phase 2 candidate: collapse to a `RunTurnInputs` struct. Out of scope here per proposal "Out of Scope" §3.
- **`turn_state` based on `full_history` vs effective `history`.** The design uses `full_history` (the slice before fresh-start trim) so turn_number reflects the customer's actual conversation length, not what the model sees. If future work changes how `history` is constructed, re-check that turn_number stays "from the customer's perspective" and not "from the prompt's perspective".

---

## 5. Testing approach

`cargo check` is the only mechanized verification (per project rule "never build after changes"). Manual smoke tests:

| # | Setup | Expected |
|---|-------|----------|
| 1 | Send "quiero info del internet" (Naguanagua bug repro). Watch logs. | AI calls `check_coverage(zone="<anything>")` → tool returns `zone_not_mentioned_by_customer`. AI's NEXT turn asks "¿de qué zona nos escribís?" |
| 2 | Send "vivo en Valencia, ¿hay cobertura?" | AI calls `check_coverage(zone="valencia")` → guardrail PASSES → tool runs the catalog lookup. |
| 3 | Send "san diego centro" then ask "tienen cobertura allá" | `check_coverage(zone="san diego")` passes (substring contained in "san diego centro"). |
| 4 | Send a payment with image attached → AI calls `report_payment(media_id=<the actual one>)` | Guardrail passes. Payment registered. |
| 5 | Send "voy a pagar" (no image yet) → AI hallucinates a media_id and calls `report_payment` | Tool returns `media_id_not_in_conversation`. AI's next turn re-asks for the comprobante. |
| 6 | Set `ENABLE_AI_GUARDRAILS=false`, restart API, repro test 1 | Guardrail bypassed, original (buggy) behavior returns. Confirms kill switch works. |
| 7 | Inspect `system_instruction` log on a turn 3 with payment intent + zone | Log shows `[turn_state]\nturn_number: 3\ncustomer_explicit_zones: valencia\ncustomer_explicit_intents: payment` |
| 8 | Sandbox call (Shadow mode) with no real conversation | `check_coverage` and `report_payment` work as before — guardrails skipped because `is_sandbox = true`. |
| 9 | Customer's literal first message is "hola" | `[turn_state]` block is OMITTED (turn_number=1, no zones, no intents). Confirms ADR-6. |

---

## 6. Affected files (recap)

| File | Change | Est. LOC |
|------|--------|---------|
| `src/modules/ai_agent/guardrails.rs` | NEW | +120 |
| `src/modules/ai_agent/mod.rs` | `pub mod guardrails;` | +1 |
| `src/modules/ai_agent/tools.rs` | +2 `ToolContext` fields, +2 guardrail blocks | +25 |
| `src/modules/ai_agent/dispatch.rs` | precompute zones/media_ids/intents/turn_state, `use super::guardrails;`, threaded through `tool_ctx` and `run_turn` | +20 |
| `src/modules/ai_agent/runner.rs` | new `turn_state` param in `run_turn` + `build_system_instruction`, chunk injection | +18 |
| `src/modules/ai_agent/sandbox.rs` | mirror `ToolContext` (2 empty Vecs) + add `None` arg in `run_turn` call | +5 |
| `src/config.rs` | `enable_ai_guardrails: bool` field + env parse | +6 |
| **TOTAL** | | **~195 LOC** |

No new dependencies. No DB migrations. No env-variable required (default `true`). No schema breaks externally — `ToolResult::err` returns a payload Gemini already knows how to read (it gets the error string back as a tool response, then composes a follow-up question to the customer).
