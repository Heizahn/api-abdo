# Plan backend: agentes IA — routing, pagos y configuración portable

## Estado del documento

- Rama de trabajo: `develop`.
- Entorno de prueba: VM de desarrollo con número WhatsApp de pruebas, aislado de producción.
- Producción: todavía no tiene agentes IA configurados.
- Alcance actual: backend primero. Frontend queda separado y se coordina cuando el contrato API esté definido.
- Regla de trabajo: este documento es planificación; no implica cambios funcionales hasta aprobación explícita.

## Objetivo

Corregir y endurecer el módulo de agentes IA para que:

1. Sofía (recepcionista) clasifique y enrute correctamente.
2. Andrea (pagos) atienda saldo, métodos de pago y comprobantes sin confirmar falsamente pagos no aprobados.
3. La configuración de agentes sea explícita, editable y portable.
4. La IA se pause/reactive por conversación, no globalmente.
5. Al final exista import/export JSON para copiar configuración probada de desarrollo a producción.

---

## Configuración actual recibida

### Agentes actuales en desarrollo

| Agente | Estado | Rol actual visible | Observación |
|---|---:|---|---|
| Sofía — Recepcionista | enabled + live | Recepcionista | `is_receptionist=true`, tiene `transfer_to_agent` a Carla y Andrea |
| Andrea — Pagos | enabled + live | Pagos/cobranzas | Atiende saldo, métodos y comprobantes; no tiene `list_banks` activo |
| Carla — Ventas | enabled + live | Ventas | Tiene `transfer_to_agent` a Andrea |
| Gabriel — Soporte | disabled + live | Soporte | No está activo actualmente |

### UI actual observada

La pantalla de detalle de agente tiene:

- Identidad: nombre, descripción, números WhatsApp, `is_receptionist`.
- Estado: activo + shadow/live.
- Horario.
- Modelo.
- Límites.
- Personalidad.
- System prompt.
- Transferencias.
- Tools.
- Reglas de escalación.
- FAQs.

No se ve campo editable para `purpose`.

---

## Hallazgos confirmados en backend

### P0 — `purpose` existe en modelo/DB pero falta en API/DTO

En `src/models/ai_agent.rs`:

- `AiAgent` ya tiene:
  - `purpose: Option<AiAgentPurpose>`
- `AiAgentPurpose` ya existe con valores:
  - `recepcionista`
  - `ventas`
  - `pagos`
  - `soporte`

Pero falta exponerlo en:

- `AiAgentItem`
- `CreateAiAgentRequest`
- `UpdateAiAgentRequest`
- `agent_to_item`
- `create_ai_agent_handler`
- `update_ai_agent_handler`
- OpenAPI contract visible al frontend

Impacto: el backend ya sabe enrutar por propósito, pero la UI/API no pueden configurarlo. En la práctica, Andrea se llama “Andrea — Pagos”, pero el backend no tiene por qué verla como `purpose=pagos` salvo edición manual en Mongo.

### P0 — Routing semántico depende de `purpose`

En `dispatch.rs`, si el preclasificador detecta `ClearPagos`, busca:

```txt
find_active_agent_by_workspace_and_purpose(workspace_id, pagos)
```

Si Andrea no tiene `purpose=pagos`, ese camino no puede activarse correctamente.

### P0 — Prompts con IDs técnicos hardcodeados

El prompt de Sofía contiene IDs de agentes:

- Andrea/Pagos: `69f240ef9b22d9461824ca71`
- Carla/Ventas: `69f2277f9b22d9461824ca70`

Esto funciona en desarrollo, pero no es portable a producción porque los ObjectIds cambiarán.

El backend ya inyecta el enum de `allowed_targets` al schema de `transfer_to_agent`; por tanto, el prompt debería hablar por rol/área, no por ObjectId.

### P1 — Andrea no tiene `list_banks` activo

Andrea tiene:

- `lookup_customer`
- `get_invoices`
- `request_human`
- `create_ticket`
- `calculate_amount_bs`
- `report_payment`
- `get_payment_methods`

Pero no tiene `list_banks` activo.

`report_payment` tiene un campo `issuing_bank_id` y el prompt/tooling recomienda resolver banco emisor con `list_banks`. Sin esa tool, Andrea puede fallar más al procesar comprobantes.

### P1 — Imagen-only de comprobante puede quedar débil

El preclasificador corre solo si hay `user_text` no vacío. Si el cliente envía solo imagen, la detección rápida `ClearPagos` puede no ejecutarse. La recepcionista sí recibe media/visión, pero hay que probar si deriva correctamente.

### P1 — Revisión humana de pagos es comportamiento correcto

`report_payment` crea `PaymentReport` en estado `Pendiente`. No aprueba ni activa saldo automáticamente. Esto es correcto por seguridad.

El prompt debe mantenerlo claro:

- “Pago registrado / reporte recibido” sí, solo si `report_payment` devuelve OK.
- “Pendiente de aprobación/revisión” siempre.
- Nunca “aprobado”, “acreditado”, “ya quedó saldado”, “ya se activó”.

### P2 — Documentación/copy viejo

Detectado:

- `src/modules/ai_agent/mod.rs` dice “Sin recepcionista todavía”. Ya no es cierto.
- `src/modules/ai_agent/tools.rs` dice “PR 2 — 4 tools”. Ya no es cierto.
- UI de FAQs menciona `search_faq`, pero en backend actual las FAQs se inyectan como bloque `[faqs]`; no se vio tool `search_faq`.
- OpenAPI info version quedó desfasada respecto al último bump hecho previamente (`src/openapi.rs` muestra `0.3.92`; `Cargo.toml` quedó en `0.3.93`). Debe sincronizarse en el próximo cambio funcional/versionado.

---

## Concepto clave: `purpose`

`purpose` es el rol técnico estable del agente. No reemplaza el nombre visible.

Ejemplo:

| Label visible | purpose |
|---|---|
| Sofía — Recepcionista | `recepcionista` |
| Andrea — Pagos | `pagos` |
| Carla — Ventas | `ventas` |
| Gabriel — Soporte | `soporte` |

### Por qué debe ser editable

Debe ser editable porque es configuración de negocio, no lógica fija del backend. Permite que el admin defina qué agente cubre cada área sin depender del nombre.

### Relación con `is_receptionist`

- `is_receptionist=true` sigue marcando quién recibe primero en un workspace.
- `purpose=recepcionista` describe semánticamente el rol.
- Ambos pueden convivir.
- Para Sofía, lo normal es tener ambos:
  - `is_receptionist=true`
  - `purpose=recepcionista`

---

## Plan backend por fases

## Fase 1 — Contrato API para `purpose` editable

### Archivos backend a modificar

- `src/models/ai_agent.rs`
- `src/modules/ai_agent/handler.rs`
- `src/openapi.rs`
- `Cargo.toml`
- `Cargo.lock`

### Cambios específicos

#### Modelo/DTO

- [ ] Agregar `purpose: Option<AiAgentPurpose>` a `AiAgentItem`.
- [ ] Agregar `purpose: Option<AiAgentPurpose>` a `CreateAiAgentRequest`.
- [ ] Agregar `purpose: Option<AiAgentPurpose>` a `UpdateAiAgentRequest`.
- [ ] Usar `#[serde(default, skip_serializing_if = "Option::is_none")]` donde aplique para compatibilidad.
- [ ] Confirmar que `AiAgentPurpose` tiene `ToSchema` y `serde(rename_all = "snake_case")`; ya existe.

#### Handler create/update

- [ ] En `agent_to_item`, devolver `a.purpose`.
- [ ] En create, después de `default_agent`, aplicar `body.purpose` si viene.
- [ ] En update, aplicar `body.purpose` si viene.
- [ ] Mantener legacy: si no viene `purpose`, queda `None`.

#### OpenAPI/versionado

- [ ] Registrar el campo en schemas generados.
- [ ] Sincronizar `src/openapi.rs` info version con `Cargo.toml`.
- [ ] Bump SemVer pre-1.0 para cambio funcional.

### Tests/validación

- [ ] `cargo check`.
- [ ] Crear agente con `purpose=pagos`.
- [ ] Actualizar agente existente a `purpose=pagos`.
- [ ] GET detalle devuelve `purpose`.
- [ ] Listado devuelve `purpose`.
- [ ] Agentes sin `purpose` siguen funcionando.

### Resultado esperado

El frontend podrá mostrar un selector editable:

```txt
Propósito: Recepcionista / Pagos / Ventas / Soporte
```

---

## Fase 2 — Configuración inicial correcta de agentes en desarrollo

Después de tener API para `purpose`, ajustar en desarrollo:

- [ ] Sofía:
  - `purpose=recepcionista`
  - `is_receptionist=true`
- [ ] Andrea:
  - `purpose=pagos`
- [ ] Carla:
  - `purpose=ventas`
- [ ] Gabriel:
  - `purpose=soporte`
  - puede seguir disabled

### Andrea tools

- [x] Activar `list_banks` para Andrea.
- [ ] Mantener `report_payment=true`.
- [ ] Mantener `get_payment_methods=true`.
- [ ] Mantener `get_invoices=true`.
- [ ] Desactivar `create_ticket` para Andrea; tickets quedan para soporte. En casos complejos de pagos, usar `request_human`.

### IVA por `tax_id` del cliente

Cambio nuevo solicitado dentro de Fase 2: los montos en Bs deben calcularse con el IVA configurado en el cliente (`Customers.idTax` / `tax_id`), no con el IVA `DEFAULT` global.

#### Alcance backend mínimo

- [ ] `get_invoices` debe resolver el cliente por `client_id` y usar su `idTax` para convertir deuda USD → Bs.
- [ ] `calculate_amount_bs` debe poder recibir `client_id` opcional para usar el `idTax` del cliente cuando la conversión pertenezca a cobranzas.
- [ ] `report_payment` debe usar el `idTax` del cliente cuando reciba `amount_usd` y tenga que derivar `amount_bs`.
- [ ] Mantener fallback a `DEFAULT` si el cliente no tiene `idTax`, salvo que negocio indique que debe fallar.
- [ ] Actualizar descripciones de tools/prompts que digan IVA `DEFAULT`, global o empresarial fijo.

#### Alcance prompt/config Andrea

- [ ] Mantener regla: siempre mostrar precios/montos en Bs.
- [ ] Cuando Andrea convierta un monto asociado a un cliente ya identificado, debe pasar `client_id` a `calculate_amount_bs`.
- [ ] No pedir al cliente tipo de IVA; debe salir del cliente en DB.

#### Dudas abiertas antes de implementar

- [ ] Confirmar si este cambio aplica solo a cobranzas/clientes existentes o también a ventas/lista de planes.
- [ ] Confirmar fallback definitivo cuando el cliente no tenga `idTax`: usar `DEFAULT` o derivar a humano.

---

## Fase 3 — Limpieza de prompts sin IDs hardcodeados

### Sofía

- [ ] Quitar tabla con ObjectIds técnicos.
- [ ] Mantener reglas conceptuales:
  - Pagos → Andrea/Pagos vía `transfer_to_agent`.
  - Ventas → Carla/Ventas vía `transfer_to_agent`.
  - Soporte técnico → ticket + humano por ahora.
- [ ] No escribir texto previo al transfer cuando sea handoff silencioso.
- [ ] En `reason`, incluir:
  - cliente si se conoce,
  - estado si se conoce,
  - mensaje literal,
  - si hay media adjunta.

### Andrea

- [ ] Ajustar prompt para incluir `list_banks` como herramienta real activa.
- [ ] Reforzar que `report_payment` devuelve reporte pendiente, no aprobación.
- [ ] Reforzar manejo de errores de:
  - `payment_date_required`
  - `media_id_not_in_conversation`
  - `destination_*_mismatch`
  - `already_registered=true`
- [ ] Revisar menciones de “fechas de vencimiento”, porque `get_invoices` devuelve saldo/monto, no necesariamente vencimiento operativo confiable.

### Criterio

Primero se corrige backend/API. Luego se corrigen prompts en UI/config dev. No hardcodear prompts en código.

---

## Fase 4 — Verificación de pausa por conversación

Objetivo: confirmar que la IA se pausa solo por chat/conversación, no global.

### Revisar/validar

- [ ] `ai_disabled=true` evita dispatch IA.
- [ ] `status=in_progress` evita dispatch IA.
- [ ] `request_human` pausa la IA en esa conversación.
- [ ] `create_ticket` + `request_human` no dejan a la IA respondiendo luego.
- [ ] Humano tomando un chat no afecta otros chats.
- [ ] Reabrir conversación limpia/rehabilita IA según flujo esperado.
- [ ] `ai_active_agent_id` no revive IA si humano ya tomó el chat.

### Pruebas manuales VM

- [ ] Chat A: pedir humano → IA se pausa.
- [ ] Chat B: IA sigue funcionando.
- [ ] Chat A: humano responde → IA no interrumpe.
- [ ] Reabrir Chat A → verificar comportamiento esperado.

---

## Fase 5 — Routing pagos y pruebas funcionales

### Rutas a probar con Sofía activa

- [ ] “saldo”
- [ ] “factura”
- [ ] “cuánto debo”
- [ ] “datos de pago”
- [ ] “pago móvil”
- [ ] “quiero pagar”
- [ ] “pagué”
- [ ] “te paso comprobante”
- [ ] texto + imagen de comprobante
- [ ] solo imagen de comprobante
- [ ] imagen no comprobante

### Esperado

- [ ] Casos de pagos llegan a Andrea.
- [ ] Andrea hace `lookup_customer`.
- [ ] Si saldo/deuda: Andrea llama `get_invoices` antes de responder.
- [ ] Si métodos: Andrea llama `get_payment_methods`.
- [ ] Si comprobante: Andrea analiza imagen, usa `list_banks` si hace falta y llama `report_payment` solo con datos suficientes.
- [ ] Si `report_payment` falla, Andrea no confirma registro.
- [ ] Si `report_payment` OK, Andrea dice pendiente de aprobación.

---

## Fase 6 — Imagen-only de comprobante

Riesgo: cliente manda solo foto sin texto.

### Primero: probar sin cambios extra

- [ ] Enviar solo imagen de comprobante al número de prueba.
- [ ] Observar si Sofía transfiere a Andrea.
- [ ] Observar si Andrea puede procesar en el mismo turno.

### Si falla, alternativas backend

#### Opción A — Prompt/config

- [ ] Reforzar Sofía: imagen de comprobante o media con contexto de pago → transfer a Pagos.

#### Opción B — Routing por estado conversacional

- [ ] Si `ai_conv_state.current_intent=pago` y llega imagen, preferir agente `purpose=pagos`.

#### Opción C — Pre-routing server-side por media

- [ ] Si `msg_type=image` y recent intents contienen pago/comprobante, enrutar a pagos.

### Decisión pendiente

No implementar B/C hasta probar A y recopilar evidencia.

---

## Fase 7 — Hardening de `report_payment`

### Confirmar comportamiento actual

- [ ] Rechaza `media_id` vacío.
- [ ] Rechaza `media_id` fuera de la conversación.
- [ ] Rechaza monto vacío.
- [ ] Rechaza `amount_bs` y `amount_usd` simultáneos.
- [ ] Rechaza monto inválido.
- [ ] Rechaza cliente no asociado al teléfono.
- [ ] No duplica referencia ya existente.
- [ ] Referencia existente en otro cliente no confirma pago.
- [ ] Pago rechazado permite nuevo reporte.
- [ ] Guarda `idCreator=ai_user_id`.
- [ ] Emite badge/evento de reporte pendiente.

### Posibles ajustes posteriores

- [ ] Normalizar error de referencia vacía (`reference_not_found_in_input` vs `reference_required`) si afecta al prompt.
- [ ] Evaluar si `payment_date_required` es demasiado estricto para comprobantes reales.
- [ ] Mejorar mensajes de error para que el LLM pida solo el dato faltante.

---

## Fase 8 — Limpieza técnica/documental backend

- [ ] Actualizar docstring de `src/modules/ai_agent/mod.rs`.
- [ ] Actualizar docstring de `src/modules/ai_agent/tools.rs`.
- [ ] Actualizar textos que digan que recepcionista/routing “llega en próxima vuelta”.
- [ ] Documentar arquitectura real:
  - recepcionista,
  - `purpose`,
  - preclassifier,
  - transfer same-workspace,
  - pausa por conversación,
  - flujo pagos.
- [ ] Revisar divergencias con `openspec/specs/ai-agent/spec.md`.

---

## Fase 9 — Import/export JSON de agentes (último)

### Requisito de producto

Desde la UI de detalle de agente:

- Botón exportar → API devuelve JSON completo del agente.
- Botón importar → se pega JSON exportado y se importa.
- Debe incluir absolutamente todo:
  - identidad,
  - purpose,
  - workspaces,
  - enabled,
  - mode live/shadow,
  - schedule,
  - model config visible,
  - personality,
  - prompt,
  - tools,
  - escalation,
  - limits,
  - debounce,
  - FAQs.

### Problema producción

Producción todavía no tiene agentes. Por eso no basta importar sobre agente existente; se necesita crear desde JSON.

### Diseño recomendado backend

#### Export individual

- [ ] `GET /v1/auth-user/whatsapp/ai-agent/agents/:id/export`
- [ ] Incluye FAQs.
- [ ] Incluye metadata de transfer targets:
  - id actual,
  - label,
  - purpose.

#### Import individual

- [ ] `POST /v1/auth-user/whatsapp/ai-agent/agents/import`
- [ ] Crea agente nuevo desde JSON.
- [ ] Crea FAQs.
- [ ] Asigna/genera `ai_user_id` según regla backend.
- [ ] Permite indicar `workspace_id` destino.

#### Resolver transfer targets

Como los ObjectIds de desarrollo no existen en producción:

- [ ] Resolver primero por `purpose`.
- [ ] Si no hay purpose, resolver por `label`.
- [ ] Si no encuentra target, devolver error claro y no importar parcialmente.

#### Export/import paquete completo

Recomendado para producción sin agentes:

- [ ] Exportar paquete con Sofía, Andrea, Carla y Gabriel.
- [ ] Importar paquete completo.
- [ ] Crear todos los agentes.
- [ ] Reconstruir `allowed_targets` después de crear todos.

---

## Plan frontend separado

No se trabaja en este repo ahora, pero queda el contrato esperado.

- [ ] Mostrar selector `purpose` en Identidad.
- [ ] Permitir editar `purpose`.
- [ ] Mostrar warnings por workspace:
  - recepcionista sin agente pagos,
  - recepcionista sin agente ventas,
  - transfer targets vacíos.
- [ ] Quitar copy viejo de “routing llega en próxima vuelta”.
- [ ] Corregir copy de FAQs que menciona `search_faq` si no existe.
- [ ] Agregar botón Exportar.
- [ ] Agregar botón Importar.
- [ ] Más adelante, import/export paquete completo.

---

## Orden de implementación recomendado

1. Fase 1 — `purpose` editable en API/OpenAPI.
2. Fase 2 — Configurar purposes reales en dev y activar `list_banks` para Andrea.
3. Fase 3 — Limpiar prompts sin ObjectIds hardcodeados.
4. Fase 4 — Verificar pausa por conversación.
5. Fase 5 — Probar routing pagos en VM desarrollo.
6. Fase 6 — Resolver imagen-only solo si falla en pruebas.
7. Fase 7 — Hardening puntual de `report_payment` según errores observados.
8. Fase 8 — Limpieza documental.
9. Fase 9 — Import/export JSON.

---

## Checklist de primera tarea funcional: `purpose` API

Cuando se autorice codear, la primera tarea concreta será:

- [ ] Bump versión en `Cargo.toml` / `Cargo.lock`.
- [ ] Sincronizar versión en `src/openapi.rs`.
- [ ] Agregar `purpose` a `AiAgentItem`.
- [ ] Agregar `purpose` a create/update request.
- [ ] Persistir `purpose` en create/update.
- [ ] Devolver `purpose` en list/detail.
- [ ] `cargo check`.
- [ ] Commit + push a `develop`.
- [ ] Probar en VM desarrollo desde UI/API.
