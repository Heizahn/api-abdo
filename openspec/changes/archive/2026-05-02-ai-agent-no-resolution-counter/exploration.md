# Exploration: ai-agent-no-resolution-counter

## Current State

### Where `no_resolution_count` lives
**No es un campo de `WaConversations`.** Es una key de Redis:
- Key pattern: `ai_agent:no_resolution:{conv_id}`
- `src/cache/redis_client.rs:478-504` — única operación expuesta es `incr_ai_no_resolution` (increment-only)
- No existe `get_ai_no_resolution` independiente — el counter se lee solo como retorno del increment

### Where `max_turns_without_resolution` is defined
- `src/models/ai_agent.rs:142` — campo en `AiEscalationRules`, embebido en `AiAgent`
- Tipo `u32`, sin default en compile-time — viene de MongoDB
- Valores actuales según el reporte del usuario: 3-4 por agente

### Where the increment / skip logic lives
`src/modules/ai_agent/dispatch.rs:871-910` — función `run_dispatch`:

```rust
let any_tool_success = last_output.tool_calls.iter().any(|t| t.success);
let resolved_now =
    had_chain_transfer || cross_workspace_message.is_some() || any_tool_success;
if last_agent.escalation.max_turns_without_resolution > 0 && !resolved_now {
    let nr = state.redis.incr_ai_no_resolution(&conv_hex).await;
    let cap = last_agent.escalation.max_turns_without_resolution as i64;
    tracing::info!(
        "[ai_agent.dispatch] no_resolution counter (conv={}, count={}/{}, resolved_now=false)",
        conv_hex, nr, cap
    );
    if nr >= cap { /* auto_escalate */ }
}
```

### What triggers reset vs increment — confirmado

| Condition | Counter behavior |
|-----------|------------------|
| Turn con cualquier tool call con `success=true` | **Skip increment** (no decrementa, no resetea) |
| Turn con tool call con `success=false` solamente | Incrementa |
| Turn con cero tool calls (texto puro) | Incrementa |
| `transfer_to_agent` con success | **Reset completo** via `clear_ai_conv_counters` |
| Escalation fires (`auto_escalate`) | Counter borrado (ya escaló — tarde) |

### Critical gotcha — skip ≠ reset
El counter **nunca se resetea** en un turn con tool exitoso, **solo se saltea el increment**. Secuencia [no-tool, no-tool, tool-success, no-tool, no-tool] con `cap=4` produce 1, 2, 2, 3, 4 → escala en el turn 5. El tool success "pausa" la acumulación, no la deshace.

Reset completo solo ocurre via `clear_ai_conv_counters`, que se llama en:
- `src/modules/ai_agent/tools.rs:765` — cuando `transfer_to_agent` persiste OK
- `src/modules/ai_agent/escalation.rs:116` — cuando ya está escalando (tarde)
- `src/modules/ai_agent/dispatch.rs:349` — fresh-start detection (primer AI turn en conv con historia humana previa)

### El caso Carla (Ventas) — explicado
Carla saluda (no tool → +1), pregunta "¿en qué zona vivís?" (no tool → +2), sigue calificando (no tool → +3 o +4). Con `max_turns_without_resolution=4`, escala al 4to turn de texto puro. Turns conversacionales legítimos de calificación — que son intencionalmente sin tool — se penalizan idéntico que un agente repitiendo la misma respuesta inútil.

### `transfer_to_agent` y reset
**Sí resetea**, en `tools.rs:757-766`. Sutileza: el reset ocurre dentro del path de persistencia DB del tool. Si la escritura DB falla (`Err(e)` en `tools.rs:753`), retorna `ToolResult::err` antes del reset. En ese fallo, **el counter NO se resetea**.

### `ai_disabled=true` side-effects
`src/modules/ai_agent/escalation.rs:67-143` (función `auto_escalate`):
1. `update_conversation_ai_state` → `ai_disabled=true`, limpia `ai_active_agent_id` + `ai_transfer_context`
2. `assign_conversation(None)` → libera asignación
3. `record_conversation_event` → entrada `event_type=ai_handoff` en timeline
4. `clear_ai_conv_counters` → borra las 3 Redis keys
5. Opcional: envía texto `farewell_to_human` via Meta Cloud API (live mode)
6. Broadcast WS event `IaPausada` a todos los agentes conectados

### Logging actual
- `dispatch.rs:891-893` — `tracing::info!` con `count={}/{}, resolved_now=false`
- **Solo loguea cuando incrementa.** Cuando `resolved_now=true` no hay log — no existe línea "skipped increment because tool was called"

## Affected Areas

- `src/modules/ai_agent/dispatch.rs:871-910` — toda la lógica de increment/skip/escalate vive en `run_dispatch`
- `src/models/ai_agent.rs:142` — campo `AiEscalationRules.max_turns_without_resolution`
- `src/cache/redis_client.rs:478-504` — `incr_ai_no_resolution` y `clear_ai_conv_counters`
- `src/modules/ai_agent/escalation.rs:67-143` — `auto_escalate` y side-effects
- `src/modules/ai_agent/tools.rs:757-766` — reset on `transfer_to_agent`

## Approaches

1. **A: Reset on any successful tool call** — Cuando `resolved_now=true`, llamar a un nuevo `redis.reset_ai_no_resolution` que zerea solo esa key (no las otras 2 que limpia `clear_ai_conv_counters`).
   - Pros: refleja la intención del usuario ("agente avanzó → empezamos de cero"). Cambio de pocas líneas en `dispatch.rs`.
   - Cons: permite que un agente llame un tool barato cada N turns y resetee perpetuamente. Vector de abuso menor.
   - Effort: Low

2. **B: No incrementar si la respuesta termina en `?`** — Trim del response_text y check de sufijo.
   - Pros: ataca directo el caso Carla. Sin config nueva.
   - Cons: heurística frágil. Multilingüe ("¿?" en español), trailing whitespace, preguntas mid-sentence sin `?` final, falso positivo cuando texto inútil termina con `?`.
   - Effort: Low pero poco confiable

3. **C: Qualification window** — Si `prior_ai_turns < qualification_window_turns`, skip counter. `prior_ai_turns` ya se computa en dispatch.
   - Pros: mapea directo al escenario Carla. Detección simple.
   - Cons: requiere campo nuevo en `AiEscalationRules` (o constante hardcodeada). No ayuda en conversaciones largas donde el agente recae en preguntas.
   - Effort: Low-Medium

4. **A + B + F (pragmatic)** — Reset on tool success + skip si termina con `?` + transfer ya resetea.
   - Pros: poco código, cubre el bug real.
   - Cons: heurística de `?` sigue siendo frágil en edge cases.
   - Effort: Low
