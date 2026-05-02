# Proposal: AI Agent — Fix no_resolution counter false escalations

## Why

El counter `no_resolution` actual escala agentes legítimos durante turnos de calificación inicial. El caso reproducible es **Carla** (ventas, `max_turns_without_resolution=4`): saluda → pregunta zona → pregunta plan → pregunta dirección — todos turnos sin tool por DISEÑO de un flujo de ventas. Al cuarto turno el counter llega al cap y `auto_escalate` dispara: `ai_disabled=true`, conversación liberada, evento `IaPausada` broadcasteado. El cliente nunca pidió un humano y el agente estaba haciendo su trabajo.

La metáfora del usuario es exacta: el counter actual es **un velocímetro que solo sube** — el "skip-not-reset" en tool success no deshace el daño acumulado, solo lo pausa. Operacionalmente esto significa que (a) los agentes conversacionales (ventas, soporte de primer nivel) son inutilizables salvo que se baje el cap a costa de perder la red de seguridad real (loops infinitos), y (b) cada falso positivo consume un turno de operador humano para retomar la conversación.

## What changes

**`src/cache/redis_client.rs:478-504`** (Decision 1, Decision 2)
- Agregar método `reset_ai_no_resolution(&self, conv_id: &str)` — `DEL` solo de `ai_agent:no_resolution:{conv_id}`. No toca `turns_conv` ni `id_attempts`. Aditivo, no modifica `clear_ai_conv_counters`.

**`src/models/ai_agent.rs:142`** (Decision 2)
- Agregar `qualification_window_turns: u32` a `AiEscalationRules` con `#[serde(default)]` → default 0 = comportamiento actual. Backwards compatible con docs existentes en MongoDB.

**`src/modules/ai_agent/tools.rs:116-122`** (Decision 1)
- Agregar enum `ToolCategory { InfoLookup, Action }` y función `tool_category(name: &str) -> ToolCategory` que matchea sobre las constantes `T_*` ya declaradas. Default fallback: `InfoLookup` (safe — preserva el comportamiento "skip" actual para tools desconocidas o nuevas no categorizadas).

**`src/modules/ai_agent/dispatch.rs:871-910`** (Decisions 1, 2, 3)
- Reescribir el bloque `max_turns_without_resolution` con tres ramas explícitas:
  1. **Qualification window**: si `prior_ai_turns < last_agent.escalation.qualification_window_turns`, `tracing::debug!` y `return Ok(())` del bloque (no incrementa, no resetea).
  2. **Action tool con success**: si algún `tool_call.success` cae en `ToolCategory::Action`, llamar `state.redis.reset_ai_no_resolution(&conv_hex)` + `tracing::debug!`.
  3. **Solo InfoLookup success o nada útil**: lógica actual — si `!resolved_now` incrementar y posiblemente escalar; si solo había InfoLookup success, skip silencioso (`tracing::debug!`).
- Mantener `tracing::info!` en el path de increment (es el evento de mayor valor diagnóstico). Promover skip/reset a `debug!` para no inflar logs.

**`src/modules/ai_agent/escalation.rs:67-143`** — sin cambios.

**`src/modules/ai_agent/tools.rs:757-766`** — sin cambios. `transfer_to_agent` sigue llamando `clear_ai_conv_counters` (limpia los 3 keys porque el target arranca limpio). El nuevo path Action-tool-reset también dispara para `transfer_to_agent`, pero `clear` ya dejó la key en 0 → idempotente.

## Scope

### In scope
- Nueva categorización `ToolCategory` (InfoLookup vs Action) en `tools.rs`.
- Nuevo campo `qualification_window_turns: u32` en `AiEscalationRules` (default 0).
- Nuevo método `redis.reset_ai_no_resolution` (targeted reset).
- Refactor del bloque de increment/skip/escalate en `dispatch.rs` con 3 ramas explícitas.
- Logs `tracing::debug!` en los paths skip y reset (diagnóstico futuro).
- Tests unitarios cubriendo los 3 escenarios (sección Test plan).

### Out of scope (explicitly NOT this change)
- LLM-judge approach (approach E del exploration).
- Per-agent `progress_tools` whitelist (approach D).
- Heurística de signo `?` al final del texto (approach B).
- Migración automática de `qualification_window_turns` en docs `AiAgent` existentes — el admin lo configura manualmente vía CRUD (las recomendaciones por agente quedan documentadas más abajo).
- Cambios en el schema de `WaConversations`.
- Cambios en la semántica de `clear_ai_conv_counters` o en los side-effects de `auto_escalate`.
- Persistencia del counter en MongoDB — sigue en Redis (TTL 7 días).
- Cambios en API pública / OpenAPI — los nuevos campos son internos del modelo `AiEscalationRules`, ya expuestos por endpoints de CRUD de agentes.

## Approach

**Decisiones 1 + 2 trabajan en planos distintos, conscientemente desacoplados.**

La **Decisión 1 (categorización Action vs InfoLookup)** ataca el riesgo del comentario actual en `dispatch.rs:875-877` que dice "CUALQUIER tool con éxito cuenta como progreso". Eso es demasiado generoso: un `list_plans` o `check_coverage` exitoso es trabajo legítimo pero NO es resolución — el agente sigue conversando, no cerró nada. Por eso `InfoLookup` mantiene el comportamiento actual (skip increment, no reset). Solo los tools que cambian estado o transfieren al humano (`create_ticket`, `request_human`, `transfer_to_agent`) se consideran resolución y resetean el counter via el nuevo `reset_ai_no_resolution` (`src/cache/redis_client.rs`). Esto cierra el vector de abuso "agente llama un tool barato cada N turns y resetea perpetuamente" mencionado en el exploration (Risks §1).

La **Decisión 2 (qualification window)** ataca el caso Carla específicamente. Es ortogonal a la Decisión 1: la categorización no ayuda si los primeros turnos son texto puro por diseño. El campo nuevo `qualification_window_turns` en `AiEscalationRules` (`src/models/ai_agent.rs:142`) hace que en `dispatch.rs:887` (antes del increment) chequeemos `prior_ai_turns < qualification_window_turns` y skipeemos el counter por completo. `prior_ai_turns` ya se computa en `dispatch.rs:331-335` — reusamos el valor existente.

**Interacción con fresh-start detection (`dispatch.rs:325-353`)**: son dos mecanismos independientes. Fresh-start dispara UNA SOLA VEZ cuando `prior_ai_turns == 0 && prior_history_count > 0` y resetea TODOS los counters per-conv via `clear_ai_conv_counters`. La qualification window evalúa en CADA turno mientras `prior_ai_turns < qualification_window_turns` y solo skipea el counter `no_resolution` (no toca `turns_conv` ni `id_attempts`). Una conversación nueva con history humano previo: turno 1 dispara fresh-start (clear all) y la qualification window también skipea el no_resolution; turno 2 ya no dispara fresh-start (`prior_ai_turns == 1`), pero sigue skipeando por window hasta llegar al threshold. Sin doble reset, sin conflicto.

**Logging (Decisión 3)**: el bloque actual solo loguea cuando incrementa. Tras el cambio, el path silencioso (skip por window, skip por InfoLookup, reset por Action) genera `tracing::debug!`. El path de increment mantiene `tracing::info!` (es el evento que disparará alarmas si el counter sube fuera de control). Esto sigue la práctica del proyecto: `info` para hechos operacionales, `debug` para flujo interno.

## Tool categorization (Decision 1)

| Tool name | Constante | Category | Behavior |
|-----------|-----------|----------|----------|
| `lookup_customer` | `T_LOOKUP_CUSTOMER` | InfoLookup | skip increment (sin reset) |
| `list_plans` | `T_LIST_PLANS` | InfoLookup | skip increment (sin reset) |
| `check_coverage` | `T_CHECK_COVERAGE` | InfoLookup | skip increment (sin reset) |
| `get_invoices` | `T_GET_INVOICES` | InfoLookup | skip increment (sin reset) |
| `create_ticket` | `T_CREATE_TICKET` | Action | reset counter via `reset_ai_no_resolution` |
| `request_human` | `T_REQUEST_HUMAN` | Action | reset counter via `reset_ai_no_resolution` |
| `transfer_to_agent` | `T_TRANSFER_AGENT` | Action | reset (ya reseteaba via `clear_ai_conv_counters` desde `tools.rs:763-766`; ahora además entra al path Action-reset del dispatch — idempotente) |

**Default para tools desconocidas / nuevas**: `InfoLookup` (safe default, preserva el comportamiento "skip" actual). Cuando se agregue una tool nueva en `tools.rs`, el commit que la introduce debe categorizarla explícitamente — el match en `tool_category` debe ser exhaustivo sobre las constantes `T_*` o devolver `InfoLookup` con un `tracing::warn!` para tools desconocidas (decisión de implementación; lock-in: el default es `InfoLookup`, no `Action`).

## Recomendaciones de defaults por agente (informativo — NO migrado)

Estos valores los configura el admin manualmente. La migración solo agrega el campo con default 0 (= comportamiento actual idéntico).

| Agente | `qualification_window_turns` recomendado | Razón |
|--------|------------------------------------------|-------|
| Sofía (recepcionista) | 0 | Debe responder rápido y rutear; no califica |
| Carla (ventas) | 4 | Saludo + 3 preguntas de calificación (zona, plan, dirección) |
| Andrea (cobranzas) | 2 | Saludo + identificación inicial |
| Gabriel (soporte) | 3 | Saludo + 2 preguntas de diagnóstico (router/luces/ONU) |

## Migration / Backwards compatibility

- **`qualification_window_turns: u32` con `#[serde(default)]`** → docs `AiAgent` existentes en MongoDB se deserializan con valor 0 → sin cambio observable hasta que el admin edite manualmente.
- **Nuevo método `reset_ai_no_resolution`** es aditivo. `clear_ai_conv_counters` no se modifica.
- **`transfer_to_agent`** sigue llamando `clear_ai_conv_counters` en `tools.rs:763-766`. El path Action-reset del dispatch también disparará `reset_ai_no_resolution` para ese tool. Como `clear` ya dejó la key en 0, el reset adicional es no-op (Redis `DEL` sobre key inexistente es idempotente).
- **No hay migración de datos en MongoDB**.
- **No hay cambios en API pública**. Los endpoints CRUD de `AiAgent` ya serializan/deserializan `AiEscalationRules` completo — el campo nuevo aparece automáticamente en el JSON con valor 0 hasta que se setee.
- **No hay cambios en OpenAPI** más allá del schema autogenerado del modelo.

## Test plan (verifies the fix)

Ubicación sugerida: `src/modules/ai_agent/dispatch.rs` o módulo de tests adyacente. Los tests deben mockear `state.redis` y `state.db` para aislar la lógica de `run_dispatch` (siguiendo el patrón existente del proyecto).

### Scenario A — Carla qualification (the original bug)

- **Setup**: `agent_id=Carla`, `escalation.max_turns_without_resolution=4`, `escalation.qualification_window_turns=4`, history = 5 turnos AI consecutivos solo-texto (cero tool calls).
- **Expected BEFORE fix**: en el turno 4 el counter llega a 4/4 → `auto_escalate` dispara → `ai_disabled=true`.
- **Expected AFTER fix**: turnos 1-4 absorbidos por la qualification window (counter no cambia, queda en 0). En turno 5 (`prior_ai_turns == 4 >= window`), entra al path normal → counter incrementa a 1/4 → no escalation. ✅

### Scenario B — Action tool reset (targeted)

- **Setup**: `max_turns_without_resolution=4`, `qualification_window_turns=0`, history = `[no-tool, no-tool, transfer_to_agent (Action, success), no-tool, no-tool]`.
- **Expected BEFORE fix**: counter = 1, 2, 0 (transfer reset via `clear_ai_conv_counters`), 1, 2 → no escalation.
- **Expected AFTER fix**: mismo outcome final, pero el reset path de Action-tool dispara `reset_ai_no_resolution` además de `clear_ai_conv_counters`. Confirma que el path Action-reset es funcional y no rompe el caso `transfer_to_agent` existente. ✅

### Scenario C — InfoLookup does NOT reset (preserves current intent)

- **Setup**: `max_turns_without_resolution=4`, `qualification_window_turns=0`, history = `[no-tool, no-tool, list_plans (InfoLookup, success), no-tool, no-tool]`.
- **Expected**: counter = 1, 2, 2 (skipped — InfoLookup no resetea), 3, 4 → escala en turno 5. ✅
- Esto confirma que `list_plans` exitoso NO se considera resolución (preserva el risk del exploration §1: agente no puede abusar de tools baratos para resetear perpetuamente).

### Scenario D (sanity) — Sin window, sin tools, escalación normal

- **Setup**: `max_turns_without_resolution=3`, `qualification_window_turns=0`, history = 3 turnos solo-texto.
- **Expected**: counter 1, 2, 3 → escala en turno 3. Confirma que el fix no rompe el path crítico de loops infinitos.

## Risks

- **Tool category es decisión estática**: agregar una tool nueva requiere categorizarla en `tool_category()`. Mitigación: default `InfoLookup` (= comportamiento actual de skip), idealmente con `tracing::warn!` para nombres desconocidos. Documentar en el `mod.rs` del feature que las tools nuevas deben categorizarse en el mismo PR.
- **Qualification window mal seteado puede ocultar loops reales**: si admin pone `qualification_window_turns=999`, el agente nunca escala. Mitigación: documentar las recomendaciones por tipo de agente; default 0 evita sorpresas; validador del CRUD podría capear (ej. ≤ 10) — fuera de scope.
- **Doble path de reset en `transfer_to_agent`**: `clear_ai_conv_counters` (en el tool) + `reset_ai_no_resolution` (en el dispatch post-turn). Ambos son DEL idempotentes; no es un bug pero es redundante. Decisión consciente: no remover el `clear` del tool porque debe correr aunque el chain en memoria falle a mitad (comentario `tools.rs:759-762` lo explica). El reset extra en dispatch es la garantía simétrica del path Action-tool.
- **Redis-only state**: si Redis cae, el counter se pierde y los contadores arrancan en 0 al reconectar. No es un riesgo nuevo — heredado del diseño actual. Documentado en exploration.

## Rollback plan

- Cambios contenidos en 4 archivos: `src/cache/redis_client.rs`, `src/models/ai_agent.rs`, `src/modules/ai_agent/tools.rs`, `src/modules/ai_agent/dispatch.rs`.
- Revert es un `git revert` único.
- No requiere cleanup de DB (Redis keys auto-expiran a los 7 días; campo nuevo en MongoDB tiene default 0 — al revertir los structs simplemente lo ignoran via `#[serde(default)]` si quedaran docs con el campo seteado).
- No requiere coordinación con frontend / clientes de API (campo nuevo en `AiEscalationRules` aparece como `0` por default en respuestas existentes; al revertir, simplemente desaparece del shape).

## Resolved decisions

1. **Default global `qualification_window_turns` al crear agentes**: queda en **0**. Aplica tanto a docs existentes (via `#[serde(default)]`) como a docs nuevos creados por el endpoint POST. El admin lo configura explícitamente según el rol del agente. Razón: backwards-compatible, explícito sobre implícito, no acopla backend a tipos de agente.

   El campo lleva doc-comment con la guía de valores recomendados:
   ```rust
   /// Number of initial AI turns where the `no_resolution_count` counter is bypassed.
   /// Recommended values:
   /// - 0: Receptionist/router agents (must classify and transfer fast)
   /// - 2-3: Payment/billing agents (structured flow)
   /// - 3-4: Technical support (initial diagnostic questions)
   /// - 4-5: Sales agents (qualification window: zone, usage, devices, etc.)
   /// Max: 10.
   #[serde(default)]
   pub qualification_window_turns: u32,
   ```

2. **Validador de rango**: `qualification_window_turns` debe estar en `0..=10`. Validación en el endpoint POST/PUT de creación y edición de `AiAgent`. Si está fuera de rango, retornar `ApiError` con código `qualification_window_turns_out_of_range` (envelope estándar `{ ok: false, error: "<code>" }`, sin campo `message`).

   El valor inválido se loguea via `tracing::warn!` para diagnóstico de operadores. El admin UI conoce el rango válido (0..=10) desde el schema/doc del campo. Una próxima spec dedicada (`api-error-message-field`) extenderá `ApiError` con `message: Option<String>` como cambio cross-cutting; cuando llegue, este endpoint adoptará el mensaje user-facing automáticamente.

   Razón: window > 10 rompe el propósito del feature (counter nunca se activa); validación temprana en el endpoint da error claro al admin; cost de implementación nulo. Mantener el envelope estándar evita acoplar este fix con un cambio cross-cutting de error shape.

## Related

- Exploration: `openspec/changes/ai-agent-no-resolution-counter/exploration.md`
- Existing fresh-start logic: `src/modules/ai_agent/dispatch.rs:325-353`
- Redis schema (incr + clear): `src/cache/redis_client.rs:478-504`
- Tool name constants: `src/modules/ai_agent/tools.rs:116-122`
- `transfer_to_agent` reset path: `src/modules/ai_agent/tools.rs:757-766`
- `auto_escalate` side-effects: `src/modules/ai_agent/escalation.rs:67-143`
