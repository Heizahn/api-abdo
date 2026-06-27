# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

# api-abdo — Contexto del Proyecto

API REST construida en **Rust** con **Axum 0.7** para gestión de clientes ISP (Internet Service Provider). Incluye autenticación JWT, pagos, solvencia, dashboard, sincronización con equipos de red (MikroTik, ZTE OLT) y soporte WhatsApp via Meta Cloud API.

## Comandos de Desarrollo

```bash
# Chequear sin compilar (más rápido — preferir sobre build)
cargo check

# Compilar
cargo build

# Correr en desarrollo
cargo run

# Build de producción
cargo build --release

# Tests (todos)
cargo test

# Un solo test por nombre (o subcadena del path)
cargo test <nombre_test>
```

> En Windows: `cd "C:/Users/Humberto/Develop/api-abdo" && cargo check`

### Setup inicial de MongoDB (OBLIGATORIO)

El rendimiento depende de índices creados manualmente. Tras clonar el repo y antes del primer `cargo run`:

```bash
mongosh <MONGO_URI> < scripts/create_indexes.js
```

Sin los índices, queries sobre `Clients` / `Payments` / `Debts` degradan fuerte.

---

## Stack Tecnológico

- **Axum 0.7** + tower-http — router, middleware (CORS, compresión, tracing)
- **MongoDB 3.0** — base de datos principal
- **Redis 0.32** — cache, sesiones, carga de agentes WA, tasa BCV
- **tokio 1.39** — async runtime
- **serde/serde_json** — serialización JSON/BSON
- **jsonwebtoken 10.2** — JWT (dos tipos: clientes y staff)
- **reqwest 0.12** — HTTP client externo (BCV, Zabbix, Meta API)
- **utoipa 4** + utoipa-swagger-ui 7 — OpenAPI/Swagger en `/docs`
- **tower_governor + governor** — rate limiting
- **ssh2 0.9** — SSH a MikroTik / ZTE OLT
- **tracing + tracing-subscriber** — logs (JSON prod, pretty dev)
- **thiserror 2.0** — errores tipados; **anyhow 1.0** — propagación

---

## Estructura del Proyecto

```
src/
  main.rs              # Entrypoint: init config, conexiones, crons, servidor
  axum_router.rs       # Router: 4 grupos de auth + webhook + ws + static + docs
  openapi.rs           # ApiDoc: spec OpenAPI central
  state.rs             # AppState: MongoDB + Redis + Config + reqwest + WsRegistry
  config.rs            # Config desde env vars
  error.rs             # ApiError → { ok: false, error: "<code>" }

  auth/                # JWT claims, service de autenticación clientes
  middleware/          # jwt_auth_middleware, user_jwt_auth_middleware, rate_limit
  crypto/              # JWT, AES, verify
  cache/               # RedisClient wrapper (incluye agent load + ws locks)
  domain/              # Tipos de dominio (Customer, etc.)

  db/
    mod.rs             # Traits: AuthRepository, UserRepository, ProfileRepository,
                       #         SalesRepository, OnuRepository, UtilsRepository,
                       #         WhatsAppRepository, WaTemplateRepository,
                       #         WaTemplateMediaRepository, WaTicketRepository,
                       #         AiAgentRepository, AiConfigRepository,
                       #         AiInstallationRepository, AiPromotionRepository
                       #         + Db (master trait que combina todos)
    mongo/             # Implementaciones MongoDB por colección

  models/              # Structs compartidos (auth, db, payment, users, onu, profile,
                       #                      receivable, zabbix, whatsapp)

  utils/               # BCV scraper, timezone (VenezuelaDateTime), SMS, WhatsApp OTP, BSON helpers
  cron_bcv.rs          # Tarea periódica: actualizar tasa BCV en Redis

  modules/
    auth_client/       # POST /v1/auth/verify_number, /login, /refresh
    auth_user/         # POST /v1/auth-user/login, refresh-token | GET /me, check-reference
    clients/           # GET /v1/auth-user/clients/all, /:id, /contact-info
    payments/          # GET/POST /v1/payments/* (cliente) | /v1/auth-user/payments/* (admin)
    receivables/       # GET /v1/receivable/me, /me/paid, /:id, /:id/payments/rejected
    profile/           # GET /v1/profile/me/group, /v1/profile/me/phone
    dashboard/         # GET /v1/auth-user/dashboard/monthly-closing, /solvency, /latest-payments
    calculations/      # POST /v1/utils/calculate/bs, /v2/utils/calculate
    providers/         # GET /v1/users/providers
    users/             # PATCH /v1/auth-user/users/:id/password, /visible, /role
    api_utils/         # ping, latest-version, bcv, ip-pppoe, image, zabbix, banks
    whatsapp/
      mod.rs           # 3 grupos de rutas: webhook_routes, ws_routes, user_routes
      webhook/         # Handler del webhook de Meta (verify + receive), debug
      ws.rs            # WebSocket /v1/ws/chat (auth por cookie HttpOnly, ?token= solo compat)
      conversations/   # CRUD conversaciones: list, get, messages, take, transfer,
                       #   close, reopen, intervene, initiate, reset-ai-state,
                       #   transferable-agents, stats, client-link
      messaging/       # Media (upload/download/limits), reactions
      settings/        # CRUD de WaSettings ("WhatsApp Numbers") + test-connection
      templates/       # CRUD de WaTemplates locales + header-media + resync
      campaigns/       # Campañas masivas: preview, recipients, exclusions,
                       #   confirm, start, send
      quick_replies/   # Snippets de texto (CRUD + active + duplicate)
      tickets/         # Tickets de soporte derivados de chats (WaTickets)
      audit/           # Trazabilidad SUPERADMIN: messages, metrics, export, timeline
      assignment.rs    # Auto-asignación: min-load sobre agentes de wa_settings
      service.rs       # WhatsAppService: send_text, mark_as_read via Meta Cloud API
      backfill.rs      # Sync histórico de conversaciones desde Meta
      url_preview.rs   # Generación de previews para URLs en mensajes
      quick_reply_validation.rs  # Validación de respuestas rápidas
      tickets.rs       # Handlers de tickets (list, create, get, update, transfer-and-ticket)
    ai_agent/          # Asistente Virtual de WhatsApp (agent-centric, vía OpenRouter)
      handler.rs       # REST: agents CRUD + faqs + test-connection + metrics + config
      dispatch.rs      # Hook que dispara la IA al llegar un mensaje inbound (tokio::spawn)
      runner.rs        # Loop de turnos: user msg → rounds LLM con tool calls → respuesta
      tools.rs         # Tool registry + implementaciones (lookup_customer, get_invoices,
                       #   request_human, create_ticket, etc.)
      guardrails.rs    # Guardrails server-side para tool calls + bloque [turn_state]
      escalation.rs    # Auto-escalación IA → humano (ai_handoff, libera asignación)
      sandbox.rs       # POST /agents/:id/sandbox — turno completo sin persistir
      pre_classifier.rs
      openrouter.rs    # Cliente multimodal OpenRouter
      recovery.rs
      reference_normalize.rs
      state.rs         # Estado de conversación IA por turno
      business_data.rs
      config_resolver.rs
      seed.rs          # Seed de agentes/config inicial
    network/
      mikrotik/        # SSH: leases DHCP, IP PPPoE, cron (cada 20 min)
      zte/             # SSH: reporte ONUs ZTE OLT
    zabbix/            # Cliente HTTP Zabbix API: tráfico por ONU
```

---

## Patrones de Arquitectura

### Router (`axum_router.rs`)
`build_router` tiene **4 grupos de protección** más rutas estáticas y docs:

```
webhook   — /v1/webhook/whatsapp (GET verify + POST receive, sin JWT, sin rate limit)
ws        — /v1/ws/chat (JWT staff validado internamente; auth primaria por cookie HttpOnly,
            `?token=` sólo en ventana de compat temporal controlada por env)
public    — auth, calculations, api_utils públicos → rate limit
user_protected  — JWT staff/admin (user_jwt_auth_middleware)
client_protected — JWT cliente (jwt_auth_middleware)
```

### AppState (`state.rs`)
```rust
pub struct AppState {
    pub db: MongoDB,
    pub redis: RedisClient,
    pub config: Arc<Config>,
    pub reqwest_client: reqwest::Client,
    pub ws_registry: WsRegistry,  // Arc<RwLock<HashMap<user_id, Sender<String>>>>
}
```
`WsRegistry` mapea UUID de agente → canal `mpsc::Sender<String>` para enviar eventos JSON al WebSocket.

### Módulos de Feature
Cada feature en `modules/<nombre>/` es auto-contenida:
- `mod.rs` — declara sub-módulos y expone funciones `pub fn routes() -> Router<Arc<AppState>>`
- `handler.rs` — handlers HTTP con anotaciones `#[utoipa::path]`
- Sub-módulos opcionales según complejidad (services, parsers, crons)

### Dos tipos de JWT
- **Clientes**: `jwt_auth_middleware` — emitido en `/v1/auth/login`
- **Staff/Admin**: `user_jwt_auth_middleware` — emitido en `/v1/auth-user/login`

### Roles de usuario
- `nRole == -1` → sentinel "sin acceso" (bloqueo de login en `user_jwt_auth_middleware`)
- `nRole == 3.0` → provider (solo ve sus clientes via `idOwner == claims.id`)
- Otros roles → acceso completo, pueden filtrar con `?owner=<id>`
- El rol **no viene en el JWT**: se resuelve consultando `find_user_by_id` en cada request que lo necesite

### DB Layer
Traits en `db/mod.rs`, implementaciones en `db/mongo/`. Los módulos acceden via el trait `Db` (master trait que combina todos los repositorios). Nunca hay `$lookup` sobre colecciones grandes — se prefieren queries paralelas + join en Rust.

### Errores
`ApiError` implementa `IntoResponse` → siempre retorna `{ "ok": false, "error": "<code>" }`.

### WhatsApp — Patrones específicos

**Colecciones MongoDB** (PascalCase, como el resto del proyecto): `WaConversations`, `WaMessages`, `WaSettings`, `WaTemplates`, `WaTickets`, `AiAgents`, `AiConfig`, `AiInstallations`, `AiPromotions`, `AiInteractions`

**Webhook** (`POST /v1/webhook/whatsapp`): Meta siempre espera HTTP 200. El número de negocio se lee de `value.metadata.display_phone_number` (no del remitente) y se valida contra `WaSettings`.

**Auto-asignación** (`assignment.rs`): Al llegar un mensaje a una conversación sin agente, se dispara en `tokio::spawn`. Usa Redis para:
- `try_lock_conversation` — lock distribuido (evita duplicados en race conditions)
- `get_agent_load` / `incr_agent_load` / `decr_agent_load` — min-load sobre la lista de `agents` de `WaSettings`

**WebSocket** (`ws.rs`): `GET /v1/ws/chat`. Autenticación **primaria vía cookie HttpOnly** (`read_staff_access_token`); el query param `?token=` se acepta **solo** en la ventana de compat temporal habilitada por `AUTH_COMPAT_ALLOW_WS_QUERY` (hasta `AUTH_COMPAT_UNTIL`). JWT validado antes del upgrade; además se aplica un gate de acceso (usuario visible, no bot, `nRole != -1`, y `bCanChat` o rol interno elegible). Eventos JSON con discriminante `tipo`: `CONECTAR`, `SUSCRIBIR_CONVERSACION` (cliente→servidor) y `MENSAJE_NUEVO`, `CONVERSACION_NUEVA`, `CHAT_TOMADO`, `CHAT_TRANSFERIDO`, `CHAT_CERRADO`, `MENSAJE_ACTUALIZADO`, `ERROR`, `CONECTADO` (servidor→cliente).

**Tipos de mensajes WA soportados**: El webhook persiste `text`, `image`, `document`, `audio`, `video`, `sticker`, `location`, `contacts`, `interactive`, `button`, `order`, `system`, `referral`, `unsupported` y cualquier tipo nuevo/desconocido como mensaje genérico con `raw_payload`. Las reacciones (`reaction`) no crean mensaje nuevo: actualizan `WaMessage.reactions` del mensaje objetivo.

**Media inbound**: Si Meta entrega `media_id`, el mensaje se guarda primero y la descarga del binario se intenta después por prefetch/cache Redis (`prefetch_media`) o bajo demanda vía `GET /v1/auth-user/whatsapp/media/:media_id`. Si Meta reporta fallo de media inbound (`131052`, `131053`, `131056`) antes de que exista mensaje en DB, el backend re-chequea con delay y, si sigue ausente, crea un placeholder visible en el chat + avisa al cliente que reenvíe el archivo. NO asumir que “no llegó nada”: puede ser un fallo de procesamiento de Meta.

### AI Agent — Patrones específicos

El módulo `modules/ai_agent/` implementa el **Asistente Virtual de WhatsApp**: cada `AiAgent` lleva su `api_key`, modelo, system prompt, tools habilitadas y límites, y atiende conversaciones vía OpenRouter (multimodal). No hay recepcionista todavía: cada agente sirve directo.

**Configuración**: La `openrouter_api_key` y el `model` **no** son env vars — viven en la colección `AiConfig` (la api_key se cifra con AES-GCM usando `JWT_SECRET`, igual que el `access_token` de Meta). El secret de cifrado lo provee `ai_agent::ai_agent_secret()`.

**Dispatch** (`dispatch.rs`): Hook disparado al llegar un mensaje inbound de WhatsApp. Corre en `tokio::spawn` para no bloquear el webhook. Flujo: resolver agente activo del workspace → cargar conv + `WaSettings` (descifrar `access_token`) → descargar multimedia si el inbound es image/audio/video/document → construir history (últimos 20 textos) → cargar FAQs del agente → `run_turn` → persistir `AiInteraction` (siempre) → si `mode=live`: enviar respuesta por Meta + persistir `WaMessage` outbound + tocar la conv + broadcast WS.

**Runner** (`runner.rs`): Un turno = mensaje del cliente → uno o más roundtrips al LLM con tool calls intermedios → respuesta final en texto. Loop con `max_iterations`.

**Tools** (`tools.rs`): Registry con `build_function_declarations` + `execute_tool`. `ToolContext.is_sandbox` corta side-effects en escritura (`request_human`, `create_ticket` devuelven respuesta sintética sin tocar DB); las tools de lectura (`lookup_customer`, `get_invoices`) siempre pegan a DB.

**Guardrails** (`guardrails.rs`): Helpers puros (sin I/O) sobre tool calls + bloque `[turn_state]` del prompt. Toda la data viene precomputada por `dispatch.rs`.

**Escalación** (`escalation.rs`): Transición IA → humano. Actualiza conv (`ai_disabled=true`, limpia `ai_active_agent_id` / `ai_transfer_context`), libera asignación y persiste un `WaConversationEvent` con `event_type=ai_handoff`. La disparan la tool `request_human` y varios gates del dispatch (limit reached, keyword matched, critical failure).

**Sandbox** (`sandbox.rs`): `POST /v1/auth-user/whatsapp/ai-agent/agents/:agent_id/sandbox` ejecuta un turno completo con tools reales pero `is_sandbox=true` — no persiste `AiInteraction`, no crea tickets, no toca conversaciones. Sirve para validar system prompt + tools + api_key + relay antes de pasar a `mode=live`.

**Modos de agente**: `mode` distingue ejecución real (`live`) de prueba (`dry`/sandbox). Los modos `live` envían por Meta; el resto solo persiste la interacción para inspección.

**Relay**: Las llamadas a OpenRouter pasan por el mismo `RELAY_URL`/`RELAY_SECRET` que Meta cuando están seteados (ver variables de entorno). Imprescindible en VMs con ISPs que bloquean `openrouter.ai`.

**REST** (`handler.rs`): Rutas bajo `/v1/auth-user/whatsapp/ai-agent/` — CRUD de agentes, FAQs, `test-connection`, `metrics`, `config`, `export-package`/`import-package`, y `sandbox`. Todas requieren JWT staff/admin (colgadas en `user_protected`); varias son SUPERADMIN only.

---

## Documentación OpenAPI

La spec vive en `src/openapi.rs` y se sirve en:
- **`/docs`** — Swagger UI interactivo
- **`/docs/openapi.json`** — spec OpenAPI raw

### Agregar documentación a un handler nuevo (3 pasos)

**1. Modelo** — agregar `ToSchema`:
```rust
#[derive(Debug, Serialize, ToSchema)]
pub struct MiResponse { pub ok: bool, pub data: String }
```

**2. Handler** — macro encima de la función:
```rust
#[utoipa::path(
    post, path = "/v1/mi-ruta", tag = "Mi Módulo",
    security(("bearerAuth" = [])),   // omitir en rutas públicas
    request_body = MiRequest,        // omitir en GET
    responses(
        (status = 200, description = "OK", body = MiResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn mi_handler(...) { ... }
```

**3. `openapi.rs`** — registrar path + schemas:
```rust
paths(crate::modules::mi_modulo::handler::mi_handler, ...)
components(schemas(MiRequest, MiResponse, ...))
```

---

## Agregar un Módulo Nuevo

1. Crear `src/modules/<nombre>/mod.rs` con `pub fn routes() -> Router<Arc<AppState>>`
2. Crear `src/modules/<nombre>/handler.rs` con handlers y anotaciones `#[utoipa::path]`
3. Declarar en `src/modules/mod.rs`: `pub mod <nombre>;`
4. Registrar rutas en `src/axum_router.rs` en el grupo de protección correcto
5. Si necesita DB: agregar métodos al trait en `src/db/mod.rs` e implementar en `src/db/mongo/`
6. Registrar en `src/openapi.rs`: paths + schemas

---

## Variables de Entorno (`.env`)

| Variable | Descripción |
|---|---|
| `MONGO_URI` | URI de conexión a MongoDB |
| `MONGO_DB` | Nombre de la DB (default `test`) |
| `MONGO_POOL_SIZE` / `MONGO_MIN_POOL_SIZE` / `MONGO_CONNECT_TIMEOUT` | Tuning del pool MongoDB |
| `REDIS_URI` | URI de conexión a Redis |
| `REDIS_POOL_SIZE` / `REDIS_EXCHANGE_RATE_TTL` | Tuning Redis y TTL de tasa BCV |
| `JWT_SECRET` | Secreto para tokens de clientes **y** clave AES-GCM para cifrar `access_token` de Meta y `openrouter_api_key` de IA |
| `JWT_USER_SECRET` | Secreto para tokens de staff/admin |
| `RUST_LOG` | Nivel de log (`info`, `debug`, etc.) |
| `LOG_FORMAT` | `json` (prod) o `pretty` (dev) |
| `HOST` / `PORT` | Binding del servidor |
| `PORT_MK` / `PASS_MK` | Credenciales SSH MikroTik |
| `OLT_ZTE_PASS` | Password SSH ZTE OLT |
| `ZABBIX_URL` / `ZABBIX_TOKEN` | API Zabbix |
| `ID_SIMCOT` | ID del editor para operaciones automáticas (crons) |
| `RATE_LIMIT_AUTH_PER_MINUTE` | Límite de requests/min en rutas de auth públicas |
| `FRONTEND_ORIGINS` | CSV de orígenes permitidos para CORS. Si está vacío, se usa `allow_origin(Any)` |
| `CORS_ALLOW_CREDENTIALS` | Bool (default `true`) — se omite si `FRONTEND_ORIGINS` está vacío para evitar `*` con credenciales |
| `AUTH_COOKIE_SECURE` | Bool (default `true`) — cookie HttpOnly `secure` |
| `AUTH_COOKIE_SAME_SITE` / `AUTH_COOKIE_DOMAIN` | Atributos de la cookie de auth |
| `AUTH_COMPAT_ALLOW_REFRESH_BODY` | Bool (default `true`) — compat: aceptar refresh token en body |
| `AUTH_COMPAT_ALLOW_WS_QUERY` | Bool (default `true`) — compat: aceptar `?token=` en el WS |
| `AUTH_COMPAT_UNTIL` | Fecha límite de la ventana de compat (`?token=` en WS) |
| `WHATSAPP_VERIFY_TOKEN` | Token de verificación del webhook de Meta (handshake GET) |
| `WHATSAPP_APP_SECRET` | Secret de la Meta App — valida la firma HMAC-SHA256 del webhook |
| `WHATSAPP_APP_ID` | (opcional) ID numérico de la Meta App — usado por la Resumable Upload API para subir media de headers de templates. Si falta, `POST /v1/auth-user/whatsapp/templates/header-media` responde 503 `app_id_not_configured` |
| `RELAY_URL` | (opcional) URL del Cloudflare Worker relay genérico — ver `tools/cf-worker-media-relay/`. Cuando está seteada, todas las llamadas a hosts externos (Meta `lookaside.fbsbx.com`, OpenRouter `openrouter.ai`) pasan por el Worker en vez de conectar directo. El Worker valida los hosts contra su `ALLOWED_HOST_SUFFIXES`. Imprescindible cuando el back corre en una VM con ISP que bloquea esos hosts (Venezuela). Acepta los nombres legacy `WA_MEDIA_RELAY_URL` como fallback durante la transición |
| `RELAY_SECRET` | (opcional) Secret compartido con el Worker. Acepta el nombre legacy `WA_MEDIA_RELAY_SECRET` como fallback. Si la URL está seteada y el secret no, las llamadas se hacen directo (no se usa el relay) |

> El `access_token` y `phone_number_id` de Meta Cloud API **no** son env vars: se
> configuran por cuenta en la colección `WaSettings` (el token se guarda cifrado
> con AES-GCM usando `JWT_SECRET` como clave). La UI de "WhatsApp Numbers" los
> administra vía `POST/PUT /v1/auth-user/whatsapp/settings`.
>
> La `openrouter_api_key` y el `model` del AI Agent **tampoco** son env vars:
> viven en la colección `AiConfig` (api_key cifrada con AES-GCM usando
> `JWT_SECRET`). El secreto lo provee `ai_agent::ai_agent_secret()`.


## Flujo de despliegue / pruebas
- El trabajo normal se sube a `develop` para probar en la VM de desarrollo.
- La VM de desarrollo usa un número de WhatsApp de prueba simulando producción, aislado de producción real.
- Producción atiende ~8000 clientes; desarrollo debe probarse solo contra los clientes de prueba disponibles.
- Antes de tocar lógica de IA/WhatsApp/pagos, confirmar el plan de prueba esperado en la VM de desarrollo y qué conversación/número de prueba se usará.

## Regla obligatoria de entrega
- No editar archivos ni versionar solo por preparar un plan/análisis, salvo autorización explícita del usuario.
- Para cambios funcionales del sistema Rust/API/IA/WhatsApp/pagos: subir versionado siguiendo SemVer pre-1.0 (`Cargo.toml` + `Cargo.lock`; OpenAPI/log de arranque si aplica), hacer commit y push a la rama en que se esté trabajando.
- Cambios solo documentales, planes o instrucciones internas no requieren bump de versión, salvo que el usuario lo pida explícitamente.
- No terminar una tarea con cambios locales sin push, salvo que el usuario lo prohíba explícitamente.
