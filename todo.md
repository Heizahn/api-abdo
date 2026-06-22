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
