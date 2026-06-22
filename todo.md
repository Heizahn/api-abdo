# TODO

## Reglas de trabajo acordadas

- Trabajar principalmente sobre `src/modules/ai_agent` y documentación relacionada en `docs/agent-tasks` / `openspec/specs/ai-agent`.
- Antes de codear, planificar aquí las tareas y explicar los cambios propuestos.
- Si una solicitud es ambigua o hay varias opciones/rutas posibles, preguntar antes de modificar.
- Hacer el mínimo código necesario para cumplir el resultado.
- Evitar comentarios innecesarios: el código debe explicarse por nombres, tipos y estructura.
- Para cambios funcionales Rust/API/IA/WhatsApp/pagos: versionar, validar con `cargo check`, commit y push salvo instrucción contraria.

## Estado inicial

- No encontré un archivo local `skills.sh`; era la web/CLI `skills.sh`.
- Instalé desde `skills-lock.json` con `npx skills experimental_install`; quedó `rust-best-practices` en `.agents/skills/rust-best-practices`.
- Leí `.agents/skills/rust-best-practices/SKILL.md`; la usaré para cambios Rust.
- El área mencionada por el usuario existe en:
  - `src/modules/ai_agent/`
  - `docs/agent-tasks/ai-agents-payments-routing-plan.md`
  - `openspec/specs/ai-agent/spec.md`

## Plan actual — Fase 1 `purpose` editable en API

Cambios mínimos propuestos:

1. `src/models/ai_agent.rs`
   - Agregar `purpose: Option<AiAgentPurpose>` a `AiAgentItem`.
   - Agregar `purpose: Option<AiAgentPurpose>` a `CreateAiAgentRequest`.
   - Agregar `purpose: Option<AiAgentPurpose>` a `UpdateAiAgentRequest`.

2. `src/modules/ai_agent/handler.rs`
   - Devolver `purpose` en `agent_to_item`.
   - Aplicar `body.purpose` al crear agente.
   - Aplicar `body.purpose` al actualizar agente.

3. Versionado/OpenAPI
   - Subir versión `0.3.93` → `0.3.94` en `Cargo.toml` y `Cargo.lock`.
   - Sincronizar `src/openapi.rs` a `0.3.94`.

4. Validación
   - [x] Ejecutar `cargo fmt`.
   - [x] Ejecutar `cargo check`.
   - [ ] Hacer commit y push a `develop` según regla del proyecto.

Notas:
- No agrego validaciones nuevas porque `AiAgentPurpose` ya está tipado por serde/schema.
- Mantengo compatibilidad: si `purpose` no viene, queda `None`.

## Resultado Fase 1

- `purpose` agregado a respuesta/listado de agentes.
- `purpose` aceptado en create/update.
- Versión subida a `0.3.94` en `Cargo.toml`, `Cargo.lock` y OpenAPI.
- `cargo check` OK.

## Siguiente paso — Fase 2 configuración real

Estado reportado por usuario:
- Sofía ya está configurada como `purpose=recepcionista`.
- Andrea ya está configurada como `purpose=pagos`.

Pendiente antes de pasar a prompts/routing:
- [x] Carla queda tal cual si está desactivada; no forzar `purpose=ventas` ahora.
- [x] Gabriel queda tal cual si está desactivado; no forzar `purpose=soporte` ahora.
- [x] Andrea ya tiene activa la tool `list_banks`.
- [x] Andrea no debería usar `create_ticket`; tickets quedan más para soporte. Para pagos complejos, preferir `request_human`.

Siguiente revisión solicitada:
- [x] Revisar configuración actual de Andrea y su system prompt antes de proponer cambios.
- No tocar código/configuración hasta aprobación explícita.

Plan propuesto para Andrea:
1. Desactivar `create_ticket` en tools de Andrea.
2. Quitar `create_ticket` del bloque `# HERRAMIENTAS` del prompt.
3. Quitar la regla final de `create_ticket` para seguimiento no urgente.
4. Reemplazar esos casos por respuesta informativa o `request_human` si realmente requiere revisión humana.
5. Mantener `list_banks`, `report_payment`, `get_payment_methods`, `get_invoices`, `calculate_amount_bs`, `lookup_customer` y `request_human`.
6. No cambiar código backend por ahora; es ajuste de configuración/prompt.

## Nueva solicitud — IVA por `tax_id` del cliente

Documentado también en `docs/agent-tasks/ai-agents-payments-routing-plan.md` dentro de Fase 2.

Contexto detectado:
- `get_invoices` hoy calcula Bs usando `find_tax_by_id(None)`, que cae al IVA `DEFAULT`.
- `calculate_amount_bs` hoy también usa `find_tax_by_id(None)`.
- `report_payment` tiene comentarios indicando que ya no usa IVA del cliente y usa IVA global/default.
- `lookup_customer` no expone `tax_id`, aunque en `Customers` existe `idTax` y en DTOs de clientes aparece como `tax_id`.

Plan tentativo mínimo, pendiente de confirmación:
1. Resolver el `tax_id` real del cliente seleccionado (`Customers.idTax`) por `client_id`.
2. Usar ese tax en `get_invoices` para que `amount_bs` salga con el IVA del cliente, no DEFAULT.
3. Ajustar `calculate_amount_bs` para poder usar IVA del cliente cuando la conversión sea para un cliente identificado.
4. Revisar si `report_payment` debe usar el IVA del cliente cuando recibe `amount_usd`.
5. Mantener fallback a DEFAULT solo si el cliente no tiene `idTax` o si negocio lo autoriza explícitamente.
6. Actualizar descripciones de tools/prompts donde dicen IVA default/global.

Dudas antes de codear:
- ¿El cambio aplica solo a clientes existentes/cobranzas o también a ventas/lista de planes?
- Si el cliente no tiene `idTax`, ¿usamos DEFAULT o fallamos con error?
- ¿`calculate_amount_bs` debe exigir/aceptar `client_id` para usar su tax?
- ¿`report_payment(amount_usd)` también debe convertir con el tax del cliente?
