# Proposal: AI Agent — `report_payment` tool

## Intent

Clientes pagan vía pago móvil/transferencia y mandan el comprobante por WhatsApp. Hoy un humano transcribe esos datos al sistema (`PaymentReports`). Queremos que el AI Agent registre el reporte end-to-end: recibe referencia + monto + imagen, calcula USD/Bs con tasa BCV vigente + IVA del cliente, y persiste con audit trail "registrado por IA".

## Scope

### In Scope
- Nuevo tool `report_payment` en `src/modules/ai_agent/tools.rs` (categoría `Action`, resetea `no_resolution_count`).
- Reuso de `SalesRepository::create_payment_report` / `check_reference` / `find_client_by_id` existentes.
- Resolución interna de `id_payment_method` vía `find_client_owner_by_id` → `find_user_payment_info_by_id` (mirror del endpoint `/v1/payments/payment/report`).
- Tasa BCV: Redis → DB fallback. IVA: `find_tax_by_id(client.id_tax)` con default 1.0.
- Idempotencia vía `check_reference(client_id, reference)` antes del insert: si existe, retorna éxito con flag `already_registered=true`.
- Schema change: agregar `id_creator: Option<String>` (`idCreator`) a `PaymentReport` con `#[serde(default)]`.
- Sandbox mode: si `ctx.is_sandbox`, retorna respuesta sintética sin escribir DB.
- Errores tipados: `invalid_args`, `invalid_client_id`, `client_not_found`, `image_required`, `amount_required`, `amount_conflict`, `invalid_amount`, `payment_method_not_configured`, `exchange_rate_unavailable`, `exchange_rate_zero`, `db_error`.

### Out of Scope
- Pago de múltiples deudas en una sola call.
- Edición / anulación / refund de un report.
- Validación de bancos contra una whitelist (campo `bank` se acepta tal cual).
- Refactor del endpoint HTTP `POST /v1/payments/payment/report` (queda intacto).
- Re-upload de la imagen a storage propio (se persiste la URL/path tal cual la pase el AI).
- OCR / parsing automático del comprobante.

## Capabilities

### New Capabilities
- None

### Modified Capabilities
- `ai-agent`: amplía el registry agregando `report_payment` (categoría `Action`) con su contrato I/O y errores. La spec debe listar el nuevo tool junto a los existentes y documentar el comportamiento de idempotencia + sandbox.

## Approach

1. **Schema**: agregar `id_creator: Option<String>` a `PaymentReport` (`src/models/payment.rs`) con `#[serde(rename = "idCreator", skip_serializing_if = "Option::is_none", default)]`.
2. **Tool registry**: const `T_REPORT_PAYMENT`, entry en `tool_default()` con JSON schema (campos detallados abajo) y descripción que instruye al LLM (ver §Tool description).
3. **Categorización**: arm en `tool_category()` → `Action` (resetea no_resolution igual que `create_ticket`).
4. **Dispatch**: arm en `execute_tool()` → `exec_report_payment`.
5. **`exec_report_payment(ctx, args)`**:
   - Parse args: `client_id`, `reference`, `image_url` requeridos; `amount_bs`/`amount_usd` exclusivos pero al menos uno; `bank`, `phone`, `debt_id`, `payment_date` opcionales.
   - Validar `image_url` no vacío/whitespace → `image_required`.
   - Validar exclusividad amounts → `amount_required` / `amount_conflict` / `invalid_amount`.
   - Si `ctx.is_sandbox` → retornar respuesta sintética y salir.
   - `find_client_by_id(client_id)` → `client_not_found`.
   - `check_reference(client_id, reference)` → si existe → retorna `{ ok: true, already_registered: true, payment_id }`.
   - Resolver `id_payment_method` por owner. Si falta → `payment_method_not_configured`.
   - Resolver `exchange_rate` (Redis → DB) y `iva_rate` (`find_tax_by_id(client.id_tax)` con default 1.0).
   - Calcular monto faltante con redondeo a 2 decimales.
   - Construir `PaymentReport` con `state="Pendiente"`, `id_creator=Some(ctx.ai_user_id)`, `created_at=Utc::now()`.
   - `create_payment_report(report)` → `payment_id`.
6. **Tool description (LLM-facing)** — debe incluir:
   - "Llamá `lookup_customer` primero. Si devuelve varios clientes para el mismo teléfono, listá los nombres al cliente y pedile que elija ANTES de llamar este tool."
   - "Pedile al cliente la foto del comprobante por WhatsApp. Sin imagen el tool va a fallar."
   - "Pasá `amount_bs` O `amount_usd`, no ambos."

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `src/modules/ai_agent/tools.rs` | Modified | Nueva const, entries en `tool_default`/`execute_tool`/`tool_category`, fn `exec_report_payment`. |
| `src/models/payment.rs` | Modified | Agregar campo `id_creator: Option<String>` a `PaymentReport`. |
| `src/db/mod.rs` | Verify | Confirmar que `find_client_owner_by_id` + `find_user_payment_info_by_id` están en traits accesibles desde el AI Agent. Si no, exponerlos. |
| `openspec/specs/ai-agent/spec.md` | Modified (delta) | Documentar el nuevo tool en el registry, idempotencia, sandbox y categorización. |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Retry de Gemini genera duplicados | Med | Idempotencia vía `check_reference` antes del insert; segunda call retorna `already_registered=true`. |
| AI elige el client equivocado cuando hay duplicados por teléfono | Med | Tool description fuerza al LLM a confirmar con el cliente; `client_id` REQUIRED (no se puede inferir). System prompt refuerza. |
| `image_url` apunta a Meta CDN y vence | High | OPEN — flag para el design phase. Posible mitigación: re-upload a storage propio o persistir el `media_id` de Meta. |
| Schema migration de `PaymentReport` (nuevo `idCreator`) | Low | `Option<String>` + `#[serde(default)]` → backwards compatible con docs existentes que no tienen el campo. |
| AI confunde `amount_bs` con `amount_usd` | Med | Errores `amount_conflict` / `amount_required` explícitos; description refuerza "uno solo, no ambos"; el cálculo deriva el otro. |
| `find_tax_by_id` con `id_tax` ausente en cliente | Low | Default a `iva_rate=1.0` (sin IVA) — mismo comportamiento que el endpoint actual. |

## Rollback Plan

El tool es opt-in vía toggle SUPERADMIN en la UI de configuración del agente (`tools` array en `AiAgent`). Para desactivar:
1. UI: marcar `report_payment` como `enabled=false` en la config del agente afectado.
2. Si se requiere remoción total: revertir el commit que agrega el registry entry, el campo `id_creator` y la entrada en spec. No hay migración de datos (el campo nuevo es Optional con default).

## Dependencies

- AI synthetic user UUID (`ctx.ai_user_id`) ya existe en el runner y se propaga al ToolContext (verificar en design phase).
- Tasa BCV operativa (cron `cron_bcv.rs`).
- Cliente target con `idTax` válido en producción para IVA correcto (else default 1.0).
- Owner del cliente con `idPaymentMethod` configurado en `Users` collection.

## Open Questions (for design phase)

- ¿Cómo accede la IA al `image_url` desde la conversación? Investigar `runner.rs` / `gemini.rs` — si Gemini "ve" la imagen como bytes o como URL, y si el path persiste.
- ¿La URL de Meta CDN expira? Si sí, ¿persistimos el `media_id` o re-subimos a storage propio antes de guardarla en `PaymentReport.image_url`?
- ¿`ctx.ai_user_id` está disponible en `ToolContext` o hay que propagarlo desde `runner.rs`?

## Success Criteria

- [ ] `cargo check` pasa sin warnings nuevos.
- [ ] Llamada al tool con args válidos crea un doc en `PaymentReports` con `idCreator = ai_user_id`, `sState="Pendiente"`, `nAmountUSD` y `nBs` coherentes.
- [ ] Segunda llamada con mismo `(client_id, reference)` retorna `already_registered=true` sin crear duplicado.
- [ ] `ctx.is_sandbox=true` retorna respuesta sintética sin tocar DB.
- [ ] Errores devuelven `ToolResult::err` con los códigos especificados (no panic, no `invalid_args` genérico).
- [ ] El tool aparece en `tool_default()`, `tool_category()` retorna `Action`, y un éxito resetea `no_resolution_count`.
- [ ] Schema change a `PaymentReport` no rompe lectura de docs existentes (sin `idCreator`).
