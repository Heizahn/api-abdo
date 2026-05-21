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

// approve_payment_report_handler: fuzzy match por cliente + referencia
db.Payments.createIndex(
  { "idClient": 1, "sReference": 1 },
  { name: "idx_payments_client_reference", background: true }
);
print("  ✅ Payments.idClient + sReference");

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

// list_payment_reports_handler + count_pending_reports: filtran por sState
db.PaymentReports.createIndex(
  { "sState": 1 },
  { name: "idx_payment_reports_state", background: true }
);
print("  ✅ PaymentReports.sState");

// check_reference pass 4: banco-scoped cross-client dedup
// Índice parcial — solo cubre docs con idIssuingBank presente (excluye legacy null docs).
db.PaymentReports.createIndex(
  { "idIssuingBank": 1, "sReference": 1 },
  {
    name: "idx_paymentreports_bank_ref",
    partialFilterExpression: { "idIssuingBank": { "$exists": true } },
    background: true
  }
);
print("  ✅ PaymentReports.idIssuingBank + sReference (partial)");

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
// COLECCIÓN: WaConversationEvents
// ============================================
print("📦 Colección: WaConversationEvents");

// Timeline por conversación: lookup ordenado ASC por (conversation_id, created_at).
db.WaConversationEvents.createIndex(
  { "conversation_id": 1, "created_at": 1 },
  { name: "idx_waconvevents_conv_created", background: true }
);
print("  ✅ WaConversationEvents.conversation_id + created_at");

// Auditoría cross-conversation: filtra por business_phone y rango de fechas.
db.WaConversationEvents.createIndex(
  { "business_phone": 1, "created_at": -1 },
  { name: "idx_waconvevents_biz_created_desc", background: true }
);
print("  ✅ WaConversationEvents.business_phone + created_at desc");

// Métricas por agente: cuántos transfers/closes hizo cada uno.
db.WaConversationEvents.createIndex(
  { "actor_id": 1, "event_type": 1, "created_at": -1 },
  { name: "idx_waconvevents_actor_type_created", background: true, sparse: true }
);
print("  ✅ WaConversationEvents.actor_id + event_type + created_at desc (sparse)");

print("");

// ============================================
// COLECCIÓN: AiAgents — agentes IA (modelo agent-centric)
// ============================================
print("📦 Colección: AiAgents");

// Listado filtrable por workspace: el array `workspace_ids` se indexa multikey
// para acelerar `find({ workspace_ids: <oid> })`.
db.AiAgents.createIndex(
  { "workspace_ids": 1 },
  { name: "idx_ai_agents_workspace_ids", background: true }
);
print("  ✅ AiAgents.workspace_ids (multikey)");

// Listado general ordenado por created_at desc.
db.AiAgents.createIndex(
  { "created_at": -1 },
  { name: "idx_ai_agents_created", background: true }
);
print("  ✅ AiAgents.created_at desc");

print("");

// ============================================
// COLECCIÓN: AiAgentFaqs — knowledge base por agente
// ============================================
print("📦 Colección: AiAgentFaqs");

// Listado por agente ordenado por `created_at` desc.
db.AiAgentFaqs.createIndex(
  { "agent_id": 1, "created_at": -1 },
  { name: "idx_ai_agent_faqs_agent_created", background: true }
);
print("  ✅ AiAgentFaqs.agent_id + created_at desc");

// Phase 3a — metrics aggregate over (agent_id, created_at) range.
// Covers: get_ai_agent_metrics summary + daily pipelines.
db.AiInteractions.createIndex(
  { "agent_id": 1, "created_at": -1 },
  { name: "agent_id_1_created_at_-1", background: true }
);
print("  ✅ AiInteractions.agent_id + created_at desc (Phase 3a metrics)");

print("");

// ============================================
// COLECCIÓN: AiPlans — catálogo de planes que la tool list_plans expone
// ============================================
print("📦 Colección: AiPlans");

db.AiPlans.createIndex(
  { "active": 1, "display_order": 1, "mbps": 1 },
  { name: "idx_ai_plans_active_order", background: true }
);
print("  ✅ AiPlans.active + display_order + mbps");

print("");

// ============================================
// COLECCIÓN: AiCoverageZones — zonas que la tool check_coverage matchea
// ============================================
print("📦 Colección: AiCoverageZones");

// Eliminar índice del esquema legacy (idempotente: no falla si no existe).
try {
    db.AiCoverageZones.dropIndex("idx_ai_coverage_active_name");
    print("  🗑️  AiCoverageZones: eliminado índice legacy idx_ai_coverage_active_name");
} catch (e) {
    // El índice ya no existe — OK.
}

// Índice principal: cubre list_ai_coverage_zones(true) + futuras queries por estado/municipio.
db.AiCoverageZones.createIndex(
    { "is_active": 1, "state": 1, "municipality": 1 },
    { name: "idx_ai_coverage_active_state_muni", background: true }
);
print("  ✅ AiCoverageZones.is_active + state + municipality");

print("");

// ============================================
// COLECCIÓN: WaMessages — auditoría cross-conversation
// ============================================
print("📦 Colección: WaMessages (auditoría)");

// Dedupe/idempotencia fuerte del webhook y de envíos persistidos. El backend
// hace upsert por `wa_message_id`, así que este índice evita scans y protege
// contra duplicados si llegan webhooks/retries concurrentes.
db.WaMessages.createIndex(
  { "wa_message_id": 1 },
  { name: "idx_wamsgs_wa_id", unique: true, background: true }
);
print("  ✅ WaMessages.wa_message_id (unique)");

// Timeline principal del chat: get_messages filtra por conversation_id y
// ordena por timestamp desc + _id desc (cursor pagination del front).
db.WaMessages.createIndex(
  { "conversation_id": 1, "timestamp": -1, "_id": -1 },
  { name: "idx_wamsgs_conv_timestamp_oid_desc", background: true }
);
print("  ✅ WaMessages.conversation_id + timestamp desc + _id desc");

// History del dispatch IA: list_recent_messages_for_conversation usa
// conversation_id y ordena por _id desc (orden real de inserción).
db.WaMessages.createIndex(
  { "conversation_id": 1, "_id": -1 },
  { name: "idx_wamsgs_conv_oid_desc", background: true }
);
print("  ✅ WaMessages.conversation_id + _id desc");

// Idempotencia de envíos manuales/retries del panel. Sparse porque la mayoría
// de mensajes no usan idempotency_key.
db.WaMessages.createIndex(
  { "conversation_id": 1, "idempotency_key": 1 },
  {
    name: "idx_wamsgs_conv_idempotency",
    unique: true,
    sparse: true,
    background: true
  }
);
print("  ✅ WaMessages.conversation_id + idempotency_key (unique, sparse)");

// Filtros típicos de auditoría: rango de fechas + agente.
db.WaMessages.createIndex(
  { "timestamp": -1, "sent_by": 1 },
  { name: "idx_wamsgs_timestamp_sentby", background: true }
);
print("  ✅ WaMessages.timestamp desc + sent_by");

// Filtro por dirección + tipo (ej. "todos los inbound de tipo image").
db.WaMessages.createIndex(
  { "direction": 1, "msg_type": 1, "timestamp": -1 },
  { name: "idx_wamsgs_dir_type_timestamp", background: true }
);
print("  ✅ WaMessages.direction + msg_type + timestamp desc");

print("");

// ============================================
// COLECCIÓN: WaConversations — badge de no leídas
// ============================================
print("📦 Colección: WaConversations");

// Identidad natural de la conversación: un chat por (cliente, número negocio).
// El webhook hace upsert con estos dos campos; unique evita duplicados por
// carreras o reintentos.
db.WaConversations.createIndex(
  { "phone": 1, "business_phone": 1 },
  { name: "idx_waconv_phone_business", unique: true, background: true }
);
print("  ✅ WaConversations.phone + business_phone (unique)");

// Bandeja principal del front: get_conversations filtra típicamente por
// status / assigned_to / business_phone y ordena por last_message_at desc.
db.WaConversations.createIndex(
  { "status": 1, "assigned_to": 1, "business_phone": 1, "last_message_at": -1, "_id": -1 },
  { name: "idx_waconv_listing", background: true }
);
print("  ✅ WaConversations.status + assigned_to + business_phone + last_message_at desc + _id desc");

// count_unread_conversations: índice parcial — solo cubre docs con unread_count > 0.
// Requiere MongoDB ≥ 3.2 (confirmado: cluster en 8.0.8).
db.WaConversations.createIndex(
  { "unread_count": 1 },
  {
    name: "idx_wa_conversations_unread_partial",
    partialFilterExpression: { "unread_count": { "$gt": 0 } },
    background: true
  }
);
print("  ✅ WaConversations.unread_count (partial: unread_count > 0)");

print("");

// ============================================
// COLECCIÓN: WaTickets — badge de tickets abiertos
// ============================================
print("📦 Colección: WaTickets");

// count_open_tickets + list con filtro por status y orden cronológico.
// WaTickets no tenía ningún índice antes de esta migración.
db.WaTickets.createIndex(
  { "status": 1, "created_at": -1 },
  { name: "idx_wa_tickets_status_created_desc", background: true }
);
print("  ✅ WaTickets.status + created_at desc");

// broadcast_ticket_updated + list: filtra por agente asignado.
db.WaTickets.createIndex(
  { "assigned_to_id": 1 },
  { name: "idx_wa_tickets_assignee", background: true }
);
print("  ✅ WaTickets.assigned_to_id");

// Auditoría: filtra por creador.
db.WaTickets.createIndex(
  { "created_by_id": 1 },
  { name: "idx_wa_tickets_created_by", background: true }
);
print("  ✅ WaTickets.created_by_id");

// Listado por workspace: filtra por business_phone + status.
db.WaTickets.createIndex(
  { "business_phone": 1, "status": 1 },
  { name: "idx_wa_tickets_phone_status", background: true }
);
print("  ✅ WaTickets.business_phone + status");

print("");

// ============================================
// VERIFICAR ÍNDICES CREADOS
// ============================================
print("=".repeat(60));
print("📋 VERIFICACIÓN DE ÍNDICES");
print("=".repeat(60));
print("");

const toVerify = ["Clients", "Payments", "Debts", "PartPayments", "PaymentReports", "Users", "verification_codes", "WaTemplates", "wa_template_media.files", "WaConversations", "WaConversationEvents", "WaMessages", "WaTickets", "AiAgents", "AiAgentFaqs", "AiInteractions", "AiPlans", "AiCoverageZones"];
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
