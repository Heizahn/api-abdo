# Delta for AI Agent — Guardrails + Turn-State HUD

## ADDED Requirements

### Requirement 9: check_coverage Zone-Mention Guardrail

The system MUST validate that the claimed `zone` argument was explicitly mentioned
by the customer in recent inbound messages before executing `check_coverage`.
Matching is bidirectional substring, case/diacritic-insensitive (normalized via
`normalize_zone`). When `Config.enable_ai_guardrails = false`, this guardrail MUST
be skipped entirely.

#### Scenario 9.1: Zone mentioned — guardrail passes

- GIVEN a recent customer inbound message contains "Valencia" (case/diacritic-insensitive)
- WHEN `exec_check_coverage` is called with `zone="Valencia"` (or "valencia carabobo")
- THEN the guardrail MUST allow execution and the tool proceeds with the coverage lookup

#### Scenario 9.2: Zone NOT mentioned — guardrail fails

- GIVEN no recent customer inbound message contains the claimed zone (normalized)
- WHEN `exec_check_coverage` is called with `zone="Naguanagua"`
- THEN the tool MUST return `ToolResult::err` with code `zone_not_mentioned_by_customer`
- AND the tool MUST NOT query coverage zones from DB

#### Scenario 9.3: Bidirectional substring match

- GIVEN a customer inbound message contains "San Diego"
- WHEN `exec_check_coverage` is called with `zone="San Diego, Carabobo"`
- THEN the guardrail MUST pass (either direction of substring inclusion satisfies the check)

#### Scenario 9.4: Empty customer zones — guardrail fails

- GIVEN `customer_explicit_zones` is empty (customer mentioned no place name in `recent`)
- WHEN `exec_check_coverage` is called with any `zone`
- THEN the tool MUST return `ToolResult::err` with code `zone_not_mentioned_by_customer`

#### Scenario 9.5: Kill switch disables guardrail

- GIVEN `Config.enable_ai_guardrails = false`
- WHEN `exec_check_coverage` is called regardless of `customer_explicit_zones`
- THEN the guardrail MUST be skipped and the tool runs as before this change
- AND `tracing::warn!` SHOULD log "guardrails disabled via config" at startup or first tool call

---

### Requirement 10: report_payment Media-ID-in-Conversation Guardrail

The system MUST validate that the `media_id` argument was present in the
recent inbound media of the current conversation before executing
`exec_report_payment`. When `Config.enable_ai_guardrails = false`, this
guardrail MUST be skipped.

#### Scenario 10.1: media_id present in recent — guardrail passes

- GIVEN `ctx.recent_media_ids` contains the claimed `media_id`
- WHEN `exec_report_payment` is called
- THEN the guardrail MUST allow execution and the tool proceeds normally

#### Scenario 10.2: media_id NOT in recent — guardrail fails

- GIVEN `ctx.recent_media_ids` does NOT contain the claimed `media_id`
- WHEN `exec_report_payment` is called
- THEN the tool MUST return `ToolResult::err` with code `media_id_not_in_conversation`
- AND the tool MUST NOT download or insert anything

#### Scenario 10.3: Kill switch disables guardrail

- GIVEN `Config.enable_ai_guardrails = false`
- WHEN `exec_report_payment` is called regardless of `recent_media_ids`
- THEN the guardrail MUST be skipped

---

### Requirement 11: Turn-State HUD Block

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

#### Scenario 11.1: HUD injected when meaningful

- GIVEN `turn_number = 3`, `customer_explicit_zones = ["Valencia"]`, `customer_explicit_intents = ["internet"]`
- WHEN `build_system_instruction` runs
- THEN the system instruction MUST include a `[turn_state]` block with:
  ```
  turn_number: 3
  customer_explicit_zones: [Valencia]
  customer_explicit_intents: [internet]
  ```
- AND the block MUST appear after `[customer_lookup_by_phone]` and before `[faqs]`

#### Scenario 11.2: HUD omitted when all values are baseline

- GIVEN `turn_number = 1`, `customer_explicit_zones = []`, `customer_explicit_intents = []`
- WHEN `build_system_instruction` runs
- THEN the `[turn_state]` block MAY be omitted

#### Scenario 11.3: HUD does not duplicate already_greeted

- GIVEN the `[agent_state]` block already contains `already_greeted`
- WHEN `build_system_instruction` runs
- THEN the `[turn_state]` block MUST NOT contain an `already_greeted` field

---

### Requirement 12: ToolContext Field Additions

`ToolContext` MUST gain two new fields: `customer_explicit_zones: Vec<String>` and
`recent_media_ids: Vec<String>`. Sandbox runs MUST initialize both as `Vec::new()`.

#### Scenario 12.1: Sandbox initializes new fields as empty

- GIVEN `sandbox.rs` constructs a `ToolContext` for a sandbox run
- WHEN the new fields are added
- THEN both `customer_explicit_zones` and `recent_media_ids` MUST be initialized to `Vec::new()`
- AND `cargo check` MUST pass with no new errors or warnings

#### Scenario 12.2: Existing tools are unaffected

- GIVEN tools `lookup_customer`, `calculate_amount_bs`, `get_invoices`, `list_plans`
- WHEN they execute after this change
- THEN they MUST behave identically to pre-change behavior (they do not read the new fields)

---

### Requirement 13: Customer Intent Extraction — Keyword Set v1

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

#### Scenario 13.1: Multiple intents matched

- GIVEN a customer message "cuánto vale el plan de internet"
- WHEN intent extraction runs
- THEN `customer_explicit_intents` MUST contain `["internet", "precio", "planes"]` (order follows declaration in `INTENT_KEYWORDS` table; uniqueness preserved)

#### Scenario 13.2: No keyword matched

- GIVEN a customer message "hola buenos días"
- WHEN intent extraction runs
- THEN `customer_explicit_intents` MUST be empty (`[]`)

#### Scenario 13.3: Intent keys in HUD, not raw text

- GIVEN a customer message "quiero contratar, cuánto cuesta"
- WHEN the `[turn_state]` HUD block is built
- THEN `customer_explicit_intents` in the block MUST be `[contratar, precio]`, NOT the raw substrings

---

### Requirement 14: Config Kill Switch

`Config` MUST expose a boolean field `enable_ai_guardrails` (default `true`).
When set to `false`, all guardrail checks in Requirements 9 and 10 MUST be bypassed.

#### Scenario 14.1: Default value is true (guardrails active)

- GIVEN the environment does not set any guardrail override
- WHEN the server starts
- THEN `Config.enable_ai_guardrails` MUST be `true`

#### Scenario 14.2: Set to false bypasses all guardrails

- GIVEN `Config.enable_ai_guardrails = false`
- WHEN `exec_check_coverage` or `exec_report_payment` executes
- THEN both guardrails (Requirements 9 and 10) MUST be skipped
- AND the tools run as if the guardrail logic does not exist
