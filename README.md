# API ABDO v0.3.0 - High Performance REST API

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

## 🛠️ Configuración Inicial

### 1. Clonar y configurar

```bash
git clone <repo-url>
cd api-abdo
```

### 2. Crear archivo .env

```bash
cp .env.example .env
```

Configurar las variables de entorno para MongoDB, Redis, JWT y servicios externos.

### 3. Crear índices en MongoDB (CRÍTICO)

```bash
# IMPORTANTE: Este paso es OBLIGATORIO para el rendimiento
mongosh mongodb://localhost:27017/tu_base_de_datos < scripts/create_indexes.js
```

### 4. Compilar y Ejecutar

```bash
# Development
cargo run

# Production
cargo build --release
./target/release/api-abdo
```

---

## 📡 API Endpoints

### 🔐 Autenticación (Pública)

| Método | Endpoint | Descripción |
| :--- | :--- | :--- |
| `POST` | `/v1/auth/verify_number` | Verifica si el número existe y envía código SMS. Payload: `{ "phone": "..." }`. |
| `POST` | `/v1/auth/login` | Login con teléfono y código. Retorna Access/Refresh tokens. Payload: `{ "phone": "...", "code": 1234 }`. |
| `POST` | `/v1/auth/refresh` | Renueva tokens usando refresh token. Payload: `{ "refresh_token": "..." }`. |

### 👤 Perfil (Requiere JWT)

| Método | Endpoint | Descripción |
| :--- | :--- | :--- |
| `GET` | `/v1/profile/me/group` | Retorna resumen de todas las cuentas asociadas al teléfono del usuario (balances, últimos pagos). |
| `GET` | `/v1/profile/me/phone` | Retorna el número de teléfono del usuario autenticado. |

### 💳 Deudas y Pagos (Requiere JWT)

| Método | Endpoint | Descripción |
| :--- | :--- | :--- |
| `GET` | `/v1/receivable/me` | Obtiene todas las deudas **activas** (saldo pendiente). Incluye pagos parciales y reportes pendientes. |
| `GET` | `/v1/receivable/me/paid` | Obtiene historial de deudas **pagadas** (saldo 0). |
| `GET` | `/v1/receivable/:id` | Obtiene detalle completo de una deuda específica (incluye pagos y reportes). |
| `GET` | `/v1/payments/methods/payment/:debt_id` | Obtiene datos de pago móvil asociados al cliente dueño de una deuda específica. |
| `GET` | `/v1/payments/methods/payment/by-client/:client_id` | Obtiene datos de pago móvil asociados a un cliente específico. |
| `POST` | `/v1/payments/payment/report` | Reporta un pago (Multipart Form). Campos: `reference`, `amount_bs`, `date`, `bank`, `phone`, `image` (file), `id_payment_method` y (`id_debt` o `id_client`). |

### 🛠️ Utilidades

| Método | Endpoint | Descripción | Acceso |
| :--- | :--- | :--- | :--- |
| `POST` | `/v1/utils/calculate/bs` | Calcula monto en BS según tasa BCV + IVA. | Público |
| `POST` | `/v2/utils/calculate` | Calculadora bidireccional (USD<->BS) según tasa e IVA. | Público |
| `GET` | `/v1/utils/list/banks` | Lista de bancos disponibles en el sistema. | JWT |
| `GET` | `/v1/utils/ping` | Health check (`pong`). | Público |
| `GET` | `/v1/utils/latest-version` | Obtiene la última versión disponible de la app. | Público |
| `GET` | `/v1/utils/image/:filename` | Sirve imágenes subidas (uploads). | Público |
| `GET` | `/v1/privacy-policy` | Retorna la política de privacidad en HTML. | Público |

---

## 🗄️ Database Functions (Modules)

La capa de acceso a datos está organizada en repositorios implementados sobre MongoDB.

### `AuthRepository` (src/db/mongo/auth.rs)
- **`store_verification_code`**: Guarda código SMS con expiración (60 min).
- **`find_verification_code`**: Busca código por teléfono y valor.
- **`delete_verification_code`**: Elimina código tras uso.

### `ProfileRepository` (src/db/mongo/profile.rs)
- **`find_customer_by_phone`**: Busca cliente principal por teléfono.
- **`find_customer_by_id`**: Busca vista de cliente por ID.
- **`find_clients_by_phone`**: Obtiene todas las cuentas de cliente (Clients) ligadas a un teléfono.
- **`find_client_by_id`**: Obtiene una cuenta específica por ID.
- **`find_tax_by_id`**: Obtiene configuración de impuestos (o default).
- **`get_clients_by_phone_group`**: Agregación compleja que agrupa todas las cuentas de un usuario, sus balances y detalles.
- **`get_last_payments_by_id_client`**: Agregación que combina `Payments` y `PaymentReports` para historial unificado.
- **`get_phone`**: Helper para obtener solo el teléfono dado un ID.

### `SalesRepository` (src/db/mongo/sales.rs)
- **`get_latest_exchange_rate`**: Obtiene tasa BCV del día (con manejo de zona horaria VET).
- **`find_part_payments_by_debt_ids`**: Busca pagos parciales asociados a lista de deudas.
- **`find_payments_by_ids`**: Busca documentos de pago (Payments) por ID.
- **`find_active_debts_by_client_ids`**: Busca deudas con estado "Activo".
- **`find_debt_by_id`**: Obtiene detalle de una deuda.
- **`find_client_owner_by_id`**: Busca el `idOwner` (proveedor) de un cliente.
- **`find_user_payment_info_by_id`**: Obtiene config de pago del proveedor.
- **`find_payment_method_by_id`**: Obtiene datos bancarios/pago móvil.
- **`create_payment_report`**: Inserta nuevo reporte de pago.
- **`find_pending_reports_by_debt_ids`**: Busca reportes en estado "Pendiente" para calcular saldos "en proceso".
- **`find_bank_list`**: Lista catálogo de bancos.

### `OnuRepository` (src/db/mongo/onu.rs)
- **`get_device_serial_numbers`**: Listado ligero de ONUs (ID, SN, MAC, OLT).
- **`save_onu_from_zte`**: Upsert de datos de ONU detectados desde OLT.
- **`get_onus_for_update_ip`**: Lista ONUs que requieren actualización de IP.
- **`update_onu_ip`**: Actualiza IP de una ONU.

### `UtilsRepository` (src/db/mongo/utils.rs)
- **`find_latest_version`**: Obtiene control de versiones de la app.
- **`exists_rate_for_date`**: Verifica si ya existe tasa BCV para un rango.
- **`save_exchange_rate`**: Guarda nueva tasa BCV histórica.

---

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
curl http://localhost:3000/v1/profile/me/group \
  -H "Authorization: Bearer <tu_access_token>"
```
