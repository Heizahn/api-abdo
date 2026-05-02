# Design: AI Agent — no_resolution counter fix

## Architecture overview

El cambio físicamente toca **5 archivos** y NO crea módulos nuevos. Toda la lógica vive ya en el módulo `ai_agent`; solo refinamos cómo `dispatch.rs` decide entre incrementar / saltear / resetear el counter `ai_agent:no_resolution:{conv_id}` en Redis. Las decisiones (qualification window, categorización de tools) se materializan como datos: un campo nuevo en `AiEscalationRules` y un enum + función pura en `tools.rs`.

| Layer | File | Cambio |
|-------|------|--------|
| Cache (Redis) | `src/cache/redis_client.rs` | + `reset_ai_no_resolution(conv_id)` (DEL targeted) |
| Domain model | `src/models/ai_agent.rs` | + campo `qualification_window_turns: u32` en `AiEscalationRules`, `AiEscalationRulesDto`, `AiEscalationRulesInput` (+ propagación en From impl) |
| Tool registry | `src/modules/ai_agent/tools.rs` | + enum `ToolCategory`, + fn `tool_category(&str) -> ToolCategory` |
| Dispatch loop | `src/modules/ai_agent/dispatch.rs` | refactor del bloque `max_turns_without_resolution` (líneas 871-910) en 5 ramas explícitas |
| CRUD handler | `src/modules/ai_agent/handler.rs` | + validador de rango `0..=10` en create + update (rama común vía `apply_escalation` o helper post-apply) |

**No se tocan**:
- `src/modules/ai_agent/escalation.rs` (`auto_escalate` y side-effects)
- `src/modules/ai_agent/tools.rs:759-766` (path de reset de `transfer_to_agent` vía `clear_ai_conv_counters`)
- `src/cache/redis_client.rs:491-504` (`clear_ai_conv_counters`, semántica intacta)
- Schema de `WaConversations` ni de `AiAgents`
- `src/openapi.rs` (la spec se actualiza sola via `ToSchema`; sólo el bloque `responses(...)` de los handlers afectados gana un 400 nuevo)

**Constants confirmadas** (`src/modules/ai_agent/tools.rs:116-122`):
```
T_LOOKUP_CUSTOMER = "lookup_customer"   // line 116
T_GET_INVOICES    = "get_invoices"      // line 117
T_REQUEST_HUMAN   = "request_human"     // line 118
T_CREATE_TICKET   = "create_ticket"     // line 119
T_TRANSFER_AGENT  = "transfer_to_agent" // line 120
T_LIST_PLANS      = "list_plans"        // line 121
T_CHECK_COVERAGE  = "check_coverage"    // line 122
```

**Struct fields confirmadas** (`dispatch.rs`):
- `last_output: RunnerOutput` — declarado en `dispatch.rs:807` (`let last_output = last_output.expect(...)`)
- `last_output.tool_calls: Vec<AiToolCallLog>` — el campo del log es **`tool_name`** (no `name`), confirmado en `src/models/ai_agent.rs:241-249`
- `had_chain_transfer: bool` — declarado en `dispatch.rs:841`
- `cross_workspace_message: Option<String>` — declarado en `dispatch.rs:509`, asignado en `dispatch.rs:724`
- `prior_ai_turns: i64` (o `u64` — viene de `count_ai_interactions_for_conversation`) — declarado en `dispatch.rs:331-335`
- `conv_hex: String` — declarado dos veces en el flujo: `dispatch.rs:74` (early stage) y `dispatch.rs:404` (post-burst). En el bloque de `max_turns_without_resolution` (línea 871+) se usa el de la línea 404.

## Sequence diagrams

### Diagram 1: Current behavior (the bug — caso Carla)

`max_turns_without_resolution = 4`, `qualification_window_turns` no existe (campo nuevo).

```
turno  inbound                          tool_calls    counter Δ   counter   action
─────  ───────────────────────────────  ────────────  ─────────   ───────   ──────────────────
  1    "hola"                           []            +1          1/4       incr (texto puro)
  2    "vivo en Caballito"              []            +1          2/4       incr (texto puro)
  3    "necesito el plan básico"        []            +1          3/4       incr (texto puro)
  4    "para mi casa"                   []            +1          4/4       incr → cap → AUTO_ESCALATE
                                                                            ai_disabled=true
                                                                            broadcast IaPausada
                                                                            ❌ FALSO POSITIVO
```

El agente Carla hizo su trabajo (calificar) pero igual escaló. El counter es un velocímetro one-way: tools exitosos lo pausan, no lo resetean (`exploration §"Critical gotcha — skip ≠ reset"`).

### Diagram 2: After fix (qualification_window_turns = 4)

```
turno  inbound                          prior_ai_turns  branch matched         counter   action
─────  ───────────────────────────────  ──────────────  ────────────────────   ───────   ──────────
  1    "hola"                                 0          B1 (window: 0<4)        0/4      skip + debug
  2    "vivo en Caballito"                    1          B1 (window: 1<4)        0/4      skip + debug
  3    "necesito el plan básico"              2          B1 (window: 2<4)        0/4      skip + debug
  4    "para mi casa"                         3          B1 (window: 3<4)        0/4      skip + debug
  5    "para 2 personas"                      4          B5 (incr — texto)       1/4      incr
  6    cliente da datos identificación        5          (lookup_customer ok)             B4 (InfoLookup) → skip
                                                                                 1/4
  7    "perfecto, agendamos visita?"          6          B5 (incr — texto)       2/4      incr
  ...                                                                            (sigue normal)
```

La window absorbe los 4 primeros turnos. A partir del 5, evaluación normal — el counter sí puede escalar si el agente realmente se queda en loop infinito de texto.

### Diagram 3: Tool category routing (post-turn dispatch)

```
                  ┌─────────────────────────────────────────────────┐
                  │ post-turn: max_turns_without_resolution > 0 ?   │
                  └─────────────────────────────────────────────────┘
                                       │ no
                                       └──► return (feature disabled)
                                       │ yes
                                       ▼
                  ┌─────────────────────────────────────────────────┐
                  │ B1: prior_ai_turns < qualification_window_turns │
                  └─────────────────────────────────────────────────┘
                          yes │                       │ no
                              ▼                       ▼
                       debug! "skip:               ┌─────────────────────────────┐
                       qualification_window"      │ B2: any tool_call.success   │
                       return                     │     && Action category ?    │
                                                  └─────────────────────────────┘
                                                       yes │           │ no
                                                           ▼           ▼
                                             reset_ai_no_resolution    │
                                             debug! "reset: Action"    │
                                             return                    │
                                                                       ▼
                                              ┌────────────────────────────────────┐
                                              │ B3: had_chain_transfer ||          │
                                              │     cross_workspace_message.some() │
                                              └────────────────────────────────────┘
                                                       yes │           │ no
                                                           ▼           ▼
                                                 debug! "skip:         │
                                                 chain_transfer"       │
                                                 return                ▼
                                              ┌────────────────────────────────────┐
                                              │ B4: any tool_call.success          │
                                              │     (será InfoLookup por defecto)  │
                                              └────────────────────────────────────┘
                                                       yes │           │ no
                                                           ▼           ▼
                                                 debug! "skip:         │
                                                 InfoLookup"           │
                                                 return                ▼
                                                          ┌──────────────────────┐
                                                          │ B5: incr counter     │
                                                          │  info!  log existente│
                                                          │  if nr >= cap →      │
                                                          │  auto_escalate       │
                                                          └──────────────────────┘
```

Las 5 ramas son mutuamente exclusivas (cada una `return`s). El orden importa: `window` antes de `Action-reset` (para que un tool exitoso DURANTE la window no haga un DEL inútil — la key ya está sin tocar). `Action` antes de `chain_transfer` para evitar que un transfer "robe" un reset legítimo. `chain_transfer` antes de `InfoLookup` para que el caso "tool failed pero hubo transfer en cadena" caiga en la rama correcta. `InfoLookup` antes de `incr` para preservar la intención del comentario actual de `dispatch.rs:875-877`.

## Detailed component design

### 1. `src/cache/redis_client.rs` — nuevo método `reset_ai_no_resolution`

Insertar **después** de `incr_ai_no_resolution` (línea 487) y **antes** de `clear_ai_conv_counters` (línea 491). Patrón calcado de `incr_ai_no_resolution`:

```rust
/// Reset targeted del counter de no-resolución para una conversación.
/// Sólo borra la key `ai_agent:no_resolution:{conv_id}` — NO toca
/// `turns_conv` ni `id_attempts` (esos los limpia `clear_ai_conv_counters`
/// en eventos terminales como auto_escalate o close/reopen).
///
/// Idempotente: DEL sobre key inexistente es no-op silencioso. Failure
/// handling: best-effort, igual que `incr_ai_no_resolution` (si Redis
/// está caído, el counter "se pierde" pero la conv sigue funcionando).
pub async fn reset_ai_no_resolution(&self, conv_id: &str) {
    let mut conn = match self.client.get_multiplexed_async_connection().await {
        Ok(c) => c,
        Err(_) => return,
    };
    let key = format!("ai_agent:no_resolution:{}", conv_id);
    let _: Result<(), _> = conn.del(&key).await;
}
```

Returns `()` (no `i64`). El caller no necesita el deletion count; el log de la rama B2 es suficiente para diagnóstico.

### 2. `src/models/ai_agent.rs` — extender `AiEscalationRules` + DTO + Input

**A. Struct doméstica** (línea 140-148):

```rust
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiEscalationRules {
    pub keywords: Vec<String>,
    pub max_turns_without_resolution: u32,
+   /// Number of initial AI turns where the `no_resolution_count` counter is bypassed.
+   /// Recommended values:
+   /// - 0: Receptionist/router agents (must classify and transfer fast)
+   /// - 2-3: Payment/billing agents (structured flow)
+   /// - 3-4: Technical support (initial diagnostic questions)
+   /// - 4-5: Sales agents (qualification window: zone, usage, devices, etc.)
+   /// Max: 10.
+   #[serde(default)]
+   pub qualification_window_turns: u32,
    pub max_identification_attempts: u32,
    pub escalate_on_critical_tool_failure: bool,
    pub always_escalate_when_asked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ticket_category_id: Option<String>,
}
```

**B. DTO de salida** (línea 503-525) — la conversión NO es derive automática (hay From impl manual), hay que tocar ambos:

```rust
#[derive(Debug, Serialize, ToSchema)]
pub struct AiEscalationRulesDto {
    pub keywords: Vec<String>,
    pub max_turns_without_resolution: u32,
+   pub qualification_window_turns: u32,
    pub max_identification_attempts: u32,
    pub escalate_on_critical_tool_failure: bool,
    pub always_escalate_when_asked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_ticket_category_id: Option<String>,
}

impl From<AiEscalationRules> for AiEscalationRulesDto {
    fn from(e: AiEscalationRules) -> Self {
        AiEscalationRulesDto {
            keywords: e.keywords,
            max_turns_without_resolution: e.max_turns_without_resolution,
+           qualification_window_turns: e.qualification_window_turns,
            max_identification_attempts: e.max_identification_attempts,
            ...
        }
    }
}
```

**C. Input DTO** (línea 684-698) — agregar campo opcional para el PATCH:

```rust
#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct AiEscalationRulesInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_without_resolution: Option<u32>,
+   #[serde(default, skip_serializing_if = "Option::is_none")]
+   pub qualification_window_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_identification_attempts: Option<u32>,
    ...
}
```

**D. Default en `default_agent`** (`handler.rs:233-240`) — agregar `qualification_window_turns: 0` para mantener back-compat explícito en docs nuevos:

```rust
escalation: AiEscalationRules {
    keywords: vec!["humano".into(), "operador".into(), ...],
    max_turns_without_resolution: 3,
+   qualification_window_turns: 0,
    max_identification_attempts: 2,
    ...
},
```

(Aunque `#[serde(default)]` cubre la deserialización, el seed local debe ser explícito porque construye via struct literal — Rust exige inicializar todos los campos.)

### 3. `src/modules/ai_agent/tools.rs` — `ToolCategory` + `tool_category`

Insertar **después** de las constantes T_* (línea 122), **antes** de `AI_BUSINESS_CACHE_TTL_SECS` (línea 126):

```rust
/// Categoría operativa de un tool, usada por `dispatch.rs` para decidir si un
/// turn cuenta como "resolución" (que reset counter) o sólo como "trabajo en
/// progreso" (skip increment, sin reset).
///
/// **Action**: el tool cambia estado externo o transfiere al humano. Un turn
/// con un Action exitoso resetea `no_resolution_count`.
///
/// **InfoLookup**: el tool consulta info pública o de catálogo. Un turn con
/// sólo InfoLookup exitosos no resetea — el agente aún está conversando.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    InfoLookup,
    Action,
}

/// Mapea el `tool_name` al ToolCategory. Default safe: `InfoLookup` para
/// nombres desconocidos (preserva el comportamiento "skip" actual y emite
/// `warn!` para que el dev categorice la tool nueva).
pub fn tool_category(tool_name: &str) -> ToolCategory {
    match tool_name {
        T_LOOKUP_CUSTOMER
        | T_LIST_PLANS
        | T_CHECK_COVERAGE
        | T_GET_INVOICES => ToolCategory::InfoLookup,

        T_CREATE_TICKET
        | T_REQUEST_HUMAN
        | T_TRANSFER_AGENT => ToolCategory::Action,

        unknown => {
            tracing::warn!(
                "[ai_agent.tools] tool_category called with unknown tool name: {} — defaulting to InfoLookup",
                unknown
            );
            ToolCategory::InfoLookup
        }
    }
}
```

**Match exhaustivo intencionalmente NO usa `_`** — los `T_*` enumerados son la totalidad declarada hoy, y la rama `unknown =>` cubre nombres de runtime (ej. tool nueva no categorizada). Si en el futuro alguien agrega un `T_*` y olvida categorizarlo, este `match` no falla en compile (los `T_*` son `&str`, no variantes de enum) — por eso el `warn!` es la red de seguridad runtime. Documentado en el risks del proposal.

### 4. `src/modules/ai_agent/dispatch.rs` — refactor del bloque (líneas 871-910)

Pseudocódigo (los nombres `last_output`, `last_output.tool_calls`, `t.tool_name`, `t.success`, `prior_ai_turns`, `had_chain_transfer`, `cross_workspace_message`, `conv_hex`, `last_agent` están confirmados contra el archivo actual):

```rust
// ── max_turns_without_resolution ───────────────────────────────────────
// Lógica en 5 ramas explícitas (mutuamente exclusivas, fast-return):
//   B1  qualification_window  → skip + debug
//   B2  Action tool success    → reset + debug
//   B3  chain transfer         → skip + debug
//   B4  InfoLookup success     → skip + debug
//   B5  no useful tool         → incr (+ posible auto_escalate)
let cap = last_agent.escalation.max_turns_without_resolution as i64;
if cap > 0 {
    // B1: qualification window
    let window = last_agent.escalation.qualification_window_turns as u64;
    if (prior_ai_turns as u64) < window {
        tracing::debug!(
            "[ai_agent.dispatch] no_resolution skipped (conv={}, reason=qualification_window, prior_ai_turns={}/{})",
            conv_hex, prior_ai_turns, window
        );
    } else {
        // B2: Action tool con success → reset
        let action_success = last_output.tool_calls.iter().find(|t| {
            t.success
                && crate::modules::ai_agent::tools::tool_category(&t.tool_name)
                    == crate::modules::ai_agent::tools::ToolCategory::Action
        });
        if let Some(t) = action_success {
            state.redis.reset_ai_no_resolution(&conv_hex).await;
            tracing::debug!(
                "[ai_agent.dispatch] no_resolution reset (conv={}, tool={}, category=Action)",
                conv_hex, t.tool_name
            );
        } else if had_chain_transfer || cross_workspace_message.is_some() {
            // B3: transfer en chain (cross-workspace o same-workspace via chain)
            tracing::debug!(
                "[ai_agent.dispatch] no_resolution skipped (conv={}, reason=chain_transfer)",
                conv_hex
            );
        } else {
            // B4: InfoLookup con success → skip silencioso (sin reset)
            let any_success = last_output.tool_calls.iter().find(|t| t.success);
            if let Some(t) = any_success {
                tracing::debug!(
                    "[ai_agent.dispatch] no_resolution skipped (conv={}, tool={}, category=InfoLookup)",
                    conv_hex, t.tool_name
                );
            } else {
                // B5: nada útil pasó → incr + posible escalate (path actual)
                let nr = state.redis.incr_ai_no_resolution(&conv_hex).await;
                tracing::info!(
                    "[ai_agent.dispatch] no_resolution counter (conv={}, count={}/{}, resolved_now=false)",
                    conv_hex, nr, cap
                );
                if nr >= cap {
                    tracing::info!(
                        "[ai_agent.dispatch] max_turns_without_resolution ({}) reached (conv={})",
                        nr, conv_hex
                    );
                    escalation::auto_escalate(
                        &state,
                        &inbound.conversation_id,
                        &last_agent,
                        escalation::REASON_NO_RESOLUTION,
                        Some("Caso sin resolver tras varios turnos"),
                        true,
                    )
                    .await;
                    return Ok(());
                }
            }
        }
    }
}
```

**Nota sobre el shape `if/else` vs early-return**: el código actual usa `if cap > 0 && !resolved_now { ... }` con un `return Ok(())` interno cuando escala. Mantenemos esa misma forma (no convertimos a múltiples `return Ok(())` post-rama) porque el bloque que sigue (`escalate_on_critical_tool_failure` en línea 912+) DEBE seguir corriendo en los casos donde NO escalamos por `no_resolution`. El único `return Ok(())` se conserva exactamente donde está hoy: tras `auto_escalate` exitoso en el path B5.

**Por qué `B2` está antes que `B3`**: si un turn tiene `transfer_to_agent` exitoso (Action) Y `had_chain_transfer=true`, queremos el log B2 (más específico — dice qué tool hizo el reset). El reset físico es idempotente con el `clear_ai_conv_counters` que ya hace el tool, así que no hay doble efecto.

**Edge: turn con Action + InfoLookup ambos exitosos en mismo turn** (raro, pero posible): B2 gana (Action) → reset. Correcto: si hay un Action exitoso, el InfoLookup que lo acompaña es trabajo de soporte para esa Action.

### 5. `src/modules/ai_agent/handler.rs` — validador de rango

Hay dos endpoints que ejecutan `apply_escalation` (`handler.rs:586` create, `handler.rs:683` update). Ambos pasan por la misma función `apply_escalation` (línea 802-817) que muta `cur` in-place y retorna `()`.

**Decisión**: cambiar la firma de `apply_escalation` a `Result<(), ApiError>` y agregar la validación dentro. Razón: única definición, dos call sites — DRY. Caller-friendly: ambos handlers ya retornan `Result<…, ApiError>`.

```rust
fn apply_escalation(
    cur: &mut AiEscalationRules,
    patch: Option<crate::models::ai_agent::AiEscalationRulesInput>,
) -> Result<(), ApiError> {
    let Some(p) = patch else { return Ok(()); };
    if let Some(v) = p.keywords { cur.keywords = v; }
    if let Some(v) = p.max_turns_without_resolution { cur.max_turns_without_resolution = v; }
    if let Some(v) = p.qualification_window_turns {
        if v > 10 {
            return Err(ApiError::domain_with_field(
                axum::http::StatusCode::BAD_REQUEST,
                "qualification_window_out_of_range",
                "qualification_window_turns",
                format!(
                    "qualification_window_turns must be between 0 and 10 (got: {})",
                    v
                ),
            ));
        }
        cur.qualification_window_turns = v;
    }
    if let Some(v) = p.max_identification_attempts { cur.max_identification_attempts = v; }
    if let Some(v) = p.escalate_on_critical_tool_failure {
        cur.escalate_on_critical_tool_failure = v;
    }
    if let Some(v) = p.always_escalate_when_asked { cur.always_escalate_when_asked = v; }
    if p.default_ticket_category_id.is_some() {
        cur.default_ticket_category_id = p.default_ticket_category_id;
    }
    Ok(())
}
```

Y los call sites:

```rust
// handler.rs:586 (create_ai_agent_handler)
- apply_escalation(&mut agent.escalation, body.escalation);
+ apply_escalation(&mut agent.escalation, body.escalation)?;

// handler.rs:683 (update_ai_agent_handler)
- apply_escalation(&mut agent.escalation, body.escalation);
+ apply_escalation(&mut agent.escalation, body.escalation)?;
```

**Constructor elegido**: `ApiError::domain_with_field(...)` (definido en `src/error.rs:191-204`). Razón: produce un body con `code`, `field` y `message` — el shape estable que el front necesita (project convention: `feedback_response_field_naming.md` + `project_api_response_shapes.md`). HTTP 400 BAD_REQUEST coincide con la spec del proposal.

**Por qué `> 10`** (no `> 10 || valor inválido`): el tipo es `u32` → ya excluye negativos. La cota inferior 0 es válida (default actual). Sólo hay que validar la superior.

**Validación retroactiva**: docs existentes en MongoDB con `qualification_window_turns > 10` (improbable porque hoy no existe el campo) NO se validan — sólo se chequea en el path de POST/PUT. Si hubiera datos sucios futuros, el siguiente PATCH del admin los corrige.

### 6. OpenAPI — actualizaciones automáticas vs manuales

- **Schemas**: `AiEscalationRulesDto` y `AiEscalationRulesInput` derivan `ToSchema`. Agregar el campo nuevo a ambos structs hace que la spec OpenAPI lo refleje automáticamente. **NO hace falta tocar `src/openapi.rs`**.
- **Response code 400**: el handler `create_ai_agent_handler` (`handler.rs:533-606`) y `update_ai_agent_handler` (línea 609+) tienen `responses(...)` en su `#[utoipa::path(...)]`. Si hoy no listan `400`, agregar:
  ```rust
  (status = 400, description = "qualification_window_out_of_range"),
  ```
  Si ya listan `400`, dejar como está (el `description` puede quedar genérico).

  Detalle: tiene que confirmarse al hacer apply si los `responses` actuales incluyen 400 — si no, agregar la entrada. Bajo costo, mejora la doc.

## Edge cases

| Case | Expected behavior |
|------|-------------------|
| Múltiples Action tools en un turn (`request_human` + `create_ticket`) | B2 matchea el primero encontrado por `find()` → un `reset_ai_no_resolution` (DEL idempotente). Log lista el primero. Comportamiento correcto. |
| Action + InfoLookup ambos exitosos en mismo turn | B2 gana (orden de ramas) → reset. InfoLookup de soporte no se loguea (ok — el log de B2 es suficiente). |
| Tool con `success=false` (cualquier categoría) | B2 no matchea (filtra por `t.success`). B4 tampoco (mismo filtro). Cae en B5 → incr. Correcto: tool fallido NO es progreso. |
| `prior_ai_turns == window` exactamente | B1 NO matchea (`<` no `<=`). Pasa a evaluación normal — empieza el path estándar. Match con la intención del proposal: "después de la ventana". |
| `qualification_window_turns == 0` | B1 nunca matchea (`(prior_ai_turns as u64) < 0` imposible para u64). Comportamiento idéntico al actual. **Default por `#[serde(default)]`** — back-compat 100%. |
| `max_turns_without_resolution == 0` | El bloque entero queda gated por `if cap > 0 { ... }`. Feature deshabilitada — comportamiento actual preservado. |
| Tool name desconocido en payload | `tool_category` emite `tracing::warn!` y devuelve `InfoLookup`. Cae en B4 → skip. Safe default. |
| Redis caído al llamar `reset_ai_no_resolution` | `get_multiplexed_async_connection().await.err() => return` — best-effort silent (mismo handling que `incr_ai_no_resolution`). El counter quedará "sucio" hasta el próximo reset/expire (TTL 7d). |
| `transfer_to_agent` exitoso → llega a B2 | El tool ya hizo `clear_ai_conv_counters` en `tools.rs:763-766` (key ya borrada). El `reset_ai_no_resolution` adicional es DEL sobre key inexistente → no-op idempotente. Sin doble efecto, sin bug. |
| `had_chain_transfer=true` Y `transfer_to_agent` exitoso en `tool_calls` | B2 gana → reset + log "reset Action transfer_to_agent". B3 nunca evalúa. Correcto: el log más específico gana. |
| `qualification_window_turns > 10` (input vía PATCH) | `apply_escalation` retorna `ApiError::domain_with_field` → HTTP 400 con `code=qualification_window_out_of_range`. Patch entero rechazado (atómico). |
| Counter expira por TTL (7d) entre turnos durante la window | B1 sigue absorbiendo turnos hasta el threshold. La key arranca de cero post-window con `incr` → 1. No se ve raro. |
| Turno dentro de la window con tool Action exitoso (raro) | B1 gana (window prevalece). El reset físico no se ejecuta — innecesario, la key ni se ha tocado todavía. Ahorra un round-trip a Redis. |

## Affected files (touch list)

| File | Change | Why |
|------|--------|-----|
| `src/cache/redis_client.rs` | + 1 método (`reset_ai_no_resolution`) entre líneas 487 y 491 | Targeted reset sin tocar otras 2 keys |
| `src/models/ai_agent.rs` | + 1 campo en `AiEscalationRules` (línea 142), + 1 campo en `AiEscalationRulesDto` (504), + 1 campo en `AiEscalationRulesInput` (685), propagar en `From<AiEscalationRules> for AiEscalationRulesDto` (514-525) | Modelo + DTO + Input |
| `src/modules/ai_agent/tools.rs` | + enum `ToolCategory`, + fn `tool_category(&str)` tras línea 122 | Categorización |
| `src/modules/ai_agent/dispatch.rs` | refactor de líneas 871-910 (5 ramas) | Lógica core del fix |
| `src/modules/ai_agent/handler.rs` | cambio de `apply_escalation` a `Result<(), ApiError>` (línea 802-817), `?` en call sites (586, 683), `qualification_window_turns: 0` en `default_agent` (línea 233-240), opcionalmente `(status=400)` en `responses(...)` de los `#[utoipa::path]` de create/update | Validador + default explícito + OpenAPI |
| Tests (TBD por apply phase — probablemente nuevo `#[cfg(test)] mod tests` en `dispatch.rs` o en un módulo de tests que ya exista) | + 4 escenarios A/B/C/D del proposal + edge cases | Cobertura |

## Untouched (explicit)

- `src/modules/ai_agent/escalation.rs` — `auto_escalate` (líneas 67-143) sin cambios.
- `src/modules/ai_agent/tools.rs:759-766` — `transfer_to_agent` sigue llamando `clear_ai_conv_counters`.
- `src/cache/redis_client.rs:491-504` — `clear_ai_conv_counters` sin cambios.
- `src/cache/redis_client.rs:478-487` — `incr_ai_no_resolution` sin cambios.
- `src/modules/ai_agent/dispatch.rs:325-353` — fresh-start detection sin cambios (ya hace `clear_ai_conv_counters` en su path; ortogonal a la window).
- Schema MongoDB `WaConversations` y `AiAgents` — sin cambios.
- `src/openapi.rs` — la `ToSchema` derive se encarga; sólo handlers afectados ganan posiblemente un `(status = 400, ...)` extra.
- Frontend / API consumers — campo nuevo aparece automáticamente en GET de agentes con valor `0` por default; el front lo ignora hasta que la UI lo muestre.

## Risks (design-level, restated)

- **Tool sin categorizar**: nueva tool en `tools.rs` que olvida un `T_*` en `tool_category` cae en `unknown =>` → `InfoLookup` + `warn!`. Aceptable: safe default, ruido logueable. Mitigación: comentario en la fn explicando que las tools nuevas se categorizan en el mismo PR (mencionado en proposal).
- **Window > 10 admin error**: bloqueado en CRUD. Docs existentes con valores stale no se validan retroactivamente — pero hoy ningún doc tiene el campo (back-compat default 0), así que el riesgo no aplica al rollout inicial.
- **`tracing::debug!` filtering**: en producción `RUST_LOG=info` los `debug!` se descartan. Decisión consciente — el path skip/reset es alto-volumen (todos los turnos no-incr). El path `info!` (incr) sigue. Si necesitamos investigar un caso, un superadmin puede subir el log level temporalmente.
- **Doble path de reset en `transfer_to_agent`** (proposal §Risks): `clear_ai_conv_counters` (en el tool) + `reset_ai_no_resolution` (en el dispatch). Ambos DEL idempotentes — sin bug, sólo redundancia consciente.
- **Cambio de firma de `apply_escalation`**: rompe nada externo (es `fn` privada del módulo). Los dos call sites internos se actualizan en el mismo PR.

## Test strategy

Tests viven idiomáticamente en el mismo crate via `#[cfg(test)] mod tests` (Rust convention). Ubicación recomendada: `src/modules/ai_agent/dispatch.rs` al final del archivo. Mockear `state.redis` y `state.db` siguiendo el patrón existente del proyecto (a confirmar — si no existe, el apply phase puede crear helpers en el mismo módulo).

**6-8 tests** estimados:

1. **Scenario A — Carla qualification window** (proposal §Test plan A):
   `cap=4`, `window=4`, 5 turnos texto puro → counter queda en 1 al final (los primeros 4 absorbidos por window, 5º sí incrementa); no escalación.
2. **Scenario B — Action reset** (proposal §Test plan B):
   `cap=4`, `window=0`, secuencia `[no-tool, no-tool, transfer_to_agent ok, no-tool, no-tool]` → counter va 1, 2, 0, 1, 2; no escalación. Verifica que B2 actúa.
3. **Scenario C — InfoLookup no resetea** (proposal §Test plan C):
   `cap=4`, `window=0`, `[no-tool, no-tool, list_plans ok, no-tool, no-tool]` → counter 1, 2, 2, 3, 4 → escala en turno 5. Verifica B4.
4. **Scenario D — Sanity** (proposal §Test plan D):
   `cap=3`, `window=0`, 3 turnos texto → 1, 2, 3 → escala. Verifica B5 path completo.
5. **Edge: window inhabilitado por `cap=0`** → ningún turn evaluado; counter no se toca.
6. **Edge: tool desconocido success** → cae en InfoLookup (skip), warn loggeado.
7. **CRUD validator**: PATCH `qualification_window_turns: 11` → 400 `qualification_window_out_of_range`.
8. **CRUD validator**: PATCH `qualification_window_turns: 10` → 200 (boundary inclusivo).

Tests 1-6 requieren mock de `redis` + `db`. Tests 7-8 son tests de handler-level (HTTP integration o unit con `apply_escalation`).

## Open design questions

Confirmaciones hechas durante este diseño (ya resueltas, no quedan abiertas):

- ✅ `last_output.tool_calls` es `Vec<AiToolCallLog>` — el campo es **`tool_name`** (no `name`). Confirmado en `src/models/ai_agent.rs:241-249` y uso en `dispatch.rs:846`. Pseudocódigo ya usa `t.tool_name`.
- ✅ `had_chain_transfer` (bool, `dispatch.rs:841`), `cross_workspace_message` (Option<String>, `dispatch.rs:509`), `prior_ai_turns` (i64, `dispatch.rs:331-335`), `conv_hex` (String, `dispatch.rs:404`) confirmados.
- ✅ `T_*` constants confirmadas (`tools.rs:116-122`).
- ✅ `apply_escalation` es la única función que materializa el patch — única locación de validación. Decisión: cambiar firma a `Result<(), ApiError>`.
- ✅ `AiEscalationRulesDto` y `AiEscalationRulesInput` requieren toque manual (no son derives automáticos del modelo doméstico — From impl + struct separadas).

**TODOs reales para el apply phase**:

- ⚠️ Confirmar si `apply_escalation` se llama desde algún tercer call site oculto (search `apply_escalation\(` en todo el módulo). Búsqueda inicial mostró 2 (líneas 586, 683) + la definición (802) — parece completo, pero apply debe verificar.
- ⚠️ Confirmar exact signature de `responses(...)` en los `#[utoipa::path(...)]` de create/update — agregar `(status = 400, description = "qualification_window_out_of_range")` solo si no está ya cubierto por una entrada genérica.
- ⚠️ Confirmar si existe un patrón de tests con mocks de `state.redis` + `state.db` en el proyecto (ej. en `whatsapp/handler.rs` o algún otro módulo con tests). Si no existe, los tests del Scenario A-D pueden escribirse como tests de integración usando un Redis local; el apply phase decide.
- ⚠️ Verificar que el seed de agentes en `seed.rs` (si lo hay para agentes default tipo Carla/Sofía) no construye `AiEscalationRules` directamente — si lo hace, agregar `qualification_window_turns: 0` ahí también.

## Inconsistencies proposal/exploration

Ninguna detectada. La proposal lockea explícitamente todas las decisiones que el exploration deja abiertas (qualification_window_turns como campo nuevo + categorización + targeted reset + validador 0..=10). Las recomendaciones de defaults por agente (Sofía/Carla/Andrea/Gabriel) están claramente marcadas como **informativas — NO migradas** (proposal §Recomendaciones).
