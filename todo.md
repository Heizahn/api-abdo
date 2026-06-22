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

Plan mínimo aprobado para codear ahora:
1. [x] Resolver el `tax_id` real del cliente seleccionado (`Customers.idTax`) por `client_id`.
2. [x] Usar ese tax en `get_invoices` para que `amount_bs` salga con el IVA del cliente, no DEFAULT.
3. [x] Ajustar `calculate_amount_bs` para aceptar `client_id` opcional y usar IVA del cliente cuando venga.
4. [x] Ajustar `report_payment` para usar IVA del cliente al derivar montos.
5. [x] Mantener fallback a DEFAULT si el cliente no tiene `idTax`.
6. [x] Actualizar descripciones de tools donde dicen IVA default/global.
7. [x] No tocar ventas/lista de planes en este cambio.
8. [x] Bump version a `0.3.95`, `cargo check` OK.
9. [x] Commit y push (`d3a93e3`).

## Corrección de proceso — saldo sin deudas

Caso observado en logs:
- Cliente pide saldo.
- Andrea llama `get_invoices`.
- Tool devuelve `{ "items": [] }`.
- La respuesta debe decir que está solvente/al día, no “Bs. 0 pendiente”.

Decisión:
- No hardcodear este comportamiento en backend/runner.
- Debe resolverse desde configuración/prompt de Andrea en la UI.
- Se revirtió el commit `4aba528` porque violaba la regla de no codear sin confirmar y movía comportamiento configurable al código.

Pendiente propuesto para prompt Andrea:
- Agregar regla: si `get_invoices` devuelve `items: []`, el cliente no tiene deuda pendiente; responder que está al día/solvente y no decir “Bs. 0 pendiente”.

## Pendientes futuros documentados — Fase 4 / hardening WhatsApp

Problema observado:
- Al abrir un chat atendido por IA, la conversación puede pasar a `in_progress`, dejando de ser atendida por IA en próximos mensajes.

Decisión de diseño pendiente:
- Blindar backend para que `/take` no pueda tomar una conversación IA activa (`status=pending`, `ai_disabled=false`, `ai_active_agent_id` presente).
- La toma manual de una conversación IA debe ser explícita con `/intervene`.
- Confirmar que `GET /messages` no cambia `status` ni pausa IA.

Problema media observado:
- Meta puede devolver `GraphMethodException code=100 subcode=33` para media inexistente/sin permiso.
- Ese caso no debe responder 500; debe mapearse a error estable tipo `404 media_unavailable` y, opcionalmente, cache negativo corto.

Estado:
- Documentado en `docs/agent-tasks/ai-agents-payments-routing-plan.md` dentro de Fase 4.
- Retomado explícitamente por el usuario.

Plan Fase 4 aprobado por el usuario:
1. [x] Blindar `POST /take`: si la conversación está `pending`, `ai_disabled=false` y tiene `ai_active_agent_id`, devolver 409 `ai_active_use_intervene` sin cambiar estado.
2. [x] Mantener `/intervene` como única acción explícita para tomar una conversación atendida por IA.
3. [x] Mapear media no disponible de Meta (`GraphMethodException`, `code=100`, `error_subcode=33`, `does not exist`, `missing permissions`) a error estable `404 media_unavailable`, no 500.
4. [x] Bump versión a `0.3.96`, `cargo fmt` y `cargo check` OK.
5. [ ] Commit y push.

## Observación prueba pagos — comprobante repetido / imagen no usable

Caso observado:
- Tras registrar correctamente un pago, se intentó enviar el mismo pago/comprobante otra vez.
- Andrea no volvió a registrarlo, lo cual es seguro.
- Respuesta actual: “Parece que hubo un problema con la imagen... envíame nuevamente la foto...”.

Lectura:
- No parece bloquear Fase 2 si el pago original se registró bien.
- Pertenece a Fase 3/Fase 7: mejorar manejo de errores de `report_payment` y mensajes de Andrea cuando la imagen/media no está disponible o no pertenece al turno.

Pendiente:
- Revisar logs del segundo intento para saber si fue `media_id_not_in_conversation`, `image_download_failed`, `media_id_stale_turn` u otro error.
- Ajustar prompt para ser más claro: si no puede usar esa imagen, pedir reenvío del comprobante sin ofrecer datos de pago salvo que el cliente los pida.
