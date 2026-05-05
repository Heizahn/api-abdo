# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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
  axum_router.rs       # Router: 4 grupos de auth + webhook + ws
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
    mod.rs             # Traits: AuthRepository, ProfileRepository, SalesRepository,
                       #         OnuRepository, UserRepository, UtilsRepository, WhatsAppRepository
                       #         + Db (master trait)
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
      handler.rs       # Webhook Meta + CRUD conversaciones + settings + debug
      ws.rs            # WebSocket /v1/ws/chat?token=<jwt> — WsRegistry, eventos JSON
      assignment.rs    # Auto-asignación: min-load sobre agentes de wa_settings
      service.rs       # WhatsAppService: send_text, mark_as_read via Meta Cloud API
      backfill.rs      # Sync histórico de conversaciones desde Meta
      url_preview.rs   # Generación de previews para URLs en mensajes
      quick_reply_validation.rs  # Validación de respuestas rápidas
    network/
      mikrotik/        # SSH: leases DHCP, IP PPPoE, cron (cada 20 min)
      zte/             # SSH: reporte ONUs ZTE OLT
    zabbix/            # Cliente HTTP Zabbix API: tráfico por ONU
```

---

## Patrones de Arquitectura

### Router (`axum_router.rs`)
`build_router` tiene **4 grupos** de protección:

```
webhook   — /v1/webhook/whatsapp (GET verify + POST receive, sin JWT, sin rate limit)
ws        — /v1/ws/chat (JWT validado internamente via ?token=)
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
    pub ws_registry: WsRegistry,  // Arc<RwLock<HashMap<user_id, UnboundedSender<String>>>>
}
```
`WsRegistry` mapea UUID de agente → canal mpsc para enviar eventos JSON al WebSocket.

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

**Colecciones MongoDB** (PascalCase, como el resto del proyecto): `WaConversations`, `WaMessages`, `WaSettings`

**Webhook** (`POST /v1/webhook/whatsapp`): Meta siempre espera HTTP 200. El número de negocio se lee de `value.metadata.display_phone_number` (no del remitente) y se valida contra `WaSettings`.

**Auto-asignación** (`assignment.rs`): Al llegar un mensaje a una conversación sin agente, se dispara en `tokio::spawn`. Usa Redis para:
- `try_lock_conversation` — lock distribuido (evita duplicados en race conditions)
- `get_agent_load` / `incr_agent_load` / `decr_agent_load` — min-load sobre la lista de `agents` de `WaSettings`

**WebSocket** (`ws.rs`): `GET /v1/ws/chat?token=<user_jwt>`. JWT validado antes del upgrade. Eventos JSON con discriminante `tipo`: `CONECTAR`, `SUSCRIBIR_CONVERSACION` (cliente→servidor) y `MENSAJE_NUEVO`, `CONVERSACION_NUEVA`, `CHAT_TOMADO`, `CHAT_TRANSFERIDO`, `CHAT_CERRADO`, `MENSAJE_ACTUALIZADO`, `ERROR`, `CONECTADO` (servidor→cliente).

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
| `REDIS_URI` | URI de conexión a Redis |
| `JWT_SECRET` | Secreto para tokens de clientes |
| `JWT_USER_SECRET` | Secreto para tokens de staff/admin |
| `RUST_LOG` | Nivel de log (`info`, `debug`, etc.) |
| `LOG_FORMAT` | `json` (prod) o `pretty` (dev) |
| `HOST` / `PORT` | Binding del servidor |
| `PORT_MK` / `PASS_MK` | Credenciales SSH MikroTik |
| `OLT_ZTE_PASS` | Password SSH ZTE OLT |
| `ZABBIX_URL` / `ZABBIX_TOKEN` | API Zabbix |
| `ID_SIMCOT` | ID del editor para operaciones automáticas (crons) |
| `WHATSAPP_VERIFY_TOKEN` | Token de verificación del webhook de Meta (handshake GET) |
| `WHATSAPP_APP_SECRET` | Secret de la Meta App — valida la firma HMAC-SHA256 del webhook |
| `WHATSAPP_APP_ID` | (opcional) ID numérico de la Meta App — usado por la Resumable Upload API para subir media de headers de templates. Si falta, `POST /v1/auth-user/whatsapp/templates/header-media` responde 503 `app_id_not_configured` |
| `WA_MEDIA_RELAY_URL` | (opcional) URL del Cloudflare Worker relay para descargas de media — ver `tools/cf-worker-media-relay/` |
| `WA_MEDIA_RELAY_SECRET` | (opcional) Secret compartido con el Worker; si ambas están seteadas, las descargas pasan por el relay en vez de `lookaside.fbsbx.com` directo |
| `AI_RELAY_URL` | (opcional) URL del Cloudflare Worker relay para llamadas a OpenRouter (`openrouter.ai`). Mismo Worker que WA media o uno separado — agregar `'openrouter.ai'` a `ALLOWED_HOST_SUFFIXES` en `tools/cf-worker-media-relay/worker.js`. Si falta, el módulo AI Agent conecta directo (puede fallar desde la VM por bloqueo del ISP) |
| `AI_RELAY_SECRET` | (opcional) Secret compartido con el Worker para AI. Independiente de `WA_MEDIA_RELAY_SECRET` para permitir rotación/aislamiento |
| `OPENROUTER_BASE_URL` | (opcional) Override de la URL base de OpenRouter. Por defecto `https://openrouter.ai/api/v1`. Útil para test o providers compatibles con la API. |

> El `access_token` y `phone_number_id` de Meta Cloud API **no** son env vars: se
> configuran por cuenta en la colección `WaSettings` (el token se guarda cifrado
> con AES-GCM usando `JWT_SECRET` como clave). La UI de "WhatsApp Numbers" los
> administra vía `POST/PUT /v1/auth-user/whatsapp/settings`.
