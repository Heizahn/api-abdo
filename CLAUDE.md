# api-abdo - Contexto del Proyecto

API REST construida en **Rust** con **Axum 0.7** para gestión de clientes ISP (Internet Service Provider). Incluye autenticación JWT, pagos, solvencia, dashboard y sincronización con equipos de red (MikroTik, ZTE OLT).

## Stack Tecnologico

### Framework Web
- `axum 0.7` + `axum-extra 0.9` — router principal, extractors, typed headers
- `tower 0.5` / `tower-http 0.6` — middleware stack (CORS, compresion, tracing, archivos estaticos)
- `tower_governor 0.3` + `governor 0.6` — rate limiting
- `tokio 1.39` — async runtime
- `hyper 1` — HTTP subyacente

### Base de Datos
- `mongodb 3.0` — base de datos principal (colecciones: clientes, pagos, cuentas por cobrar, ONUs, usuarios)
- `redis 0.32` con `connection-manager` — cache, sesiones, tokens refresh, tasa BCV

### Serializacion
- `serde 1.0` + `serde_json 1.0` — serializar/deserializar structs <-> JSON/BSON

### Autenticacion y Criptografia
- `jsonwebtoken 10.2` — JWT con `rust_crypto`
- `aes-gcm 0.10` — cifrado simétrico AES-256-GCM
- `bcrypt 0.18` — hashing de contraseñas
- `hmac 0.12` + `sha2 0.10` — HMAC-SHA256
- `base64 0.22` — encoding

### HTTP Client
- `reqwest 0.12` con feature `json` — llamadas externas (BCV, Zabbix, SMS)

### Utilidades
- `dotenvy 0.15` — variables de entorno desde `.env`
- `uuid 1.18` (v4, serde) — IDs únicos
- `chrono 0.4` + `chrono-tz 0.10` — fechas/horas con timezone (America/Caracas)
- `async-trait 0.1` — traits async
- `futures 0.3` — combinators async
- `regex 1.10` — validaciones
- `ssh2 0.9` — conexion SSH a MikroTik
- `scraper 0.25` — scraping HTML del BCV

### Observabilidad
- `tracing 0.1` + `tracing-subscriber 0.3` — logs estructurados (JSON en prod, pretty en dev)

### Manejo de Errores
- `thiserror 2.0` — errores tipados con derive
- `anyhow 1.0` — propagacion de errores en capas de infraestructura

## Estructura del Proyecto

```
src/
  main.rs              # Entrypoint: init config, conexiones, crons, servidor
  axum_router.rs       # Router: rutas publicas, protegidas cliente, protegidas admin
  state.rs             # AppState: MongoDB + Redis + Config + reqwest::Client
  config.rs            # Config desde env vars
  error.rs             # AppError enum -> respuestas HTTP

  auth/                # Logica de autenticacion clientes
  middleware/          # jwt_auth_middleware, user_jwt_auth_middleware, rate_limit
  handlers/            # Handlers HTTP (auth, payment, receivable, dashboard, etc.)
  models/              # Structs de request/response y modelos de BD
  db/mongo/            # Acceso a MongoDB por dominio
  services/            # Logica de negocio (MikroTik, ZTE, IP PPPoE, Zabbix)
  crypto/              # JWT, AES, verify
  cache/               # RedisClient wrapper
  utils/               # BCV scraper, timezone, SMS, bancos, BSON helpers
  domain/              # Tipos de dominio (Customer, etc.)
  cron_bcv.rs          # Tarea periodica: actualizar tasa BCV desde redis
  cron_mikrotik.rs     # Tarea periodica: sincronizar clientes MikroTik
  cron_zte.rs          # Tarea periodica: sincronizar ONUs ZTE (desactivado)
```

## Patrones de Arquitectura

- **AppState** compartido via `Arc<AppState>` inyectado en todos los handlers
- **Dos tipos de JWT**: clientes (`jwt_auth_middleware`) y staff/admin (`user_jwt_auth_middleware`)
- **Roles de usuario**: `owner`, `admin`, `staff` — los handlers de dashboard filtran por owner
- **Rutas versionadas**: `/v1/...` y `/v2/...`
- **Errores**: `AppError` implementa `IntoResponse` para retornar JSON con status HTTP

## Comandos de Desarrollo

```bash
# Compilar
cargo build

# Correr en desarrollo
cargo run

# Tests
cargo test

# Chequear sin compilar
cargo check
```

## Variables de Entorno (.env)

El proyecto usa `dotenvy`. Variables principales:
- `MONGO_URI` — URI de conexion a MongoDB
- `REDIS_URI` — URI de conexion a Redis
- `JWT_SECRET` — secreto para tokens de clientes
- `JWT_USER_SECRET` — secreto para tokens de staff/admin
- `RUST_LOG` — nivel de log (ej: `info`, `debug`)
- `LOG_FORMAT` — `json` (prod) o `pretty` (dev)
- `HOST` / `PORT` — binding del servidor
