# api-abdo — Contexto del Proyecto

API REST construida en **Rust** con **Axum 0.7** para gestión de clientes ISP (Internet Service Provider). Incluye autenticación JWT, pagos, solvencia, dashboard y sincronización con equipos de red (MikroTik, ZTE OLT).

## Stack Tecnológico

### Framework Web
- `axum 0.7` + `axum-extra 0.9` — router principal, extractors, typed headers
- `tower 0.5` / `tower-http 0.6` — middleware stack (CORS, compresión, tracing)
- `tower_governor 0.3` + `governor 0.6` — rate limiting
- `tokio 1.39` — async runtime
- `hyper 1` — HTTP subyacente

### Base de Datos
- `mongodb 3.0` — base de datos principal (colecciones: clientes, pagos, cuentas por cobrar, ONUs, usuarios)
- `redis 0.32` con `connection-manager` — cache, sesiones, tokens refresh, tasa BCV

### Serialización
- `serde 1.0` + `serde_json 1.0` — serializar/deserializar structs <-> JSON/BSON

### Autenticación y Criptografía
- `jsonwebtoken 10.2` — JWT con `rust_crypto`
- `aes-gcm 0.10` — cifrado simétrico AES-256-GCM
- `bcrypt 0.18` — hashing de contraseñas
- `hmac 0.12` + `sha2 0.10` — HMAC-SHA256
- `base64 0.22` — encoding

### HTTP Client
- `reqwest 0.12` con feature `json` — llamadas externas (BCV, Zabbix, SMS)

### Documentación
- `utoipa 4` con features `axum_extras`, `chrono`, `uuid` — anotaciones OpenAPI en handlers y modelos
- `utoipa-swagger-ui 7` con feature `axum` — Swagger UI en `/docs`

### Utilidades
- `dotenvy 0.15` — variables de entorno desde `.env`
- `uuid 1.18` (v4, serde) — IDs únicos
- `chrono 0.4` + `chrono-tz 0.10` — fechas/horas con timezone (America/Caracas)
- `async-trait 0.1` — traits async
- `futures 0.3` — combinators async
- `regex 1.10` — validaciones
- `ssh2 0.9` — conexión SSH a MikroTik / ZTE OLT
- `scraper 0.25` — scraping HTML del BCV

### Observabilidad
- `tracing 0.1` + `tracing-subscriber 0.3` — logs estructurados (JSON en prod, pretty en dev)

### Manejo de Errores
- `thiserror 2.0` — errores tipados con derive
- `anyhow 1.0` — propagación de errores en capas de infraestructura

---

## Estructura del Proyecto

```
src/
  main.rs              # Entrypoint: init config, conexiones, crons, servidor
  axum_router.rs       # Router: orquestador delgado, hace merge de rutas por grupo de auth
  openapi.rs           # ApiDoc: spec OpenAPI central (paths + schemas de todos los módulos)
  state.rs             # AppState: MongoDB + Redis + Config + reqwest::Client
  config.rs            # Config desde env vars
  error.rs             # ApiError enum -> respuestas HTTP JSON { ok: false, error: "..." }

  # Infraestructura (cross-cutting, sin dependencias entre features)
  auth/                # JWT claims, service de autenticación clientes
  middleware/          # jwt_auth_middleware, user_jwt_auth_middleware, rate_limit
  crypto/              # JWT, AES, verify
  cache/               # RedisClient wrapper
  domain/              # Tipos de dominio (Customer, etc.)

  # Capa de datos (traits + implementaciones MongoDB)
  db/
    mod.rs             # Traits: AuthRepository, ProfileRepository, SalesRepository, etc.
    mongo/             # Implementaciones MongoDB por colección

  # Modelos compartidos (usados por db/ y múltiples módulos)
  models/
    auth.rs            # VerifyNumberRequest/Response, LoginRequest/Response, RefreshRequest/Response, TokenPair
    db.rs              # Client, Debt, Payment, Tax, ClientListItem, ClientDetail, ClientOnu
    payment.rs         # PaymentReport, PaymentMethod, Bank, CheckReferenceRequest/Response
    users.rs           # User, UserLoginRequest/Response, UserResponse, RefreshTokenRequest/Response
    onu.rs             # ONU structs
    zabbix.rs          # ZabbixTrafficResponse, MonthlyTraffic
    profile.rs         # ClientSummary, MeGroupResponse, etc.
    receivable.rs      # ReceivableData, RejectedPayment, etc.

  # Utilidades compartidas
  utils/               # BCV scraper, timezone (VenezuelaDateTime), SMS, WhatsApp OTP, BSON helpers
  cron_bcv.rs          # Tarea periódica: actualizar tasa BCV en Redis

  # Módulos de feature (cada uno auto-contenido: handler + rutas propias)
  modules/
    mod.rs             # pub mod de todos los módulos

    auth_client/       # Autenticación de clientes via teléfono + OTP
      mod.rs           # pub fn routes() -> Router
      handler.rs       # POST /v1/auth/verify_number, /v1/auth/login, /v1/auth/refresh

    auth_user/         # Autenticación de staff/admin
      mod.rs           # pub fn public_routes(), pub fn protected_routes()
      handler.rs       # POST /v1/auth-user/login, refresh-token | GET /v1/auth-user/me, check-reference

    clients/           # Gestión de clientes ISP
      mod.rs           # pub fn routes()
      handler.rs       # GET /v1/auth-user/clients/all, /:id, /contact-info | /v1/clients/:id/status-history

    payments/          # Pagos y reportes de pago
      mod.rs           # pub fn client_routes(), pub fn user_routes()
      handler.rs       # GET/POST /v1/payments/* (cliente) | /v1/auth-user/payments/* (admin)

    receivables/       # Cuentas por cobrar (deudas)
      mod.rs           # pub fn routes()
      handler.rs       # GET /v1/receivable/me, /me/paid, /:id, /:id/payments/rejected

    profile/           # Perfil del cliente (app móvil)
      mod.rs           # pub fn routes()
      handler.rs       # GET /v1/profile/me/group, /v1/profile/me/phone

    dashboard/         # Dashboard admin: cierre mensual, solvencia, últimos pagos
      mod.rs           # pub fn routes()
      handler.rs       # GET /v1/auth-user/dashboard/monthly-closing, /solvency, /latest-payments

    calculations/      # Cálculo USD <-> BS con tasa BCV
      mod.rs           # pub fn routes()
      handler.rs       # POST /v1/utils/calculate/bs, /v2/utils/calculate

    providers/         # Listado de proveedores
      mod.rs           # pub fn routes()
      handler.rs       # GET /v1/users/providers

    api_utils/         # Endpoints utilitarios (BCV, ping, imágenes, bancos, IP PPPoE, Zabbix)
      mod.rs           # pub fn public_routes(), user_routes(), client_routes(), static_routes()
      handler.rs       # GET /v1/utils/ping, /latest-version, /bcv, /ip-pppoe/:sn, /image/:filename,
                       #     /zabbix/:id_client, /utils/list/banks, /auth-user/utils/list/banks,
                       #     /v1/privacy-policy

    network/           # Integraciones con equipos de red (sin rutas HTTP propias)
      mikrotik/
        service.rs     # SSH: descarga de leases DHCP
        parser.rs      # Parseo de leases y detección de cambios de IP
        ip_pppoe.rs    # Búsqueda de IP PPPoE activa por SN (usada por clients/ y api_utils/)
        cron.rs        # Tarea periódica: sincronizar IPs ONU cada 20 min
      zte/
        service.rs     # SSH: descarga de reporte ONUs ZTE OLT
        parser.rs      # Parseo del reporte y detección de cambios (exporta OnuDetected)
        cron.rs        # Tarea periódica: sincronizar ONUs ZTE (desactivado)

    zabbix/            # Integración con Zabbix para tráfico de red
      service.rs       # Cliente HTTP Zabbix API: historial de tráfico por ONU
```

---

## Patrones de Arquitectura

### Router (`axum_router.rs`)
`build_router` es un orquestador delgado que agrupa módulos por nivel de protección y aplica middleware:

```rust
// Rutas públicas + rate limit
Router::new()
    .merge(auth_client::routes())
    .merge(auth_user::public_routes())
    .merge(calculations::routes())
    .merge(api_utils::public_routes())
    .layer(auth_rate_limit)

// Rutas protegidas con JWT de staff/admin
Router::new()
    .merge(auth_user::protected_routes())
    .merge(clients::routes())
    .merge(dashboard::routes())
    // ...
    .route_layer(middleware::from_fn_with_state(state, user_jwt_auth_middleware))

// Rutas protegidas con JWT de cliente
Router::new()
    .merge(profile::routes())
    .merge(receivables::routes())
    // ...
    .route_layer(middleware::from_fn_with_state(state, jwt_auth_middleware))
```

Cada módulo expone una o varias funciones `pub fn routes() -> Router<Arc<AppState>>` en su `mod.rs`.

### Módulos de Feature
Cada feature en `modules/<nombre>/` es auto-contenida:
- `mod.rs` — declara sub-módulos y expone las funciones de rutas
- `handler.rs` — handlers HTTP con anotaciones `#[utoipa::path]`
- Sub-módulos opcionales según complejidad (services, parsers, crons)

### AppState
Compartido via `Arc<AppState>` inyectado en todos los handlers. Contiene MongoDB, Redis, Config y el cliente HTTP `reqwest`.

### Dos tipos de JWT
- **Clientes**: `jwt_auth_middleware` — emitido en `/v1/auth/login`
- **Staff/Admin**: `user_jwt_auth_middleware` — emitido en `/v1/auth-user/login`

### Roles de usuario
- `nRole == 3.0` → provider (solo ve sus clientes via `idOwner == claims.id`)
- Otros roles → acceso completo, pueden filtrar con `?owner=<id>`

### DB Layer
Traits en `db/mod.rs`, implementaciones en `db/mongo/`. Los módulos acceden via el trait `Db` (master trait que combina todos los repositorios).

### Errores
`ApiError` implementa `IntoResponse` → siempre retorna `{ "ok": false, "error": "<code>" }` con el status HTTP correspondiente.

---

## Documentación OpenAPI

La spec vive en `src/openapi.rs` y se sirve en:
- **`/docs`** — Swagger UI interactivo (público, sin JWT)
- **`/docs/openapi.json`** — spec OpenAPI raw

### Agregar documentación a un handler nuevo (3 pasos)

**1. Modelo** — agregar `ToSchema` al derive:
```rust
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct MiResponse {
    pub ok: bool,
    /// Descripción inline del campo (opcional)
    pub data: String,
}
```

**2. Handler** — agregar macro encima de la función:
```rust
#[utoipa::path(
    post,                              // método HTTP
    path = "/v1/mi-ruta",
    tag = "Mi Módulo",                 // agrupa en Swagger UI
    request_body = MiRequest,          // omitir si es GET
    responses(
        (status = 200, description = "OK", body = MiResponse),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "No encontrado"),
    )
)]
pub async fn mi_handler(...) { ... }
```

Para rutas con JWT, agregar `security`:
```rust
#[utoipa::path(
    get,
    path = "/v1/mi-ruta-protegida",
    tag = "Mi Módulo",
    security(("bearerAuth" = [])),
    responses(...)
)]
```

**3. `openapi.rs`** — registrar path + schemas:
```rust
paths(
    // ... paths existentes ...
    crate::modules::mi_modulo::handler::mi_handler,
),
components(schemas(
    // ... schemas existentes ...
    MiRequest, MiResponse,
))
```

---

## Agregar un Módulo Nuevo

Para agregar una feature completa (ej: WhatsApp, banking, reportes PDF):

1. Crear `src/modules/<nombre>/mod.rs`:
   ```rust
   pub mod handler;

   use axum::{routing::{get, post}, Router};
   use crate::state::AppState;
   use std::sync::Arc;

   pub fn routes() -> Router<Arc<AppState>> {
       Router::new()
           .route("/v1/<nombre>/...", get(handler::mi_handler))
   }
   ```

2. Crear `src/modules/<nombre>/handler.rs` con los handlers HTTP y anotaciones `#[utoipa::path]`

3. Declarar el módulo en `src/modules/mod.rs`:
   ```rust
   pub mod <nombre>;
   ```

4. Registrar las rutas en `src/axum_router.rs` en el grupo de protección correcto:
   ```rust
   .merge(<nombre>::routes())
   ```

5. Si necesita DB: agregar métodos al trait correspondiente en `src/db/mod.rs` e implementar en `src/db/mongo/`

6. Registrar en `src/openapi.rs`: paths + schemas del módulo nuevo

---

## Comandos de Desarrollo

```bash
# Chequear sin compilar (más rápido)
cargo check

# Compilar
cargo build

# Correr en desarrollo
cargo run

# Tests
cargo test
```

> En Windows usar `cd "C:/Users/Humberto/Develop/api-abdo" && cargo check`

---

## Variables de Entorno (`.env`)

El proyecto usa `dotenvy`. Variables principales:

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
