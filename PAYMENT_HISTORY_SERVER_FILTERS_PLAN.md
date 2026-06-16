# Plan: filtros server-side para historial de pagos

## Objetivo

Migrar el historial de pagos para que la tabla mantenga el mismo comportamiento visible para el usuario, pero que la búsqueda, filtros y ordenamiento se ejecuten en DB a través de la API.

El front nuevo debe dejar de filtrar localmente sobre páginas ya cargadas y debe usar un único endpoint paginado.

## Estado

- Backend fase 1 completado en `develop`.
- Commit backend inicial: `8c04d6d Add server-side payment history filters`.
- Corrección de performance: `search`, `client`, `creator` y `editor` ahora resuelven nombres a IDs antes de consultar `Payments`, evitando `$lookup` masivo antes de paginar salvo cuando se ordena por nombre. Para mantener búsquedas por nombre rápidas, `search` usa un fast-path: si el término coincide con `Clients.sName` o `Users.sName`, filtra por IDs; si no hay coincidencias de nombres, cae al search textual sobre campos propios de `Payments`.
- Índices nuevos requeridos: ejecutar `mongosh <MONGO_URI> scripts/migrations/2026-06-16-payment-history-indexes.js` en producción/staging.
- Ajuste de rango de fechas: cuando viene `created_from` o `created_to`, el endpoint devuelve todos los pagos coincidentes del rango en una sola respuesta (`has_next_page=false`), sin aplicar `$skip/$limit` por paginación.
- Corrección de `creator`/`editor`: `Users._id` es UUID string, mientras `Clients._id` es ObjectId. Los filtros por operador/editor ahora resuelven `_id` como BSON genérico para que `Payments.idCreator` / `Payments.idEditor` filtren correctamente por strings.
- Validación backend ejecutada: `cargo fmt`, `cargo check`, diagnósticos del editor y `git diff --check`.
- Pendiente: integración frontend y validación end-to-end del comportamiento visible de la tabla.

## Endpoint único

```txt
GET /v1/auth-user/payments/list/complete
```

El front nuevo deja de usar:

```txt
GET /v1/auth-user/payments/list/simple
```

`simple` puede quedar en backend por compatibilidad temporal, pero no debe ser parte del flujo nuevo del historial.

## Respuesta esperada

La estructura de respuesta no cambia:

```json
{
  "ok": true,
  "data": {
    "items": [],
    "page": 1,
    "per_page": 500,
    "has_next_page": false
  }
}
```

## Query params acordados

```txt
owner=<idOwner>
idOwner=<idOwner>

page=1
per_page=500

search=<texto global>

reference=<referencia>
client=<cliente>
reason=<motivo>
commentary=<comentario>
state=<estado>
creator=<operador>
editor=<editor>

type=cash|usd|mobile

created_from=2026-06-01T00:00:00Z
created_to=2026-06-30T23:59:59Z

amount_min=10
amount_max=50

amount_bs_min=1000
amount_bs_max=5000

sort_by=created_at|client|reason|state|creator|editor|amount|amount_bs|reference
sort_dir=asc|desc
```

## Semántica de filtros

Todos los filtros estructurados se combinan con `AND`.

La búsqueda global `search` también se combina con `AND`, pero internamente busca con `OR` en varios campos.

Ejemplo:

```txt
state=Activo&type=mobile&search=maria
```

Debe significar:

```txt
state == Activo
AND type == mobile
AND (
  reference contains maria
  OR reason contains maria
  OR commentary contains maria
  OR client contains maria
  OR creator contains maria
  OR editor contains maria
  OR state contains maria
)
```

## Definiciones cerradas

### Paginación

| Param | Regla |
|---|---|
| `page` | Página actual. Default `1`. |
| `per_page` | Tamaño de página. Default `500`, máximo `500`. |

### Owner

| Param | Regla |
|---|---|
| `owner` | Filtra por provider/owner. |
| `idOwner` | Alias legacy de `owner`. |

Reglas de permisos:

- Provider solo ve sus propios datos.
- Staff/admin puede ver todos o filtrar por owner.
- Owner no permitido responde `403`.

### Búsqueda global

```txt
search=<texto>
```

Debe buscar parcial y case-insensitive en:

| Campo lógico | Fuente |
|---|---|
| Referencia | `Payments.sReference` |
| Motivo | `Payments.sReason` |
| Comentario | `Payments.sCommentary` |
| Estado | `Payments.sState` |
| Cliente | `Clients.sName` |
| Creador/operador | `Users.sName` via `idCreator` |
| Editor | `Users.sName` via `idEditor` |

Reglas backend:

- Hacer `trim`.
- Ignorar si queda vacío.
- Escapar regex si se usa regex.
- Case-insensitive.
- No tratar `search` como alias de referencia.
- Aceptar cualquier longitud tras `trim`.

Reglas frontend:

- Aplicar debounce de `400ms`.

### Referencia

```txt
reference=<texto>
```

Confirmado:

```txt
reference = contains parcial case-insensitive
```

Ejemplo:

```txt
reference=8561
```

Debe encontrar referencias que contengan `8561`.

No se implementa `reference_exact` en esta fase.

### Cliente

```txt
client=<texto>
```

Parcial y case-insensitive sobre el nombre del cliente.

### Motivo

```txt
reason=<texto>
```

Parcial y case-insensitive sobre `Payments.sReason`.

### Comentario

```txt
commentary=<texto>
```

Parcial y case-insensitive sobre `Payments.sCommentary`.

### Creador / operador

```txt
creator=<texto>
```

Parcial y case-insensitive sobre nombre del creador/operador.

### Editor

```txt
editor=<texto>
```

Parcial y case-insensitive sobre nombre del editor.

### Estado

```txt
state=<estado>
```

Exacto normalizado.

Ejemplos equivalentes:

```txt
state=activo
state=ACTIVO
state=Activo
```

Deben resolver a:

```txt
Activo
```

También normalizar otros estados existentes, por ejemplo:

```txt
anulado -> Anulado
```

### Tipo

```txt
type=cash|usd|mobile
```

Mapeo:

| UI | Query | Condición DB |
|---|---|---|
| Efectivo | `cash` | `bCash = true` |
| USD | `usd` | `bCash = false AND bUSD = true` |
| Pago móvil | `mobile` | `bCash = false AND bUSD = false` |

### Fechas

```txt
created_from=2026-06-01T00:00:00Z
created_to=2026-06-30T23:59:59Z
```

Reglas:

- Rango inclusivo.
- Si viene solo `created_from`, filtra desde esa fecha.
- Si viene solo `created_to`, filtra hasta esa fecha.
- Aceptar ISO.
- Backend debe soportar `dCreation` como `Date` BSON y como string ISO.

### Montos USD

```txt
amount_min=10
amount_max=50
```

Sobre `Payments.nAmount`.

### Montos VES

```txt
amount_bs_min=1000
amount_bs_max=5000
```

Sobre `Payments.nBs`.

### Ordenamiento

Incluir en esta fase:

```txt
sort_by=created_at|client|reason|state|creator|editor|amount|amount_bs|reference
sort_dir=asc|desc
```

Defaults:

```txt
sort_by=created_at
sort_dir=desc
```

Mapeo:

| UI | `sort_by` |
|---|---|
| Fecha/Hora | `created_at` |
| Cliente | `client` |
| Motivo | `reason` |
| Estado | `state` |
| Operador | `creator` |
| Editor | `editor` |
| USD | `amount` |
| VES | `amount_bs` |
| Referencia | `reference` |

La columna `Tipo` queda fuera en esta fase salvo que se decida agregar `sort_by=type` después.

## Estrategia backend recomendada

### 1. Parsear query params

Ampliar la query del handler de `complete` para incluir:

- `search`
- `reference`
- `client`
- `reason`
- `commentary`
- `state`
- `creator`
- `editor`
- `type`
- `created_from`
- `created_to`
- `amount_min`
- `amount_max`
- `amount_bs_min`
- `amount_bs_max`
- `sort_by`
- `sort_dir`

### 2. Validaciones

- `page`: mínimo `1`.
- `per_page`: mínimo `1`, máximo `500`.
- `type`: solo `cash`, `usd`, `mobile`.
- `sort_by`: solo valores permitidos.
- `sort_dir`: solo `asc`, `desc`.
- Fechas: ISO válido o responder `400 bad_request`/domain error.
- Montos: números válidos; si min > max, responder error claro.

### 3. Match inicial en `Payments`

Aplicar primero filtros que no requieren `$lookup`:

- `owner` / `idOwner` convertido a `idClient IN [...]`.
- `reference`.
- `reason`.
- `commentary`.
- `state`.
- `type`.
- `created_from` / `created_to`.
- `amount_min` / `amount_max`.
- `amount_bs_min` / `amount_bs_max`.

### 4. Fecha normalizada

Agregar campo temporal para ordenar y filtrar de forma compatible con `Date` BSON y string ISO:

```txt
_sortDate = convert(dCreation -> date)
```

### 5. Lookups condicionales

Hacer `$lookup` antes de paginar si se necesita filtrar, buscar o ordenar por:

- `client`
- `creator`
- `editor`
- `search`
- `sort_by=client`
- `sort_by=creator`
- `sort_by=editor`

Si no se necesita ninguno de esos campos para filtrar/buscar/ordenar, paginar primero y hacer `$lookup` después, como optimización.

### 6. Search global

Si viene `search`, aplicar un `$match` con `$or` sobre:

- `sReference`
- `sReason`
- `sCommentary`
- `sState`
- `client_name`
- `creator_name`
- `editor_name`

### 7. Sort server-side

Aplicar sort real antes de paginar.

Campos de sort:

| `sort_by` | Campo pipeline |
|---|---|
| `created_at` | `_sortDate` |
| `client` | `client_name` |
| `reason` | `sReason` |
| `state` | `sState` |
| `creator` | `creator_name` |
| `editor` | `editor_name` |
| `amount` | `nAmount` |
| `amount_bs` | `nBs` |
| `reference` | `sReference` |

Agregar `_id` como tiebreaker estable.

### 8. Paginación

Sin filtro de fechas, usar:

```txt
skip = (page - 1) * per_page
limit = per_page + 1
```

Con `created_from` o `created_to`, no aplicar `$skip/$limit`: el rango de fechas debe devolver todos los pagos coincidentes en una sola respuesta.

Luego:

```txt
has_next_page = items.len() > per_page
```

Si hay extra, truncar a `per_page`.

## Tareas backend

- [x] Revisar implementación actual de `list_payments_complete`.
- [x] Ampliar `PaymentHistoryQuery` con todos los query params acordados.
- [x] Crear helpers para:
  - [x] trim de strings opcionales.
  - [x] regex escapado case-insensitive.
  - [x] normalización de `state`.
  - [x] parse de `type`.
  - [x] parse de fechas ISO.
  - [x] parse de montos.
  - [x] parse de `sort_by` / `sort_dir`.
- [x] Implementar filtros iniciales sobre `Payments`.
- [x] Implementar filtro por rango de fechas soportando `Date` BSON y string ISO.
- [x] Implementar lookups condicionales para cliente/creator/editor.
- [x] Implementar filtros por cliente/creator/editor.
- [x] Implementar búsqueda global `search`.
- [x] Implementar sort server-side.
- [x] Mantener estructura de respuesta sin cambios.
- [x] Validar permisos `owner/idOwner` igual que ahora.
- [x] Ejecutar `cargo fmt`.
- [x] Ejecutar `cargo check`.
- [x] Revisar diagnósticos.
- [x] Solo al final, subir versión del paquete y OpenAPI.
- [x] Commit y push cuando el cambio esté validado.

## Tareas frontend

- [ ] Dejar de consumir `GET /v1/auth-user/payments/list/simple`.
- [ ] Usar solo `GET /v1/auth-user/payments/list/complete`.
- [ ] Mapear filtros actuales de la UI a query params server-side.
- [ ] Enviar `search` como búsqueda global.
- [ ] Enviar filtros estructurados:
  - [ ] `reference`.
  - [ ] `client`.
  - [ ] `reason`.
  - [ ] `commentary`.
  - [ ] `state`.
  - [ ] `creator`.
  - [ ] `editor`.
  - [ ] `type`.
  - [ ] fechas.
  - [ ] montos USD/VES.
- [ ] Enviar `sort_by` / `sort_dir` cuando el usuario ordene columnas.
- [ ] Mantener scroll infinito con los mismos filtros y solo cambiando `page`.
- [ ] Cada cambio de filtro/sort debe:
  - [ ] resetear `page=1`.
  - [ ] limpiar lista acumulada.
  - [ ] solicitar la primera página.
- [ ] Aplicar debounce de `400ms` para campos de texto.
- [ ] Quitar filtrado local del historial cuando backend soporte todos los filtros.
- [ ] Quitar ordenamiento local del historial cuando `sort_by/sort_dir` esté integrado.

## Riesgos y consideraciones

### Search global

`search` es el filtro más costoso. En backend se optimizó para resolver coincidencias de `Clients.sName` / `Users.sName` a IDs antes de consultar `Payments`. Para evitar búsquedas de 40s+ por operador/cliente, si el término coincide con nombres se usa ese fast-path por IDs; si no coincide con nombres, se aplica el search textual sobre campos propios del pago (`sReference`, `sReason`, `sCommentary`, `sState`), que puede seguir siendo más costoso porque esos filtros son `contains` case-insensitive.

Si en producción se vuelve lento, considerar:

- Campo denormalizado de búsqueda en `Payments`.
- Índices adicionales.
- MongoDB Atlas Search o text index si aplica.
- Mínimo de caracteres para `search` en una iteración futura.

### Fechas mixtas

Actualmente `Payments.dCreation` puede estar como `Date` BSON o string ISO.

El endpoint debe soportar ambos, pero a futuro conviene una migración separada para normalizar `dCreation` a `Date` BSON.

### Ordenamiento con joins

Ordenar por `client`, `creator` o `editor` obliga a resolver nombres antes de paginar.

Esto es correcto para exactitud, pero puede ser más costoso que ordenar por campos propios de `Payments`.

## Criterios de aceptación

- [ ] La tabla del front mantiene el mismo comportamiento visible.
- [ ] `complete` es la única fuente de datos del historial.
- [x] Filtros y búsqueda se aplican en DB, no localmente.
- [ ] Scroll infinito sigue funcionando.
- [x] `reference` busca parcial case-insensitive.
- [x] `search` busca parcial case-insensitive en todos los campos acordados.
- [x] `state` y `type` son exactos/normalizados.
- [x] Rangos de fechas y montos funcionan.
- [x] Sort server-side funciona para los campos acordados.
- [x] Respuesta mantiene `{ ok, data: { items, page, per_page, has_next_page } }`.
- [x] `cargo check` pasa.
- [x] No se cambia versión hasta que el backend esté implementado y validado.
