# WhatsApp Templates — API Spec

> **Estado:** v1, pendiente de implementación.
> **Fuentes:** front (`feature/whatsapp-module`) + back (`feature/whatsapp-support`).
> **Alcance:** 5 endpoints REST CRUD + 3 eventos WS + handler de webhook Meta para status updates. Source of truth híbrido (DB local + Meta).

---

## 1. Resumen

5 endpoints REST bajo `/v1/auth-user/whatsapp/templates` (POST, GET list, GET :id, PATCH, DELETE) más 3 eventos WebSocket. La colección Mongo nueva `WaTemplates` guarda metadatos custom; Meta sigue siendo dueña de `name`, `language`, `components`. El webhook de Meta sincroniza `status` y `rejection_reason` en DB y emite WS.

**Breaking change:** `GET /v1/auth-user/whatsapp/templates` cambia su shape — pasa de devolver el JSON crudo de Meta a devolver `{ ok: true, data: WaTemplateItem[] }` con campos custom (`id`, `display_name`, `is_system`, `rejection_reason`, `created_by`, etc.). Único consumidor es el front que se reescribe en este ciclo.

**Cache Redis deprecado:** `get_templates`/`set_templates` en `src/cache/redis_client.rs:130-143` se eliminan. La nueva colección DB es source of truth y se lee directo.

---

## 2. Decisiones cerradas

| ID | Tema | Decisión | Fuente |
|---|---|---|---|
| A | Source of truth | Híbrido: DB guarda metadatos custom; Meta dueña de `name`/`language`/`components`. Webhook actualiza `status` + `rejection_reason`. | back |
| B | Multi-language | Un doc `WaTemplate` por language. UI agrupa. Backend NO agrupa. | back |
| C | Edit policy | DRAFT/REJECTED → edit completo. APPROVED → solo BODY. PENDING/IN_REVIEW → readonly. | back |
| D | Delete policy | Bloqueo si está en `WaSettings.purposes`. Hard-delete: Meta `DELETE /{waba_id}/message_templates?hsm_id=X&name=Y` (1 language) + DB. Si `submit_to_meta == false`, sólo DB. | back |
| E | `is_system` | Flag explícito en DB (default `false`). Para creación nueva, lo setea el caller. Para migración inicial, se infiere por prefix `sistema_`. Sólo SUPERADMIN puede modificarlo via PATCH. | back |
| F | WS scope | Eventos broadcast a los agentes de `WaSettings.agents` del `phone_number_id` correspondiente. NO se extiende `WsRegistry` ni se agrega `SUSCRIBIR_NUMERO`. | back |
| G | GET endpoint | Mismo path, shape extendido. Breaking change explícita. | back |
| H | Auth en writes | SUPERADMIN-only (`nRole == 0`) para POST/PATCH/DELETE/GET-by-id. `bCanChat` para GET list. **Excepción explícita** a la convención `bCanChat` del módulo, justificada en §5. | front + back |
| I | Auto-prefix | Si `is_system: true` en POST, back genera `name = sistema_abdo_<slug(name_input)>_<YYYYMMDD>`. Si `is_system: false`, back usa `slug(name_input)` directo (debe pasar regex Meta). | front + back |
| J | WS naming | Un único `WA_TEMPLATE_UPDATED` para edit + status change (con `prev_status` opcional). NO se separa en `WA_TEMPLATE_STATUS_CHANGED`. | back |
| K | Slug collision | Back devuelve `409 name_already_exists`. NO genera sufijos `_2/_3` automáticos. | back |
| L | Status mapping | Meta emite 8 estados; exponemos 6. `IN_REVIEW → PENDING`, `FLAGGED → REJECTED + rejection_reason: "flagged_by_meta_quality"`. | back |

> **NOTA H (excepción a convención).** La memoria de proyecto registra `bCanChat == true` como gateway único del módulo WhatsApp. Esta excepción aplica sólo a CRUD de plantillas (administración global), no a operaciones de chat. Documentada en `feedback_whatsapp_authz.md` después de implementar.

---

## 3. Modelo `WaTemplate`

### Documento Mongo (colección `WaTemplates`, PascalCase)

| Campo | Tipo | Required | Descripción |
|---|---|---|---|
| `_id` | ObjectId | sí | PK Mongo |
| `phone_number_id` | string | sí | ID del número WA (Meta). Joinable con `WaSettings.phone_number_id` |
| `name` | string | sí | Nombre Meta. Generado por backend (ver §I) |
| `display_name` | string | sí | Etiqueta legible para UI (= `name_input`) |
| `name_input` | string | sí | Texto humano original (auditoría + edits) |
| `language` | string | sí | Código Meta (`es`, `es_VE`, `en`, `en_US`, …) |
| `category` | string | sí | `MARKETING` \| `UTILITY` \| `AUTHENTICATION` |
| `components` | Array | sí | Header + body + footer + buttons. Mismo shape que Meta |
| `body_placeholders` | int | sí (derivado) | Count de `{{N}}` en BODY.text. Lo computa el back en write |
| `status` | string | sí | Ver §4 |
| `rejection_reason` | string \| null | no | Razón Meta cuando `status` ∈ {REJECTED, FLAGGED→REJECTED} |
| `meta_template_id` | string \| null | no | `id` de Meta (`hsm_id`). `null` mientras `status == DRAFT` |
| `is_system` | bool | sí | `true` si plantilla del sistema (prefix `sistema_abdo_`) |
| `submit_to_meta` | bool | sí | Flag de creación: `false` → queda DRAFT en DB sin tocar Meta |
| `created_by` | string | sí | UUID del user creador (claims.id) |
| `created_by_name` | string | sí | Resuelto al crear (snapshot, no se actualiza si el user cambia nombre) |
| `created_at` | DateTime | sí | UTC |
| `updated_at` | DateTime | sí | UTC |

### Struct Rust (en `src/models/whatsapp.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WaTemplate {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub phone_number_id: String,
    pub name: String,
    pub display_name: String,
    pub name_input: String,
    pub language: String,
    pub category: WaTemplateCategory,
    pub components: serde_json::Value,
    pub body_placeholders: u32,
    pub status: WaTemplateStatus,
    pub rejection_reason: Option<String>,
    pub meta_template_id: Option<String>,
    pub is_system: bool,
    pub submit_to_meta: bool,
    pub created_by: String,
    pub created_by_name: String,
    pub created_at: bson::DateTime,
    pub updated_at: bson::DateTime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum WaTemplateCategory { Marketing, Utility, Authentication }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum WaTemplateStatus { Draft, Pending, Approved, Rejected, Paused, Disabled }
```

### Response shape (`WaTemplateItem`)

Igual al doc DB, pero:
- `id: String` (hex de ObjectId)
- `created_at`, `updated_at` como ISO-8601 string
- Sin `_id`

> **NOTA modelo.** `WhatsAppTemplate` actual (`src/models/whatsapp.rs:1119-1135`) es DTO de response del cache Meta. Se renombra a `WhatsAppTemplateMetaRaw` (uso interno del service para parsear respuestas Meta) y se introduce `WaTemplate` (DB) + `WaTemplateItem` (response). Coexisten hasta que se elimine el cache Redis.

---

## 4. Estados

| Estado | Descripción | Editable | Eliminable | Origen |
|---|---|---|---|---|
| `DRAFT` | Creada local, no enviada a Meta (`submit_to_meta: false`) | sí (todo) | sí (sólo DB) | local |
| `PENDING` | Enviada a Meta, esperando review | no | sí (Meta + DB) | Meta (`PENDING` o `IN_REVIEW` → mapeado) |
| `APPROVED` | Aprobada por Meta | sólo BODY | sí (Meta + DB) | Meta |
| `REJECTED` | Rechazada (incluye `FLAGGED` mapeado, ver §L) | sí (todo) | sí (Meta + DB) | Meta |
| `PAUSED` | Pausada por Meta por baja calidad | no | sí (Meta + DB) | Meta |
| `DISABLED` | Deshabilitada por Meta | no | sí (Meta + DB) | Meta |

**Transiciones válidas (server-side):**
- `null → DRAFT` — POST con `submit_to_meta: false`
- `null → PENDING` — POST con `submit_to_meta: true`
- `DRAFT → PENDING` — PATCH con `submit_to_meta: true` (envía a Meta retroactivo)
- `PENDING → APPROVED | REJECTED` — vía webhook
- `APPROVED → PAUSED | DISABLED | REJECTED` — vía webhook
- `* → (eliminado)` — DELETE

**Mapping Meta → expuesto:**
- `IN_REVIEW` → `PENDING` (Meta a veces usa uno u otro inconsistentemente)
- `FLAGGED` → `REJECTED` con `rejection_reason: "flagged_by_meta_quality"`

---

## 5. Endpoints

Todos los endpoints viven bajo `user_protected` (router de `axum_router.rs::build_router`). Auth base: `user_jwt_auth_middleware`. Auth adicional por endpoint según tabla §H.

> **Excepción a `bCanChat` (§H):** los endpoints de write y GET-by-id requieren `nRole == 0` (SUPERADMIN). El GET list sigue requiriendo `bCanChat == true` para que cualquier agente pueda usar el `WaTemplatePicker`. Implementación: chequeo de `nRole` resolviéndolo via `find_user_by_id` (el rol no viene en el JWT).

### 5.1 `POST /v1/auth-user/whatsapp/templates`

Crea una plantilla. Si `submit_to_meta: true`, sincroniza con Meta antes de persistir.

**Auth:** `user_jwt_auth_middleware` + `nRole == 0`.

**Request body** (forma flat — el back transforma a `components: []` antes de hablar con Meta):

```json
{
  "phone_number_id": "1234567890",
  "name_input": "Recordatorio de pago",
  "is_system": true,
  "category": "UTILITY",
  "language": "es",
  "header": {
    "type": "TEXT",
    "text": "Recordatorio"
  },
  "body": "Hola {{1}}, su pago de {{2}} vence el {{3}}.",
  "body_samples": ["Juan", "25 USD", "30 de abril"],
  "footer": "Equipo Abdo",
  "buttons": [
    { "type": "QUICK_REPLY", "text": "Pagar ahora" },
    { "type": "QUICK_REPLY", "text": "Hablar con un agente" }
  ],
  "submit_to_meta": true
}
```

**Variantes de `header`:**

```json
// Header de texto
"header": { "type": "TEXT", "text": "Hola {{1}}" }

// Header con imagen (requiere upload previo a /header-media — ver §14)
"header": {
  "type": "IMAGE",
  "example": { "header_handle": ["<media_id>"] }
}

// Sin header
"header": null   // o simplemente omitirlo
```

**Variantes de `buttons[]`:**

```json
{ "type": "QUICK_REPLY", "text": "Hola" }
{ "type": "URL", "text": "Ver", "url": "https://app.abdo77.com.ve" }
{ "type": "URL", "text": "Cliente {{1}}", "url": "https://app.abdo77.com.ve/c/{{1}}", "example": ["123"] }
{ "type": "PHONE_NUMBER", "text": "Llamar", "phone_number": "+584142000000" }
```

**Lógica del back:**
1. Resolver `WaSettings` por `phone_number_id`. Si no existe → `404 phone_number_not_found`.
2. Validar `name_input` no vacío, `category` válida, `language` válida, `components` válidos (§9).
3. Generar `name`:
   - Si `is_system: true` → `name = "sistema_abdo_" + slug(name_input) + "_" + YYYYMMDD` (UTC)
   - Si `is_system: false` → `name = slug(name_input)`
   - `slug(s)` = lowercase, espacios y no-alfanuméricos → `_`, strip emojis, max 512 chars.
4. Validar `name` contra regex Meta `^[a-z][a-z0-9_]{0,511}$`. Si no pasa → `400 name_invalid`.
5. Verificar unicidad `(phone_number_id, name, language)`. Si existe → `409 name_already_exists`.
6. Computar `body_placeholders` (count de `{{N}}` en BODY).
7. Si `submit_to_meta: true`:
   - POST a Meta `/{waba_id}/message_templates` con `{ name, language, category, components }`.
   - Si Meta responde 200 → `status: PENDING`, `meta_template_id: <id de Meta>`.
   - Si Meta responde error → `502 meta_rejected` con `details.meta_error_code` y `.meta_error_message`. NO se persiste el doc.
8. Si `submit_to_meta: false` → `status: DRAFT`, `meta_template_id: null`.
9. Insert en `WaTemplates`. Resolver `created_by_name` via `find_user_by_id(claims.id)`.
10. Emit WS `WA_TEMPLATE_CREATED { template: <WaTemplateItem> }` a agentes de `WaSettings.agents`.

**Response 200:**
```json
{
  "ok": true,
  "data": {
    "id": "65f...",
    "phone_number_id": "1234567890",
    "name": "sistema_abdo_recordatorio_de_pago_20260424",
    "display_name": "Recordatorio de pago",
    "name_input": "Recordatorio de pago",
    "language": "es",
    "category": "UTILITY",
    "components": [ ... ],
    "body_placeholders": 3,
    "status": "PENDING",
    "rejection_reason": null,
    "meta_template_id": "9876543210",
    "is_system": true,
    "submit_to_meta": true,
    "created_by": "uuid-...",
    "created_by_name": "Juan Pérez",
    "created_at": "2026-04-24T15:30:00Z",
    "updated_at": "2026-04-24T15:30:00Z"
  }
}
```

**Errores:** ver §6. Códigos relevantes: `name_required`, `name_invalid`, `name_already_exists`, `invalid_category`, `invalid_component`, `phone_number_not_found`, `meta_rejected`, `permission_denied_is_system` (si non-superadmin intenta `is_system: true`).

### 5.2 `GET /v1/auth-user/whatsapp/templates`

Lista plantillas con filtros y paginación cursor.

**Auth:** `user_jwt_auth_middleware` + `bCanChat == true`.

**Query params:**
| Param | Tipo | Required | Default | Descripción |
|---|---|---|---|---|
| `phone_number_id` | string | sí | — | Filtra por número |
| `status` | string | no | — | Filtra por status (uno o múltiples separados por `,`) |
| `category` | string | no | — | `MARKETING` \| `UTILITY` \| `AUTHENTICATION` |
| `only_system` | bool | no | `false` | Si `true`, sólo `is_system == true` |
| `search` | string | no | — | Substring match (case-insensitive) sobre `display_name` y `name` |
| `limit` | int | no | `50` | Max 100 |
| `cursor` | string | no | — | Cursor opaco (base64 de `_id`) |

**Response 200:**
```json
{
  "ok": true,
  "data": [ <WaTemplateItem>, ... ],
  "next_cursor": "eyJfaWQiOi..." 
}
```

`next_cursor` es `null` cuando no hay más páginas.

**Errores:** `400 invalid_query`, `404 phone_number_not_found`.

### 5.3 `GET /v1/auth-user/whatsapp/templates/:id`

Detalle de una plantilla.

**Auth:** `user_jwt_auth_middleware` + `nRole == 0`.

**Response 200:** mismo shape que POST data.

**Errores:** `404 template_not_found`.

### 5.4 `PATCH /v1/auth-user/whatsapp/templates/:id`

Actualiza una plantilla. Aplica edit policy de §C.

**Auth:** `user_jwt_auth_middleware` + `nRole == 0`.

**Request body** (todos los campos opcionales — sólo se aplican los presentes):
```json
{
  "name_input": "Nuevo nombre",
  "is_system": true,
  "category": "MARKETING",
  "components": [ ... ],
  "submit_to_meta": true
}
```

**Lógica del back:**
1. Cargar doc por `id`. Si no → `404 template_not_found`.
2. Validar permisos:
   - Si el doc tiene `is_system: true` y el caller no es SUPERADMIN → `403 permission_denied_is_system`.
   - Si el body trae `is_system` y cambia el valor, sólo SUPERADMIN puede setearlo (en este spec ya es SUPERADMIN-only, redundante).
3. Validar edit policy según `status`:
   - `DRAFT` o `REJECTED` → todos los campos editables.
   - `PENDING` → `409 cannot_edit_pending`.
   - `APPROVED` → solo BODY editable. Si el body trae cambios en `header`, `footer`, `buttons`, `category`, `language`, `name_input` → `403 cannot_edit_approved`.
   - `PAUSED`, `DISABLED` → readonly. Cualquier cambio → `409 cannot_edit_pending` (mismo bucket conceptual).
4. Si cambió `name_input` (sólo posible en DRAFT/REJECTED): regenerar `name` (paso §5.1.3) y revalidar unicidad.
5. Si `submit_to_meta` pasa de `false` a `true` (transición DRAFT → PENDING): POST a Meta. Si éxito → status `PENDING` + `meta_template_id`. Si Meta responde error → `502 meta_rejected`.
6. Si cambió BODY de un APPROVED: PATCH a Meta `/{meta_template_id}` (sólo edit de body permitido por Meta, 1/24h, 10/mes).
7. Recomputar `body_placeholders`. Update `updated_at`.
8. Emit WS `WA_TEMPLATE_UPDATED { template, prev_status }`.

**Response 200:** shape completo actualizado.

**Errores:** `404 template_not_found`, `403 cannot_edit_approved`, `409 cannot_edit_pending`, `400 invalid_component`, `502 meta_rejected`, `403 permission_denied_is_system`, `429 meta_edit_rate_limited` (si Meta devuelve rate limit del edit).

### 5.5 `DELETE /v1/auth-user/whatsapp/templates/:id`

Elimina una plantilla.

**Auth:** `user_jwt_auth_middleware` + `nRole == 0`.

**Lógica del back:**
1. Cargar doc por `id`. Si no → `404 template_not_found`.
2. Resolver in-use: query `WaSettings` con `phone_number_id == doc.phone_number_id` y verificar si algún `purposes[*].template_name == doc.name`. Si hay match → `409 template_in_use_cannot_delete` con `details.purposes: [{ key: <purpose_key>, label: <purpose_label> }]`.
3. Si `submit_to_meta: false` (DRAFT que nunca fue a Meta) → sólo borrar de DB.
4. Si tiene `meta_template_id`:
   - DELETE a Meta `/{waba_id}/message_templates?hsm_id={meta_template_id}&name={name}` (borra ese language solamente — confirmado contra docs Meta, ver `git log` del spec o `WebSearch` original).
   - Si Meta responde error 404 (template ya no existe en Meta) → log warning, continuar con delete local.
   - Si Meta responde otro error → `502 meta_rejected`.
5. Borrar doc de DB.
6. Emit WS `WA_TEMPLATE_DELETED { id, name, language, phone_number_id }`.

**Response 200:**
```json
{ "ok": true, "data": { "id": "65f..." } }
```

**Errores:** `404 template_not_found`, `409 template_in_use_cannot_delete`, `502 meta_rejected`.

---

## 6. Códigos de error

Envelope estándar: `{ ok: false, error: { code, field?, message, details? } }`. `code` es snake_case estable; `message` es user-facing en español; `field` se setea cuando el error es de un campo específico; `details` es JSON libre con info adicional cuando aplica.

| `code` | `field` | HTTP | `message` | `details` |
|---|---|---|---|---|
| `name_required` | `name_input` | 400 | "El nombre es requerido" | — |
| `name_invalid` | `name_input` | 400 | "Nombre inválido. Usa solo letras minúsculas, números y guión bajo (debe empezar con letra)" | — |
| `name_already_exists` | `name_input` | 409 | "Ya existe una plantilla con ese nombre en este idioma" | — |
| `invalid_category` | `category` | 400 | "Categoría inválida. Debe ser MARKETING, UTILITY o AUTHENTICATION" | — |
| `invalid_language` | `language` | 400 | "Idioma no soportado por Meta" | — |
| `invalid_component` | `components` | 400 | "Componente inválido: <detalle>" | `{ component_index: int, reason: string }` |
| `phone_number_not_found` | `phone_number_id` | 404 | "El número de WhatsApp no está configurado" | — |
| `template_not_found` | — | 404 | "Plantilla no encontrada" | — |
| `cannot_edit_pending` | — | 409 | "No se puede editar una plantilla en revisión" | — |
| `cannot_edit_approved` | — | 403 | "Solo el cuerpo es editable en plantillas aprobadas" | — |
| `template_in_use_cannot_delete` | — | 409 | "La plantilla está en uso en propósitos del sistema" | `{ purposes: [{ key: string, label: string }] }` |
| `permission_denied_is_system` | — | 403 | "Solo SUPERADMIN puede gestionar plantillas del sistema" | — |
| `meta_rejected` | — | 502 | "Meta rechazó la plantilla" | `{ meta_error_code: string, meta_error_message: string, rejection_reason?: string }` |
| `meta_edit_rate_limited` | — | 429 | "Meta limita las ediciones a 1 por día y 10 por mes. Intenta más tarde" | `{ retry_after?: int }` |

> **NOTA error envelope.** `ApiError` actual (`src/error.rs:79-80`) sólo emite `{ ok, error, code, field, message }`. Hay que extender la variante `ValidationError` o agregar una nueva `Domain { status, code, field, message, details: serde_json::Value }` para soportar `details`. Decisión preferida: agregar variante nueva, no romper `ValidationError` existente. Documentar en `feedback_api_response_shapes.md` después.

---

## 7. Webhook `message_template_status_update`

Meta emite este evento al WABA cuando cambia el status de un template (review completed, flagged, paused, etc.). Llega al mismo `POST /v1/webhook/whatsapp` que ya recibe mensajes; se distingue por `entry[].changes[].field`.

### Payload Meta (referencial)

```json
{
  "object": "whatsapp_business_account",
  "entry": [
    {
      "id": "<waba_id>",
      "changes": [
        {
          "field": "message_template_status_update",
          "value": {
            "event": "APPROVED" | "REJECTED" | "FLAGGED" | "PAUSED" | "DISABLED",
            "message_template_id": "9876543210",
            "message_template_name": "sistema_abdo_recordatorio_...",
            "message_template_language": "es",
            "reason": "INVALID_FORMAT" | null,
            "other_info": { ... }
          }
        }
      ]
    }
  ]
}
```

### Lógica del handler (en `src/modules/whatsapp/handler.rs`)

1. Webhook actual sólo matchea `field == "messages"` (`handler.rs:127`). Agregar branch nuevo:
   ```rust
   match change.field.as_str() {
       "messages" => process_messages(...),
       "message_template_status_update" => process_template_status(...),
       _ => { /* log + ignore */ }
   }
   ```
2. `process_template_status`:
   - Lookup en `WaTemplates` por `meta_template_id == value.message_template_id`. Si no existe → log warning + ignore (puede ser un template creado manualmente en el dashboard de Meta antes de la migración).
   - Aplicar mapping de §L:
     - `APPROVED` → `status: APPROVED`, `rejection_reason: null`
     - `REJECTED` → `status: REJECTED`, `rejection_reason: value.reason`
     - `FLAGGED` → `status: REJECTED`, `rejection_reason: "flagged_by_meta_quality"`
     - `PAUSED` → `status: PAUSED`, `rejection_reason: value.reason` (Meta a veces explica por qué)
     - `DISABLED` → `status: DISABLED`, `rejection_reason: value.reason`
   - Update `updated_at`. Capturar `prev_status`.
   - Emit WS `WA_TEMPLATE_UPDATED { template: <WaTemplateItem>, prev_status }`.
3. **Retornar HTTP 200 SIEMPRE** (Meta retry agresivo si no recibe 200).
4. Validar firma HMAC-SHA256 del request con `WHATSAPP_APP_SECRET` (ya implementado para el webhook actual — reusar mismo middleware).

### Modelo de webhook a extender

`WebhookValue` (`src/models/whatsapp.rs:220-226`) debe ganar campos opcionales o se introduce una variante tagged por `field`. Preferencia: variante tagged.

```rust
#[derive(Debug, Deserialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum WebhookChangeValue {
    Messages(WebhookMessagesValue),
    MessageTemplateStatusUpdate(WebhookTemplateStatusValue),
}

#[derive(Debug, Deserialize)]
pub struct WebhookTemplateStatusValue {
    pub event: String,
    pub message_template_id: String,
    pub message_template_name: String,
    pub message_template_language: String,
    pub reason: Option<String>,
}
```

> **NOTA webhook subscription.** Meta requiere suscribir el campo `message_template_status_update` en la app de Meta (Webhooks settings). El usuario confirmó que ya está suscrito.

---

## 8. Eventos WebSocket

Mismo patrón que los eventos existentes (`MENSAJE_NUEVO`, `CONVERSACION_NUEVA`, etc.) — discriminante `tipo` + payload en `datos`. Scope: agentes en `WaSettings.agents` del `phone_number_id` correspondiente.

### `WA_TEMPLATE_CREATED`

Emitido al crear (POST exitoso, incluso si el status es DRAFT).

```json
{
  "tipo": "WA_TEMPLATE_CREATED",
  "datos": {
    "template": <WaTemplateItem>
  }
}
```

### `WA_TEMPLATE_UPDATED`

Emitido al editar (PATCH exitoso) o cuando el webhook de Meta cambia el status. Cubre ambos casos para no duplicar.

```json
{
  "tipo": "WA_TEMPLATE_UPDATED",
  "datos": {
    "template": <WaTemplateItem>,
    "prev_status": "PENDING"
  }
}
```

`prev_status` se incluye sólo si cambió el status (i.e., en eventos disparados por webhook). En PATCHes que no tocan status, `prev_status` puede omitirse o ser igual a `template.status`.

### `WA_TEMPLATE_DELETED`

Emitido al borrar (DELETE exitoso).

```json
{
  "tipo": "WA_TEMPLATE_DELETED",
  "datos": {
    "id": "65f...",
    "name": "sistema_abdo_recordatorio_...",
    "language": "es",
    "phone_number_id": "1234567890"
  }
}
```

### Implementación de scope (alternativa simple — §F)

`WsRegistry` actual (`src/state.rs`) indexa por `user_id` solamente. Para emitir scoped por `phone_number_id` sin modificar la estructura:

1. Resolver agentes: `let agents = wa_settings.agents.iter().map(|a| &a.user_id);`
2. Por cada agente, llamar `ws_registry.send_to_user(user_id, &payload_json)` (helper existente o agregar uno).
3. Si el agente no está conectado, drop silencioso (mismo comportamiento que mensajes).

NO se agrega `SUSCRIBIR_NUMERO` al protocolo. NO se modifica `WsRegistry`.

---

## 9. Validaciones server-side

| Campo | Regla | Error |
|---|---|---|
| `name_input` | non-empty, max 512 chars | `name_required` |
| `name` (computed) | regex `^[a-z][a-z0-9_]{0,511}$`, único `(phone_number_id, name, language)` | `name_invalid`, `name_already_exists` |
| `language` | enum Meta válido (lista actualizable; mínimo: `es`, `es_VE`, `en`, `en_US`, `pt_BR`) | `invalid_language` |
| `category` | `MARKETING` \| `UTILITY` \| `AUTHENTICATION` | `invalid_category` |
| `components` | array con al menos un `BODY` | `invalid_component` (`reason: "body_required"`) |
| `BODY.text` | non-empty, ≤ 1024 chars | `invalid_component` |
| `FOOTER.text` | opcional, ≤ 60 chars | `invalid_component` |
| `HEADER` | opcional. `format` ∈ {`TEXT`, `IMAGE`, `VIDEO`, `DOCUMENT`}. Si TEXT, `text` ≤ 60 chars | `invalid_component` |
| `BUTTONS` | opcional. `buttons[]` con max 3 `QUICK_REPLY`, **o** 1 `URL`, **o** 1 `PHONE_NUMBER`. NO mezclar tipos | `invalid_component` (`reason: "buttons_mixed_types"` o `"buttons_too_many"`) |
| `body_placeholders` | computado (no recibido) | — |

---

## 10. Índices Mongo

Agregar a `scripts/create_indexes.js`:

```js
db.WaTemplates.createIndex(
  { phone_number_id: 1, name: 1, language: 1 },
  { unique: true, name: "unique_phone_name_lang" }
);

db.WaTemplates.createIndex(
  { phone_number_id: 1, status: 1 },
  { name: "phone_status" }
);

db.WaTemplates.createIndex(
  { phone_number_id: 1, is_system: 1 },
  { name: "phone_is_system" }
);

db.WaTemplates.createIndex(
  { meta_template_id: 1 },
  { unique: true, sparse: true, name: "unique_meta_id" }
);

db.WaTemplates.createIndex(
  { phone_number_id: 1, created_at: -1 },
  { name: "phone_created_desc" }
);
```

`unique_meta_id` es `sparse` porque DRAFTs tienen `meta_template_id: null`.

---

## 11. Migración inicial

Lazy upsert en el primer `GET /v1/auth-user/whatsapp/templates` post-deploy. NO se hace cron de bootstrap (KISS).

**Lógica** (en `handler::list_templates_handler`):
1. Si la query `?phone_number_id=X` matchea un `WaSettings` cuyo `templates_synced_at` está vacío o > 24h:
   - Llamar `service::list_templates(waba_id)` para fetchear de Meta.
   - Por cada template Meta no presente en `WaTemplates`:
     - `name = name_input = display_name = <name de Meta>`
     - `is_system = name.starts_with("sistema_abdo_") || name.starts_with("sistema_")`
     - `created_by = "00000000-0000-0000-0000-000000000000"` (sentinel "migración")
     - `created_by_name = "Migración"`
     - `submit_to_meta = true`
     - `meta_template_id = <id de Meta>` (Meta lo expone en la respuesta de `GET /{waba_id}/message_templates` desde Graph API v17+)
     - `body_placeholders = count {{N}} en BODY.text`
     - `status = <mapeado de Meta>`, `rejection_reason = null` (Meta no lo expone en GET, sólo en webhook)
   - Setear `WaSettings.templates_synced_at = now`.
2. En subsiguientes GETs, leer directo de `WaTemplates` sin tocar Meta.
3. El webhook `message_template_status_update` mantiene la DB actualizada.

> **NOTA migration.** Si Meta no devuelve `id` en `GET /{waba_id}/message_templates` (depende de versión Graph API), fallback: usar `name + "@" + language` como `meta_template_id` sintético. Verificar en implementación.

---

## 12. Decisiones pendientes

Estos puntos se resuelven en review del spec (antes o durante implementación):

1. **`ApiError` extension.** Agregar variante `Domain { status, code, field, message, details }` vs. extender `ValidationError`. **Voto:** variante nueva.
2. **`WhatsAppTemplate` actual.** Renombrar a `WhatsAppTemplateMetaRaw` y reusar para parsing interno, o eliminarlo del todo cuando el cache Redis se elimine. **Voto:** renombrar y reusar.
3. **Cache Redis (`get_templates`/`set_templates`).** Eliminar de una vez vs. dejar deprecado por 1 ciclo. **Voto:** eliminar.
4. **`templates_synced_at` en `WaSettings`.** Campo nuevo para gate de migración lazy. Confirmar OK con back.
5. **Editar APPROVED full (crear nueva versión deprecando vieja).** Fuera de scope v1. Si se quiere después, es un endpoint separado (`POST /:id/new-version`).
6. **`meta_edit_rate_limited` (429).** Documentar el header `Retry-After` que Meta devuelve cuando rate-limita el edit del BODY. Confirmar formato.
7. **`is_system` en migración.** Hoy se infiere por prefix. ¿Hay templates legacy SIN prefix `sistema_` que SÍ son del sistema? Si hay, hace falta lista manual. **Voto:** asumir que no hay, marcar todo lo no-prefijado como `is_system: false`. Reabrir si se descubre lo contrario.

---

## 13. Resumen de archivos a tocar

| Archivo | Cambio |
|---|---|
| `src/models/whatsapp.rs` | + `WaTemplate`, `WaTemplateItem`, `WaTemplateCategory`, `WaTemplateStatus`. Renombrar `WhatsAppTemplate` → `WhatsAppTemplateMetaRaw`. + `WebhookTemplateStatusValue` |
| `src/db/mod.rs` | + trait `WaTemplateRepository` (CRUD + search por phone+name+lang) |
| `src/db/mongo/whatsapp.rs` | + impl `WaTemplateRepository` |
| `src/modules/whatsapp/service.rs` | + `create_template_meta`, `update_template_meta_body`, `delete_template_meta(hsm_id, name)` |
| `src/modules/whatsapp/handler.rs` | Reemplazar `list_templates_handler` (cambia shape). + `get_template_handler`, `create_template_handler`, `update_template_handler`, `delete_template_handler`. + branch `message_template_status_update` en webhook |
| `src/modules/whatsapp/mod.rs` | Registrar 4 rutas nuevas |
| `src/modules/whatsapp/ws.rs` | + helper `emit_to_phone_number_agents(state, phone_number_id, event_json)` |
| `src/error.rs` | + variante `Domain { ..., details }` |
| `src/openapi.rs` | Registrar paths nuevos + schemas nuevos |
| `src/cache/redis_client.rs` | Eliminar `get_templates`/`set_templates` (cleanup) |
| `scripts/create_indexes.js` | + 5 índices §10 |
| `src/state.rs` o `src/models/whatsapp.rs` (`WaSettings`) | + campo `templates_synced_at: Option<DateTime>` |

---

---

## 14. Upload de media para headers de templates

Templates con header `IMAGE`, `VIDEO` o `DOCUMENT` requieren que Meta reciba un **handle** producido por su **Resumable Upload API** — NO es el media upload normal de WhatsApp (ése es sólo para mensajes).

Flujo de Meta (oficial, 2 pasos):
1. `POST graph.facebook.com/v22.0/{whatsapp_app_id}/uploads?file_length=X&file_type=Y` → devuelve `{ id: "upload:abc..." }`
2. `POST graph.facebook.com/v22.0/{upload_id}` con el binario → devuelve `{ h: "<handle>" }`

El `handle` es **single-use y corta vida** (~30 min). Se consume al crear/editar el template. NO se cachea.

### Decisiones cerradas

- **Persistencia del binario:** GridFS bucket `wa_template_media` (evita dependencia de S3, aprovecha MongoDB existente). Reutilizable entre re-creaciones y DRAFT → submit retroactivo.
- **Handle on-demand:** el endpoint de upload NO habla con Meta. Solo persiste binario + devuelve `media_id` nuestro. El swap a handle Meta ocurre dentro de `create_template_handler` y `update_template_handler` cuando detectan nuestros IDs en `components[].example.header_handle`.
- **Dedup por SHA-256:** si el mismo binario fue subido antes para el mismo `phone_number_id`, reusamos el `media_id` existente.
- **Meta App ID en env var** (`WHATSAPP_APP_ID`). Opcional — si falta, el endpoint responde `503 app_id_not_configured`.

### Endpoint

`POST /v1/auth-user/whatsapp/templates/header-media`

**Auth:** `user_jwt_auth_middleware` + `nRole == 0`.

**Request:** `multipart/form-data`

| Field | Tipo | Required | Descripción |
|---|---|---|---|
| `file` | File | sí | Binario. Stream o buffered en memoria según tamaño |
| `phone_number_id` | string | sí | Para scoping del dedup |
| `format` | string | sí | `IMAGE` \| `VIDEO` \| `DOCUMENT` |

**Response 200:**
```json
{
  "ok": true,
  "data": {
    "media_id": "65fc...e4",
    "mime_type": "image/jpeg",
    "file_size": 284719,
    "sha256": "a94f..."
  }
}
```

> **Sin `header_handle`, sin `expires_at`, sin `url`.** El `media_id` es el único ID que el front usa. El handle real se genera on-demand al crear el template.

**Errores:**

| Code | HTTP | Notas |
|---|---|---|
| `invalid_file_type` | 400 | `details: { allowed_mime_types: [...] }` según format |
| `file_too_large` | 413 | `details: { max_size: N, actual_size: M }` |
| `invalid_format` | 400 | `format` no es IMAGE/VIDEO/DOCUMENT |
| `phone_number_not_found` | 404 | `phone_number_id` no existe |
| `file_required` | 400 | multipart sin field `file` |
| `app_id_not_configured` | 503 | `WHATSAPP_APP_ID` no seteado en server |

### Mime whitelist y max sizes (impuestos por Meta)

| Format | Mime types aceptados | Max size |
|---|---|---|
| `IMAGE` | `image/jpeg`, `image/png` | 5 MB |
| `VIDEO` | `video/mp4`, `video/3gpp` | 16 MB |
| `DOCUMENT` | `application/pdf` | 100 MB |

### Uso desde el front

Al crear/editar template con header media:
```json
{
  "components": [
    {
      "type": "HEADER",
      "format": "IMAGE",
      "example": { "header_handle": ["<media_id_nuestro>"] }
    },
    ...
  ]
}
```

El back en `create_template_handler` / `update_template_handler`:
1. Antes de llamar a Meta, itera `components`.
2. Para cada HEADER con `format != TEXT`, valida que `example.header_handle[0]` sea un ObjectId nuestro existente en GridFS.
3. Fetch del binario, llamada a Resumable Upload API → obtiene `h`.
4. Reemplaza `example.header_handle[0] = h` (el handle Meta real).
5. Llama a `create_template_meta` / `update_template_body_meta` con el components ya swapeado.

Si el `media_id` no existe en GridFS → `400 invalid_component` con `details: { component_index, reason: "header_media_not_found" }`.

### Modelo Mongo (GridFS metadata)

GridFS en MongoDB almacena `fs.files` y `fs.chunks`. Usamos bucket custom `wa_template_media`:
- Colección `wa_template_media.files` — metadatos (filename, length, uploadDate, metadata).
- Colección `wa_template_media.chunks` — binario troceado.

En `metadata` del file doc guardamos:
```json
{
  "phone_number_id": "1234567890",
  "mime_type": "image/jpeg",
  "sha256": "a94f...",
  "format": "IMAGE",
  "uploaded_by": "uuid-...",
  "uploaded_by_name": "Juan Pérez"
}
```

Índice para dedup:
```js
db.wa_template_media.files.createIndex(
  { "metadata.phone_number_id": 1, "metadata.sha256": 1 },
  { unique: true, name: "idx_wa_template_media_phone_sha" }
);
```

### Repo trait `WaTemplateMediaRepository`

```rust
pub struct StoreTemplateMediaInput<'a> {
    pub phone_number_id: &'a str,
    pub format: &'a str,
    pub mime_type: &'a str,
    pub sha256: &'a str,
    pub bytes: &'a [u8],
    pub uploaded_by: &'a str,
    pub uploaded_by_name: &'a str,
}

#[async_trait::async_trait]
pub trait WaTemplateMediaRepository {
    /// Persiste el binario en GridFS. Si ya existe uno con mismo
    /// `(phone_number_id, sha256)`, devuelve el `media_id` existente (dedup).
    async fn store_template_media(&self, input: StoreTemplateMediaInput<'_>) -> Result<WaTemplateMediaRef, String>;
    async fn find_template_media_by_id(&self, id: &ObjectId) -> Result<Option<WaTemplateMediaRef>, String>;
    async fn read_template_media_bytes(&self, id: &ObjectId) -> Result<Option<(Vec<u8>, String)>, String>; // (bytes, mime)
    #[allow(dead_code)]
    async fn delete_template_media(&self, id: &ObjectId) -> Result<bool, String>;
}

pub struct WaTemplateMediaRef {
    pub id: ObjectId,
    pub phone_number_id: String,
    pub mime_type: String,
    pub sha256: String,
    pub file_size: u64,
}
```

### Service `upload_to_meta_resumable`

```rust
/// Los 2 pasos del Resumable Upload API. Devuelve el handle `h` que va en
/// `components[i].example.header_handle[0]`. NO cachear — single-use.
pub async fn upload_to_meta_resumable(
    &self,
    app_id: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<String, anyhow::Error> { ... }
```

Usa el mismo `access_token` que el resto de llamadas Meta del service.

### Validación extra en BODY (placeholder position)

Además de las validaciones de §9, agregar: el `body.text` **no puede empezar con `{{N}}` ni terminar con `{{N}}`** (Meta las rechaza). Si incumple → `invalid_component` con `details: { reason: "placeholder_at_edge" }`.

---

**Fin del spec.**
