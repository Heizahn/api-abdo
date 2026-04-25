// Script para crear índices optimizados en MongoDB
// Ejecutar con: mongosh mongodb://localhost:27017/NOMBRE_BD scripts/create_indexes.js
//
// Si requiere autenticación:
//   mongosh "mongodb://user:pass@host:27017/NOMBRE_BD" scripts/create_indexes.js
//
// createIndex ignora silenciosamente si el índice ya existe.

print("=".repeat(60));
print("📊 CREANDO ÍNDICES PARA OPTIMIZACIÓN DE API-ABDO");
print("=".repeat(60));
print("");

// Obtener el nombre de la base de datos actual
const dbName = db.getName();
print("📍 Base de datos: " + dbName);
print("");

// ============================================
// COLECCIÓN: Clients
// ============================================
print("📦 Colección: Clients");

db.Clients.createIndex(
  { "sPhone": 1 },
  { name: "idx_clients_phone", background: true }
);
print("  ✅ Clients.sPhone");

db.Clients.createIndex(
  { "sName": 1 },
  { name: "idx_clients_name", background: true }
);
print("  ✅ Clients.sName");

// Dashboard: filtro por owner (get_solvency_counts, find_active_clients_for_closing, get_latest_payments)
db.Clients.createIndex(
  { "idOwner": 1 },
  { name: "idx_clients_owner", background: true }
);
print("  ✅ Clients.idOwner");

// Dashboard: solvency counts y monthly-closing filtran por sState
db.Clients.createIndex(
  { "sState": 1 },
  { name: "idx_clients_state", background: true }
);
print("  ✅ Clients.sState");

// Dashboard: filtro combinado más común (solvency + monthly-closing con owner)
db.Clients.createIndex(
  { "sState": 1, "idOwner": 1 },
  { name: "idx_clients_state_owner", background: true }
);
print("  ✅ Clients.sState + idOwner");

print("");

// ============================================
// COLECCIÓN: verification_codes
// ============================================
print("📦 Colección: verification_codes");

db.verification_codes.createIndex(
  { "phone": 1, "code": 1 },
  { name: "idx_verification_phone_code", background: true }
);
print("  ✅ Índice compuesto: verification_codes.phone + code");

// TTL Index: Borrado automático de códigos expirados
db.verification_codes.createIndex(
  { "expires_at": 1 },
  {
    name: "idx_verification_ttl",
    expireAfterSeconds: 0,
    background: true
  }
);
print("  ✅ Índice TTL: verification_codes.expires_at (auto-delete)");

db.verification_codes.createIndex(
  { "created_at": -1 },
  { name: "idx_verification_created", background: true }
);
print("  ✅ Índice creado: verification_codes.created_at");

print("");

// ============================================
// COLECCIÓN: Payments
// ============================================
print("📦 Colección: Payments");

// Dashboard: get_latest_payments (sort global por fecha)
db.Payments.createIndex(
  { "dCreation": -1 },
  { name: "idx_payments_date", background: true }
);
print("  ✅ Payments.dCreation");

// Dashboard: get_latest_payments con owner ($in de client_ids + sort)
db.Payments.createIndex(
  { "idClient": 1, "dCreation": -1 },
  { name: "idx_payments_client_date", background: true }
);
print("  ✅ Payments.idClient + dCreation");

// Dashboard: sum_active_payments_in_range (idClient + sState + rango de fecha)
db.Payments.createIndex(
  { "idClient": 1, "sState": 1, "dCreation": -1 },
  { name: "idx_payments_client_state_date", background: true }
);
print("  ✅ Payments.idClient + sState + dCreation");

db.Payments.createIndex(
  { "sState": 1 },
  { name: "idx_payments_state", background: true }
);
print("  ✅ Payments.sState");

print("");

// ============================================
// COLECCIÓN: Debts
// ============================================
print("📦 Colección: Debts");

// find_active_debts_by_client_ids: filtra por idClient + sState
db.Debts.createIndex(
  { "idClient": 1, "sState": 1 },
  { name: "idx_debts_client_state", background: true }
);
print("  ✅ Debts.idClient + sState");

db.Debts.createIndex(
  { "idClient": 1 },
  { name: "idx_debts_client", background: true }
);
print("  ✅ Debts.idClient");

print("");

// ============================================
// COLECCIÓN: PartPayments
// ============================================
print("📦 Colección: PartPayments");

db.PartPayments.createIndex(
  { "idDebt": 1 },
  { name: "idx_partpayments_debt", background: true }
);
print("  ✅ PartPayments.idDebt");

db.PartPayments.createIndex(
  { "idPayment": 1 },
  { name: "idx_partpayments_payment", background: true }
);
print("  ✅ PartPayments.idPayment");

print("");

// ============================================
// COLECCIÓN: PaymentReports
// ============================================
print("📦 Colección: PaymentReports");

// find_pending_reports_by_debt_ids: filtra por idDebt + sState
db.PaymentReports.createIndex(
  { "idDebt": 1, "sState": 1 },
  { name: "idx_paymentreports_debt_state", background: true }
);
print("  ✅ PaymentReports.idDebt + sState");

// get_last_payments_by_id_client: lookup por idClient
db.PaymentReports.createIndex(
  { "idClient": 1 },
  { name: "idx_paymentreports_client", background: true }
);
print("  ✅ PaymentReports.idClient");

print("");

// ============================================
// COLECCIÓN: Users
// ============================================
print("📦 Colección: Users");

// find_providers: filtra por nRole
db.Users.createIndex(
  { "nRole": 1 },
  { name: "idx_users_role", background: true }
);
print("  ✅ Users.nRole");

print("");

// ============================================
// COLECCIÓN: wa_template_media.files (GridFS)
// ============================================
print("📦 Colección: wa_template_media.files (GridFS)");

// Dedup por (phone_number_id, sha256): si el mismo binario se sube 2 veces,
// reusamos el media_id. El bucket de GridFS es `wa_template_media` ⇒ el doc
// de metadatos vive en `wa_template_media.files`.
db.wa_template_media.files.createIndex(
  { "metadata.phone_number_id": 1, "metadata.sha256": 1 },
  { name: "idx_wa_template_media_phone_sha", unique: true, background: true }
);
print("  ✅ wa_template_media.files.metadata.phone_number_id + sha256 (unique)");

print("");

// ============================================
// COLECCIÓN: WaTemplates
// ============================================
print("📦 Colección: WaTemplates");

// Unicidad por (phone_number_id, name, language) — gate del 409 name_already_exists
db.WaTemplates.createIndex(
  { "phone_number_id": 1, "name": 1, "language": 1 },
  { name: "idx_watemplates_phone_name_lang", unique: true, background: true }
);
print("  ✅ WaTemplates.phone_number_id + name + language (unique)");

// Listado por número con filtro por status (caso típico de UI)
db.WaTemplates.createIndex(
  { "phone_number_id": 1, "status": 1 },
  { name: "idx_watemplates_phone_status", background: true }
);
print("  ✅ WaTemplates.phone_number_id + status");

// Filtro `only_system` del listado
db.WaTemplates.createIndex(
  { "phone_number_id": 1, "is_system": 1 },
  { name: "idx_watemplates_phone_is_system", background: true }
);
print("  ✅ WaTemplates.phone_number_id + is_system");

// Lookup desde el webhook `message_template_status_update` (DRAFT no tiene id, sparse)
db.WaTemplates.createIndex(
  { "meta_template_id": 1 },
  { name: "idx_watemplates_meta_id", unique: true, sparse: true, background: true }
);
print("  ✅ WaTemplates.meta_template_id (unique, sparse)");

// Orden por fecha (paginación cursor descendente)
db.WaTemplates.createIndex(
  { "phone_number_id": 1, "created_at": -1 },
  { name: "idx_watemplates_phone_created_desc", background: true }
);
print("  ✅ WaTemplates.phone_number_id + created_at desc");

print("");

// ============================================
// VERIFICAR ÍNDICES CREADOS
// ============================================
print("=".repeat(60));
print("📋 VERIFICACIÓN DE ÍNDICES");
print("=".repeat(60));
print("");

const toVerify = ["Clients", "Payments", "Debts", "PartPayments", "PaymentReports", "Users", "verification_codes", "WaTemplates", "wa_template_media.files"];
toVerify.forEach(col => {
  print(col + ":");
  db.getCollection(col).getIndexes().forEach(idx => {
    print("  - " + idx.name + " → " + JSON.stringify(idx.key));
  });
  print("");
});

// ============================================
// BASE DE DATOS: BCV (Tasas de Cambio)
// ============================================
print("=".repeat(60));
print("💱 CREANDO ÍNDICES EN BASE DE DATOS BCV");
print("=".repeat(60));
print("");

// Cambiar a base de datos BCV
db = db.getSiblingDB('BCV');
print("📍 Base de datos: BCV");
print("");

print("📦 Colección: BCVRates");

db.BCVRates.createIndex(
  { "timestamp": -1 },
  { name: "idx_bcvrates_timestamp", background: true }
);
print("  ✅ Índice creado: BCVRates.timestamp");

db.BCVRates.createIndex(
  { "value": 1 },
  { name: "idx_bcvrates_value", background: true }
);
print("  ✅ Índice creado: BCVRates.value");

print("");
print("BCVRates:");
db.BCVRates.getIndexes().forEach(idx => {
  print("  - " + idx.name + " → " + JSON.stringify(idx.key));
});
print("");

// ============================================
// RESUMEN FINAL
// ============================================
print("=".repeat(60));
print("✨ TODOS LOS ÍNDICES CREADOS EXITOSAMENTE");
print("=".repeat(60));
print("");
print("Índices existentes son ignorados automáticamente por MongoDB.");
print("");
