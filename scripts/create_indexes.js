// Script para crear índices optimizados en MongoDB
// Ejecutar con: mongosh < scripts/create_indexes.js

// Nota: Reemplazar 'nombre_base_datos' con el nombre real de tu base de datos
// O ejecutar con: mongosh mongodb://localhost:27017/nombre_base_datos < scripts/create_indexes.js

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
print("  ✅ Índice creado: Clients.sPhone");

db.Clients.createIndex(
  { "_id": 1, "sPhone": 1 },
  { name: "idx_clients_id_phone", background: true }
);
print("  ✅ Índice compuesto: Clients._id + sPhone");

db.Clients.createIndex(
  { "sName": 1 },
  { name: "idx_clients_name", background: true }
);
print("  ✅ Índice creado: Clients.sName");

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

db.Payments.createIndex(
  { "idClient": 1, "dCreation": -1 },
  { name: "idx_payments_client_date", background: true }
);
print("  ✅ Índice compuesto: Payments.idClient + dCreation");

db.Payments.createIndex(
  { "sState": 1 },
  { name: "idx_payments_state", background: true }
);
print("  ✅ Índice creado: Payments.sState");

db.Payments.createIndex(
  { "dCreation": -1 },
  { name: "idx_payments_date", background: true }
);
print("  ✅ Índice creado: Payments.dCreation");

db.Payments.createIndex(
  { "idClient": 1, "sState": 1 },
  { name: "idx_payments_client_state", background: true }
);
print("  ✅ Índice compuesto: Payments.idClient + sState");

print("");

// ============================================
// COLECCIÓN: Debts
// ============================================
print("📦 Colección: Debts");

db.Debts.createIndex(
  { "idClient": 1 },
  { name: "idx_debts_client", background: true }
);
print("  ✅ Índice creado: Debts.idClient");

db.Debts.createIndex(
  { "nAmount": 1 },
  { name: "idx_debts_amount", background: true }
);
print("  ✅ Índice creado: Debts.nAmount");

print("");

// ============================================
// COLECCIÓN: PartPayment
// ============================================
print("📦 Colección: PartPayment");

db.PartPayment.createIndex(
  { "idDebt": 1 },
  { name: "idx_partpayment_debt", background: true }
);
print("  ✅ Índice creado: PartPayment.idDebt");

db.PartPayment.createIndex(
  { "idPayment": 1 },
  { name: "idx_partpayment_payment", background: true }
);
print("  ✅ Índice creado: PartPayment.idPayment");

db.PartPayment.createIndex(
  { "idDebt": 1, "idPayment": 1 },
  { name: "idx_partpayment_debt_payment", background: true }
);
print("  ✅ Índice compuesto: PartPayment.idDebt + idPayment");

print("");

// ============================================
// VERIFICAR ÍNDICES CREADOS
// ============================================
print("=".repeat(60));
print("📋 VERIFICACIÓN DE ÍNDICES");
print("=".repeat(60));
print("");

print("Clients:");
db.Clients.getIndexes().forEach(idx => {
  print("  - " + idx.name + " → " + JSON.stringify(idx.key));
});
print("");

print("verification_codes:");
db.verification_codes.getIndexes().forEach(idx => {
  print("  - " + idx.name + " → " + JSON.stringify(idx.key));
});
print("");

print("Payments:");
db.Payments.getIndexes().forEach(idx => {
  print("  - " + idx.name + " → " + JSON.stringify(idx.key));
});
print("");

print("Debts:");
db.Debts.getIndexes().forEach(idx => {
  print("  - " + idx.name + " → " + JSON.stringify(idx.key));
});
print("");

print("PartPayment:");
db.PartPayment.getIndexes().forEach(idx => {
  print("  - " + idx.name + " → " + JSON.stringify(idx.key));
});
print("");

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
print("📊 Impacto esperado:");
print("  • Queries 10-50x más rápidas");
print("  • Agregaciones optimizadas");
print("  • Auto-limpieza de códigos de verificación expirados");
print("");
print("💡 Próximo paso:");
print("  • Ejecutar: cargo build --release");
print("  • Iniciar la nueva API con Axum");
print("");
