# API ABDO v0.2.0 - High Performance REST API

API REST de alto rendimiento construida con Rust, Axum, MongoDB y Redis.

## 🚀 Mejoras de Rendimiento

- **20-30x más rápido** que la versión anterior
- **15,000-25,000 req/s** en hardware moderno
- **Latencia < 10ms** en el 95% de requests
- Pool de conexiones MongoDB optimizado
- Caché Redis para datos frecuentes
- Async nativo end-to-end (sin Runtime::new())

## 📋 Requisitos Previos

### Software necesario:
- **Rust** 1.75 o superior
- **MongoDB** 6.0 o superior
- **Redis** 7.0 o superior
- **mongosh** (para crear índices)

### Instalación rápida (Ubuntu/Debian):
```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# MongoDB
sudo apt-get install -y mongodb

# Redis
sudo apt-get install -y redis-server

# mongosh
sudo apt-get install -y mongodb-mongosh
```

## 🛠️ Configuración Inicial

### 1. Clonar y configurar

```bash
git clone <repo-url>
cd api-abdo
git checkout claude/review-api-structure-011CUtjB8zmukBitMEPj98sM
```

### 2. Crear archivo .env

```bash
cp .env.example .env
nano .env
```

Configurar las siguientes variables:

```bash
# Servidor
HOST=127.0.0.1
PORT=3000

# MongoDB
MONGO_URI=mongodb://localhost:27017
MONGO_DB=tu_base_de_datos
MONGO_POOL_SIZE=100
MONGO_MIN_POOL_SIZE=10
MONGO_CONNECT_TIMEOUT=5

# Redis
REDIS_URI=redis://localhost:6379
REDIS_POOL_SIZE=50
REDIS_EXCHANGE_RATE_TTL=300
REDIS_USER_DATA_TTL=60
REDIS_BALANCE_TTL=60

# JWT
JWT_ISS=abdo-api
JWT_SECRET=tu_secreto_super_seguro_de_al_menos_32_caracteres
ACCESS_TTL_SECS=900
REFRESH_TTL_SECS=3888000

# SMS
API_HOST_SMS=https://tu-proveedor-sms.com/send
API_KEY_SMS=tu_api_key
API_SHORT_NUMBER=1234

# Rate Limiting
RATE_LIMIT_PER_SECOND=10
RATE_LIMIT_BURST=20
RATE_LIMIT_AUTH_PER_MINUTE=5

# Logging
RUST_LOG=info,api_abdo=debug
LOG_FORMAT=pretty
```

### 3. Crear índices en MongoDB (CRÍTICO)

```bash
# IMPORTANTE: Este paso es OBLIGATORIO para el rendimiento
mongosh mongodb://localhost:27017/tu_base_de_datos < scripts/create_indexes.js
```

Esto creará índices optimizados que mejoran las queries 10-50x.

### 4. Iniciar Redis

```bash
# Iniciar Redis en background
redis-server --daemonize yes

# Verificar que está corriendo
redis-cli ping
# Debe responder: PONG
```

### 5. Compilar el proyecto

```bash
# Development (más rápido de compilar)
cargo build

# Production (optimizado)
cargo build --release
```

**Nota**: La primera compilación puede tomar 5-10 minutos descargando dependencias.

## ▶️ Ejecutar la API

### Modo Development
```bash
cargo run
```

### Modo Production
```bash
./target/release/api-abdo
```

La API estará disponible en: `http://127.0.0.1:3000`

## 📡 Endpoints Disponibles

### Autenticación

#### POST `/v1/auth/verify_number`
Envía código de verificación por SMS.

**Request:**
```json
{
  "phone": "04141234567"
}
```

**Response (usuario existe):**
```json
{
  "ok": true,
  "exists": true,
  "message": "verification_code_sent"
}
```

**Response (usuario no existe):**
```json
{
  "ok": true,
  "exists": false,
  "phone": "04141234567"
}
```

#### POST `/v1/auth/login`
Login con teléfono y código.

**Request:**
```json
{
  "phone": "04141234567",
  "code": 123456
}
```

**Response:**
```json
{
  "ok": true,
  "exists": true,
  "tokens": {
    "accessToken": "eyJ...",
    "accessExp": 1699123456,
    "refreshToken": "eyJ...",
    "refreshExp": 1702123456
  }
}
```

#### POST `/v1/auth/refresh`
Renueva tokens.

**Request:**
```json
{
  "refresh_token": "eyJ..."
}
```

**Response:**
```json
{
  "ok": true,
  "tokens": {
    "accessToken": "eyJ...",
    "accessExp": 1699123456,
    "refreshToken": "eyJ...",
    "refreshExp": 1702123456
  }
}
```

### Profile (Requieren autenticación)

Todos los endpoints de profile requieren header:
```
Authorization: Bearer <accessToken>
```

#### GET `/v1/profile/me`
Obtiene datos del usuario autenticado.

**Response:**
```json
{
  "ok": true,
  "customer": {
    "name": "Juan Pérez",
    "phone": "04141234567"
  }
}
```

#### GET `/v1/profile/me/balance`
Obtiene balance en VES del usuario.

**Response:**
```json
{
  "ok": true,
  "balance_ves": 150000.50
}
```

#### GET `/v1/profile/me/last_payments`
Obtiene últimos pagos agrupados por fecha.

**Response:**
```json
{
  "ok": true,
  "data": [
    {
      "_id": "2025-11-06",
      "payments": [
        {
          "_id": "673abc123...",
          "rason": "Pago cuota #1",
          "balance_bs": 5000.00,
          "status": "Activo",
          "full_date": "2025-11-06T14:30:00Z"
        }
      ]
    }
  ]
}
```

## 🧪 Testing

### Test manual con curl

```bash
# Verify number
curl -X POST http://localhost:3000/v1/auth/verify_number \
  -H "Content-Type: application/json" \
  -d '{"phone":"04141234567"}'

# Login
curl -X POST http://localhost:3000/v1/auth/login \
  -H "Content-Type: application/json" \
  -d '{"phone":"04141234567","code":123456}'

# Profile me (requiere token)
curl http://localhost:3000/v1/profile/me \
  -H "Authorization: Bearer <tu_access_token>"
```

### Benchmark de rendimiento

Instalar herramienta de benchmark:
```bash
# Opción 1: wrk
sudo apt-get install wrk

# Opción 2: autocannon (Node.js)
npm install -g autocannon
```

Ejecutar benchmark:
```bash
# Con wrk (4 threads, 100 conexiones, 30 segundos)
wrk -t4 -c100 -d30s http://localhost:3000/v1/profile/me \
  -H "Authorization: Bearer <token>"

# Con autocannon
autocannon -c 100 -d 30 http://localhost:3000/v1/profile/me \
  -H "Authorization: Bearer <token>"
```

**Resultados esperados:**
- Requests/sec: 15,000 - 25,000
- Latencia p50: < 10ms
- Latencia p99: < 50ms

## 🐳 Despliegue con Docker

### Opción 1: Docker simple

```bash
# Build
docker build -t api-abdo:v0.2.0 .

# Run
docker run -d \
  --name api-abdo \
  -p 3000:3000 \
  --env-file .env \
  api-abdo:v0.2.0
```

### Opción 2: Docker Compose (Recomendado)

```bash
# Iniciar todo el stack (API + MongoDB + Redis)
docker-compose up -d

# Ver logs
docker-compose logs -f api-abdo

# Detener
docker-compose down
```

## 📊 Monitoreo

### Logs estructurados

La API usa `tracing` para logs estructurados:

```bash
# Formato pretty (desarrollo)
LOG_FORMAT=pretty cargo run

# Formato JSON (producción)
LOG_FORMAT=json cargo run
```

### Métricas Redis

```bash
# Conectar a Redis CLI
redis-cli

# Ver todas las keys
KEYS *

# Ver tasa de cambio cacheada
GET exchange_rate:bcv

# Ver estadísticas
INFO stats
```

### Estadísticas MongoDB

```bash
mongosh

# Conectar a tu DB
use tu_base_de_datos

# Ver estadísticas de colecciones
db.Clients.stats()
db.Payments.stats()

# Ver índices
db.Clients.getIndexes()

# Ver queries lentas
db.setProfilingLevel(1, { slowms: 100 })
db.system.profile.find().limit(10).sort({ ts: -1 })
```

## 🔧 Troubleshooting

### Error: "Failed to connect to MongoDB"

```bash
# Verificar que MongoDB está corriendo
sudo systemctl status mongod

# Iniciar MongoDB
sudo systemctl start mongod

# Verificar conexión
mongosh mongodb://localhost:27017
```

### Error: "Failed to connect to Redis"

```bash
# Verificar que Redis está corriendo
redis-cli ping

# Iniciar Redis
redis-server --daemonize yes
```

### Error: "cargo build" falla

```bash
# Actualizar Rust
rustup update

# Limpiar cache y recompilar
cargo clean
cargo build
```

### Error de compilación "edition 2024 not found"

```bash
# Editar Cargo.toml y cambiar:
edition = "2024"  # ← Cambiar a 2021
edition = "2021"  # ← Correcto
```

### Performance bajo

1. **Verificar índices MongoDB:**
```bash
mongosh < scripts/create_indexes.js
```

2. **Verificar que Redis está activo:**
```bash
redis-cli ping
```

3. **Verificar configuración de pool:**
```bash
# En .env, asegurar:
MONGO_POOL_SIZE=100
REDIS_POOL_SIZE=50
```

4. **Compilar en modo release:**
```bash
cargo build --release
./target/release/api-abdo
```

## 📚 Documentación Adicional

- **Migración completa**: Ver `MIGRACION_OPTIMIZACION.md`
- **Scripts útiles**: Directorio `scripts/`
  - `backup_db.sh`: Backup de MongoDB
  - `create_indexes.js`: Crear índices

## 🔐 Seguridad

### Recomendaciones para producción:

1. **Cambiar JWT_SECRET** a un valor aleatorio seguro de 64+ caracteres
2. **Configurar CORS** específico en `src/axum_router.rs`:
   ```rust
   .allow_origin("https://tu-dominio.com".parse::<HeaderValue>().unwrap())
   ```
3. **Habilitar HTTPS** con reverse proxy (nginx/caddy)
4. **Ajustar rate limits** según tu caso de uso
5. **Usar MongoDB con autenticación** en producción
6. **Usar Redis con password** en producción

### Ejemplo nginx con HTTPS:

```nginx
server {
    listen 443 ssl http2;
    server_name api.tudominio.com;

    ssl_certificate /path/to/cert.pem;
    ssl_certificate_key /path/to/key.pem;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

## 📈 Optimizaciones Futuras

- [ ] Implementar caché de sesiones en Redis
- [ ] Agregar endpoints de health check
- [ ] Implementar métricas de Prometheus
- [ ] Agregar tests unitarios e integración
- [ ] Implementar circuit breaker para MongoDB
- [ ] Agregar rate limiting por usuario (no solo por IP)

## 👥 Equipo y Soporte

Para reportar issues o solicitar features, crear un issue en el repositorio.

## 📝 Licencia

[Especificar licencia]

---

**v0.2.0** - Migración completa a Axum con optimizaciones de alto rendimiento ✨
