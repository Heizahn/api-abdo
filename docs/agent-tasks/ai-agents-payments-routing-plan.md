# Plan: agentes IA — enrutamiento y flujo de pagos

## Objetivo

Lograr que el flujo de WhatsApp con agentes IA maneje pagos de forma transparente y auditable:

1. La recepcionista detecta intención de pago, saldo, deuda, factura, transferencia, referencia o comprobante.
2. Si corresponde, deriva al agente especialista de pagos.
3. El agente de pagos identifica al cliente, consulta saldo/deudas si aplica, pide o procesa comprobante, llama `report_payment` solo con datos suficientes y responde sin prometer aprobaciones automáticas.

> Alcance de esta etapa: plan y diagnóstico. No se implementa código todavía.

## Archivos revisados

- `src/modules/ai_agent/mod.rs`
- `src/models/ai_agent.rs`
- `src/modules/ai_agent/dispatch.rs`
- `src/modules/ai_agent/pre_classifier.rs`
- `src/modules/ai_agent/runner.rs`
- `src/modules/ai_agent/tools.rs`
- `src/modules/ai_agent/guardrails.rs`
- `src/modules/ai_agent/state.rs`
- `src/db/mongo/ai_agent.rs`
- `src/models/payment.rs`
- `openspec/specs/ai-agent/spec.md`

## Estado actual observado

### Lo que ya existe y sirve para pagos

- `AiAgentPurpose` ya tiene `recepcionista`, `ventas`, `pagos`, `soporte`.
- `select_agent` ya prioriza:
  1. `ai_active_agent_id`,
  2. agente `is_receptionist=true`,
  3. agente activo más viejo del workspace.
- El pre-clasificador ya clasifica `ClearPagos` para pago/factura/deuda/comprobante.
- Si el pre-clasificador devuelve `ClearPagos`, `dispatch.rs` intenta buscar un agente activo del workspace con `purpose = pagos`.
- `transfer_to_agent` ya permite handoff silencioso al especialista del mismo workspace.
- `report_payment` ya existe como tool `Action` y registra `PaymentReport` en estado `Pendiente`.
- El runner ya tiene defensas para no confirmar falsamente si `report_payment` falla.
- El guardrail de `report_payment` valida que el `media_id` pertenezca a la conversación.
- El HUD `[turn_state]` ya expone `available_media_ids` al LLM.

## Hallazgos / riesgos antes de tocar código

### P0 — El propósito del agente existe en modelo/DB, pero no está expuesto en la API

`AiAgent` tiene `purpose: Option<AiAgentPurpose>`, y `find_active_agent_by_workspace_and_purpose` depende de ese campo.

Pero en los DTO/API actuales:

- `CreateAiAgentRequest` no expone `purpose`.
- `UpdateAiAgentRequest` no expone `purpose`.
- `AiAgentItem` no devuelve `purpose`.
- `default_agent` setea `purpose: None`.

Impacto: aunque el código de routing ya busca `purpose=pagos`, el front/admin no puede configurarlo por API normal. En producción esto puede obligar a editar Mongo manualmente; si no se hace, `ClearPagos` cae al agente original.

### P0 — Documentación interna vieja genera confusión

Hay comentarios/docstrings desactualizados:

- `src/modules/ai_agent/mod.rs` dice: “Sin recepcionista todavía”. Hoy ya hay recepcionista/pre-clasificador/transfer.
- `src/modules/ai_agent/tools.rs` dice “PR 2 — 4 tools”, pero el catálogo actual tiene muchas más, incluyendo pagos.

Impacto: alto riesgo operativo al trabajar este módulo, porque el código ya evolucionó y la documentación local guía mal.

### P1 — Imagen de comprobante sin texto puede no disparar `ClearPagos`

El pre-clasificador solo corre si `user_text` no está vacío. Si el cliente manda únicamente una foto del comprobante:

- No corre `ClearPagos`.
- El turno cae al agente seleccionado inicialmente, normalmente la recepcionista.
- Si la recepcionista no tiene prompt/tooling claro para derivar fotos de comprobante, puede responder mal o pedir datos de más.

Hay visión en el runner si hay imagen, pero el routing semántico rápido depende de texto.

### P1 — `report_payment` no activa saldo automáticamente

La tool crea `PaymentReport` en estado `Pendiente`. La aprobación y ajuste real de saldo/deuda siguen siendo humanos desde panel.

Esto es correcto para seguridad, pero el prompt del agente de pagos debe decir explícitamente:

- “Recibí tu comprobante y queda en revisión”.
- No decir “tu servicio/saldo quedó activo/aprobado” hasta que un pago real esté aprobado.

### P1 — El flujo de pagos requiere configuración exacta de tools por agente

Para el agente de pagos, el set mínimo recomendado es:

- `lookup_customer`
- `get_invoices`
- `get_payment_methods`
- `list_banks`
- `report_payment`
- `request_human`

Opcionales según política:

- `create_ticket`
- `calculate_amount_bs`

Si falta `list_banks`, el LLM puede fallar más al llenar `issuing_bank_id`. Si falta `lookup_customer`, `report_payment` no debe usarse de forma segura.

### P1 — `payment_date` es requerido por schema de `report_payment`

`report_payment` requiere `payment_date`; si el comprobante no lo muestra o el modelo no la extrae en RFC3339, la tool falla con `payment_date_required`.

Esto es seguro, pero el prompt de pagos debe tener un flujo claro para pedir la fecha sin confirmar registro.

### P2 — Spec/documentación vs código no están 100% alineados

Ejemplos detectados:

- `openspec/specs/ai-agent/spec.md` tiene contratos de `calculate_amount_bs` con shape/IVA que no coinciden totalmente con la implementación actual bidireccional.
- La spec de estado persistido habla de evictar llave vieja en `collected_data`, pero `state.rs` preserva las viejas y descarta la nueva.

No bloquea el objetivo de pagos, pero conviene registrar deuda técnica para evitar regresiones.

## Plan propuesto

### Fase 1 — Alinear contrato de agentes con routing por propósito

1. Exponer `purpose` en:
   - request de creación,
   - request de actualización,
   - response/listado,
   - OpenAPI.
2. Validar valores permitidos: `recepcionista`, `ventas`, `pagos`, `soporte`.
3. Definir regla operacional:
   - solo un `is_receptionist=true` recomendado por workspace,
   - al menos un agente `purpose=pagos` por workspace para activar routing de pagos.
4. Documentar configuración mínima del agente de pagos.

### Fase 2 — Endurecer routing de pagos

1. Confirmar flujo actual `ClearPagos -> find_active_agent_by_workspace_and_purpose(pagos)` con pruebas.
2. Añadir pruebas para mensajes:
   - “quiero pagar”,
   - “te paso comprobante”,
   - “cuánto debo”,
   - “saldo”,
   - referencia + monto.
3. Diseñar comportamiento para imagen-only:
   - opción A: prompt fuerte de recepcionista para derivar cualquier imagen sospechosa de comprobante a pagos,
   - opción B: pre-routing server-side por `msg_type=image` + contexto reciente con intención `pago`,
   - opción C: dejarlo al agente de pagos si la conversación ya tiene `current_intent=pago`.

### Fase 3 — Prompt/contrato del agente de pagos

Crear guía operativa para el agente de pagos:

1. Identificar cliente con `lookup_customer`.
2. Si pregunta saldo/deuda: llamar `get_invoices` antes de responder monto.
3. Si pide datos para pagar: llamar `get_payment_methods`.
4. Si envía comprobante:
   - leer imagen,
   - extraer referencia, monto, banco origen, fecha,
   - llamar `list_banks` si falta resolver banco,
   - llamar `report_payment` solo si tiene datos requeridos.
5. Si `report_payment` OK:
   - decir que el reporte quedó pendiente/en revisión,
   - no decir que el pago fue aprobado.
6. Si `already_registered=true`:
   - seguir `_hint` y distinguir duplicado/aprobado/pendiente.
7. Si falta dato:
   - pedir solo el dato faltante.
8. Si hay mismatch de destino o referencia usada por otro cliente:
   - escalar a humano.

### Fase 4 — Pruebas de seguridad y no-regresión

Casos mínimos:

- Recepcionista deriva a pagos cuando texto contiene intención pago.
- Recepcionista deriva a pagos para saldo/deuda/factura.
- Agente de pagos no confirma registro si `report_payment` falla.
- `report_payment` rechaza `media_id` inventado.
- `report_payment` no duplica referencia ya existente.
- Referencia ya aprobada responde como “ya registrada”, no “nuevo reporte”.
- Comprobante sin fecha pide fecha.
- Comprobante con banco destino equivocado devuelve mismatch y no registra.

### Fase 5 — Limpieza documental

1. Actualizar comentarios viejos del módulo IA.
2. Crear documentación corta de arquitectura actual:
   - recepcionista,
   - pre-clasificador,
   - propósito de agentes,
   - transfer same-workspace/cross-workspace,
   - flujo de pagos.
3. Alinear `openspec/specs/ai-agent/spec.md` con implementación real o abrir cambio formal si se decide modificar comportamiento.

## Orden recomendado de implementación futura

1. `purpose` en API/OpenAPI + tests.
2. Documentación de configuración mínima.
3. Pruebas de routing `ClearPagos`.
4. Prompt/seed recomendado para agente de pagos.
5. Endurecimiento de imagen-only.
6. Limpieza de specs viejas.

## Decisiones pendientes

- ¿Queremos que una imagen-only vaya a pagos automáticamente si la conversación no tiene texto?
- ¿El agente de pagos debe crear ticket además del `PaymentReport`, o basta con el badge/evento de reporte pendiente?
- ¿Se mantiene siempre revisión humana, o se evaluará auto-aprobación para referencias verificables? Recomendación actual: mantener revisión humana.
- ¿El front debe permitir configurar `purpose` y mostrar alertas cuando no exista agente `pagos` para un workspace?
