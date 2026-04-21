// Script de migración WhatsApp: limpia WaConversations / WaMessages
// y crea el índice único compuesto (phone, business_phone).
//
// Ejecutar con:
//   mongosh "mongodb://user:pass@host:27017/NOMBRE_BD" scripts/wa_migrate_reset.js

print("=".repeat(60));
print("🧹 RESET WHATSAPP + ÍNDICES");
print("=".repeat(60));

const dbName = db.getName();
print("📍 Base de datos: " + dbName);

// Migrar colecciones viejas (snake_case) si existen
const oldCols = db.getCollectionNames();
if (oldCols.includes("wa_conversations")) {
  db.wa_conversations.renameCollection("WaConversations", /*dropTarget*/ true);
  print("  ♻  wa_conversations → WaConversations");
}
if (oldCols.includes("wa_messages")) {
  db.wa_messages.renameCollection("WaMessages", /*dropTarget*/ true);
  print("  ♻  wa_messages → WaMessages");
}
if (oldCols.includes("wa_settings")) {
  db.wa_settings.renameCollection("WaSettings", /*dropTarget*/ true);
  print("  ♻  wa_settings → WaSettings");
}

// Borrar conversaciones y mensajes (los datos previos no tenían business_phone).
const convCount = db.WaConversations.countDocuments({});
const msgCount = db.WaMessages.countDocuments({});
db.WaConversations.deleteMany({});
db.WaMessages.deleteMany({});
print("  🗑  WaConversations: " + convCount + " docs borrados");
print("  🗑  WaMessages:      " + msgCount + " docs borrados");

// Índice único compuesto: un chat por (contacto, número de negocio).
try { db.WaConversations.dropIndex("phone_1"); } catch (e) {}
try { db.WaConversations.dropIndex("idx_wa_conversations_phone"); } catch (e) {}

db.WaConversations.createIndex(
  { "phone": 1, "business_phone": 1 },
  { name: "uniq_wa_conv_phone_business", unique: true, background: true }
);
print("  ✅ WaConversations: índice único (phone, business_phone)");

// Índice de listados: sort por last_message_at con filtros comunes.
db.WaConversations.createIndex(
  { "business_phone": 1, "last_message_at": -1 },
  { name: "idx_wa_conv_business_last", background: true }
);
db.WaConversations.createIndex(
  { "assigned_to": 1, "last_message_at": -1 },
  { name: "idx_wa_conv_assigned_last", background: true }
);
db.WaConversations.createIndex(
  { "status": 1, "last_message_at": -1 },
  { name: "idx_wa_conv_status_last", background: true }
);

// Mensajes: por conversación + timestamp descendente.
db.WaMessages.createIndex(
  { "conversation_id": 1, "timestamp": -1 },
  { name: "idx_wa_msg_conv_ts", background: true }
);
// Deduplicación de wa_message_id (viene de Meta).
db.WaMessages.createIndex(
  { "wa_message_id": 1 },
  { name: "uniq_wa_msg_waid", unique: true, background: true }
);

print("  ✅ WaMessages: índices (conversation_id,timestamp) + unique(wa_message_id)");

// WaConversationOpens: timestamp del último open de cada agente por conversación.
// Un doc por par (user_id, conversation_id).
db.WaConversationOpens.createIndex(
  { "user_id": 1, "conversation_id": 1 },
  { name: "uniq_wa_conv_open_user_conv", unique: true, background: true }
);
// Lookup batch por agente (filtra por user_id, $in conversation_id).
db.WaConversationOpens.createIndex(
  { "user_id": 1 },
  { name: "idx_wa_conv_open_user", background: true }
);
print("  ✅ WaConversationOpens: unique(user_id, conversation_id)");

print("");
print("📋 Índices actuales en WaConversations:");
db.WaConversations.getIndexes().forEach(i => print("  - " + i.name + " → " + JSON.stringify(i.key)));
print("");
print("📋 Índices actuales en WaMessages:");
db.WaMessages.getIndexes().forEach(i => print("  - " + i.name + " → " + JSON.stringify(i.key)));
print("");
print("📋 Índices actuales en WaConversationOpens:");
db.WaConversationOpens.getIndexes().forEach(i => print("  - " + i.name + " → " + JSON.stringify(i.key)));
print("");
print("✨ Reset + índices OK");
