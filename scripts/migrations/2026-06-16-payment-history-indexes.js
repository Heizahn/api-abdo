// Índices para filtros server-side del historial de pagos.
// Ejecutar con:
//   mongosh <MONGO_URI> scripts/migrations/2026-06-16-payment-history-indexes.js

print("=".repeat(60));
print("📊 Creando índices para historial de pagos");
print("=".repeat(60));

print("📍 Base de datos: " + db.getName());
print("");

print("📦 Colección: Payments");

db.Payments.createIndex(
  { "idCreator": 1, "dCreation": -1 },
  { name: "idx_payments_creator_date", background: true }
);
print("  ✅ Payments.idCreator + dCreation");

db.Payments.createIndex(
  { "idEditor": 1, "dCreation": -1 },
  { name: "idx_payments_editor_date", background: true }
);
print("  ✅ Payments.idEditor + dCreation");

print("");
print("📦 Colección: Users");

db.Users.createIndex(
  { "sName": 1 },
  { name: "idx_users_name", background: true }
);
print("  ✅ Users.sName");

print("");
print("✅ Índices de historial de pagos listos");
