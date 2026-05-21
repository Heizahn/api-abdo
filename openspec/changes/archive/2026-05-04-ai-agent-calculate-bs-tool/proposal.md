# Proposal: AI Agent — `calculate_amount_bs` tool (IVA empresarial)

## Intent

Clientes preguntan precios en Bs por WhatsApp (ej. "¿cuánto es 10 USD?"). Hoy el AI Agent no tiene una herramienta determinística para responder cifras concretas y termina inventando o esquivando. Necesitamos exponer un tool que convierta USD → Bs aplicando tasa BCV vigente + IVA 16% del segmento `EMPRESARIAL`, con redondeo a 2 decimales y desglose auditable.

## Scope

### In Scope
- Nuevo tool `calculate_amount_bs` en el registry de `src/modules/ai_agent/tools.rs` (categoría `InfoLookup`).
- Nuevo método de trait `find_tax_by_target(target: &str) -> Result<Option<Tax>, String>` en `ProfileRepository` con implementación MongoDB.
- Lookup con `sTarget == "EMPRESARIAL"` (NO el `DEFAULT` actual).
- Resolución de tasa: Redis (`get_exchange_rate`) → fallback DB (`get_latest_exchange_rate`).
- Output JSON con `amount_usd`, `bcv_rate`, `rate_date`, `iva_factor`, `iva_percent`, `amount_bs_base`, `amount_bs_with_iva`.
- Errores tipados: `invalid_args`, `invalid_amount`, `exchange_rate_unavailable`, `exchange_rate_zero`, `tax_config_missing`.

### Out of Scope
- IVA per-cliente (resolución por `id_tax` del cliente).
- Multi-currency (EUR, COP).
- Tasas históricas (sólo "hoy").
- Formato locale (`Bs. 1.060,82`) — el front formatea.
- Cambios en `system_prompt`, `runner.rs`, `gemini.rs`, `dispatch.rs`.

## Capabilities

### New Capabilities
- None

### Modified Capabilities
- `ai-agent`: amplía el registry de tools agregando `calculate_amount_bs` (InfoLookup) con su contrato I/O y errores. La spec de `ai-agent` debe listar el nuevo tool junto a los existentes.

## Approach

1. Agregar constante `T_CALCULATE_AMOUNT_BS = "calculate_amount_bs"` y registrar entry en `tool_default()` (descripción + JSON schema con `amount_usd: number, required`).
2. Agregar arm en `execute_tool()` que despacha a `exec_calculate_amount_bs` y arm en `tool_category()` → `InfoLookup`.
3. Implementar `exec_calculate_amount_bs(ctx, args)`:
   - Parse `amount_usd: f64`, validar `> 0` (else `invalid_amount`).
   - Tasa: `state.redis.get_exchange_rate()` → si falla/None, `state.db.get_latest_exchange_rate()`. Si ambos fallan → `exchange_rate_unavailable`. Si `rate == 0.0` → `exchange_rate_zero`.
   - IVA: `state.db.find_tax_by_target("EMPRESARIAL")`. Si `None` → `tax_config_missing`.
   - `bs_base = round2(amount_usd * rate)`, `bs_with_iva = round2(amount_usd * rate * tax.iva)`.
   - `iva_percent = round2((tax.iva - 1.0) * 100.0)`.
   - `rate_date = VenezuelaDateTime::now().date_string_venezuela()`.
4. Agregar `find_tax_by_target` al trait `ProfileRepository` (`src/db/mod.rs`) e implementar en `src/db/mongo/profile.rs` con `find_one({ "sTarget": target })`.

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `src/modules/ai_agent/tools.rs` | Modified | Nueva const, entries en `tool_default`/`execute_tool`/`tool_category`, fn `exec_calculate_amount_bs`. |
| `src/db/mod.rs` | Modified | Nuevo método `find_tax_by_target` en trait `ProfileRepository`. |
| `src/db/mongo/profile.rs` | Modified | Implementación MongoDB de `find_tax_by_target`. |
| `openspec/specs/ai-agent/spec.md` | Modified (delta) | Documentar el nuevo tool en el registry. |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Tasa BCV stale (TTL 5 min, cron 1×/día) | Med | Aceptable para quote referencial; la respuesta incluye `rate_date` y el system prompt puede pedir al AI aclarar "tasa BCV de hoy". |
| `sTarget=EMPRESARIAL` hardcoded en código (no config) | Med | Documentar en la descripción del tool; futuro toggle por agente vive en otro change. Si el doc no existe → `tax_config_missing` (error claro). |
| Redondeo único al final (`amount_bs_with_iva`) vs. redondeo intermedio | Low | Calcular `bs_base` y `bs_with_iva` por separado desde el monto USD original (no encadenar redondeos). Usar `(x*100.0).round()/100.0` consistente con handlers `/v1/utils/calculate/*`. |
| AI no usa el tool si SUPERADMIN no lo menciona en el prompt | Low | Esperado — el módulo es prompt-driven. Documentar en notas operativas. |

## Rollback Plan

El tool es opt-in vía toggle SUPERADMIN en la UI de configuración del agente (`tools` array en `AiAgent`). Para desactivar:
1. UI: marcar `calculate_amount_bs` como `enabled=false` en la config del agente afectado.
2. Si se requiere remoción total: revertir el commit que agrega el registry entry y el método de trait. No hay migración de datos involucrada.

## Dependencies

- Documento `BCV.IVA` con `sTarget="EMPRESARIAL"` debe existir en producción antes del rollout. Verificar manualmente con `mongosh` antes de habilitar el tool en agentes.
- Cron BCV (`cron_bcv.rs`) operativo para que `get_exchange_rate` no devuelva stale > 24h.

## Success Criteria

- [ ] `cargo check` pasa sin warnings nuevos.
- [ ] Llamada al tool con `{ "amount_usd": 10 }` retorna JSON con los 7 campos esperados y valores numéricos coherentes (manual smoke en sandbox).
- [ ] Errores devuelven `ToolResult::err` con los códigos especificados (no panic, no `invalid_args` genérico cuando hay causa específica).
- [ ] Sin cambios en respuesta/comportamiento de tools existentes (regresión cero en registry).
- [ ] El tool aparece listado en `tool_default()` y `tool_category()` retorna `InfoLookup`.
