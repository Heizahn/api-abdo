pub mod mongo;
use crate::models::ai_agent::{
    AiAgent, AiAgentFaq, AiClientLookup, AiCoverageZone, AiInteraction, AiPlan,
};
use crate::models::db::{ActiveClientBalance, ClientDetail, ClientListItem, ClientStatusHistoryItem, CustomerInfoItem, LatestPayment, LatestVersion, OnuForUpdateIp, OnuIdentity, OnuIpUpdate, SolvencyCounts, Tax};
use crate::models::whatsapp::{
    ConversationStats, QuickReplyButton, QuickReplyCtaUrl, QuickReplyHeader, QuickReplyList,
    UrlPreview, WaConversation, WaConversationEvent, WaConversationEventInput, WaMessage,
    WaQuickReply, WaSettings, WaTemplate, WaTemplateCategory, WaTemplateStatus, WaTicket,
    WaTicketTimelineEntry,
};
use std::collections::HashMap;

use crate::models::payment::{Bank, PaymentReport, ReferenceMatchInfo};
use crate::models::users::{User, UserCredentials}; // Import
use crate::modules::network::zte::parser::OnuDetected;
use crate::{
    auth::claims::VerificationCode,
    db::mongo::ResultGroupedByDate,
    domain::customer::{Customer, CustomerView},
    models::db::{Client, Debt, PartPayment, Payment},
    models::payment::{ClientOwner, PaymentMethod, UserPaymentInfo},
};
use mongodb::bson::oid::ObjectId;
use mongodb::bson::Document;
use mongodb::error::Error as MongoError;
use mongodb::results::InsertOneResult;
use crate::error::ApiError;

// ============================================
// 1. AuthRepository: Login, Verificación
// ============================================
#[async_trait::async_trait]
pub trait AuthRepository {
    async fn store_verification_code(&self, phone: &str, code: &u32) -> mongodb::error::Result<()>;
    async fn find_verification_code(&self, phone: &str, code: &u32) -> Option<VerificationCode>;
    async fn delete_verification_code(&self, id: &ObjectId) -> Result<u64, MongoError>;
}

// ============================================
// 6. UserRepository: Auth Users (Admin/Staff)
// ============================================

/// Filtros para `list_users` — todos opcionales excepto `limit`.
pub struct UserListFilter<'a> {
    /// Substring case-insensitive en `sName` O `email`.
    pub search: Option<&'a str>,
    /// Filtro exacto por `nRole`.
    pub role: Option<f32>,
    /// Filtro por `visible`. `None` trae ambos.
    pub visible: Option<bool>,
    /// Filtro por `bCanChat`. `None` trae ambos.
    pub can_chat: Option<bool>,
    /// Resultados por página.
    pub limit: i64,
    /// Cursor opaco (copiar de `next_cursor` de la página anterior).
    pub cursor: Option<&'a str>,
}

#[async_trait::async_trait]
#[allow(dead_code)]
pub trait UserRepository {
    async fn find_user_by_email(&self, email: &str) -> Result<Option<User>, String>;
    async fn find_user_credentials_by_user_id(
        &self,
        user_id: &str,
    ) -> Result<Option<UserCredentials>, String>;
    async fn find_user_by_id(&self, id: &str) -> Result<Option<User>, String>;
    async fn create_user(&self, user: User) -> Result<(), String>;
    async fn create_user_credentials(&self, creds: UserCredentials) -> Result<(), String>;
    async fn find_providers(&self) -> Result<Vec<User>, String>;
    async fn find_agents(&self) -> Result<Vec<User>, String>;
    /// Usuarios con permiso para atender chats (campo `bCanChat == true` y `visible == true`).
    /// Usado para poblar el dropdown de transferencia de conversaciones.
    async fn find_chat_agents(&self) -> Result<Vec<User>, String>;
    /// UUIDs de todos los SUPERADMIN visibles (`nRole == 0` y `visible == true`).
    /// Usado por el módulo de tickets para determinar el scope de broadcast
    /// del evento `TICKET_ACTUALIZADO` (creador + asignado + supervisores).
    /// Devuelve sólo strings — el caller no necesita el doc User completo.
    async fn find_superadmin_ids(&self) -> Result<Vec<String>, String>;
    /// Listado paginado con filtros para el CRUD de usuarios.
    /// Ordenado por `sName` ascendente con `_id` como tiebreaker estable.
    async fn list_users(&self, filter: UserListFilter<'_>) -> Result<Vec<User>, String>;
    /// Setea `visible` en el doc (soft delete / reactivación). Retorna `true`
    /// si el user existía, `false` si no — para devolver 404 al caller.
    async fn set_user_visible(&self, id: &str, visible: bool) -> Result<bool, String>;
    /// Update parcial del user. Sólo se tocan los campos `Some` del patch.
    /// Retorna `true` si el doc existía.
    async fn update_user(&self, id: &str, patch: UpdateUserPatch) -> Result<bool, String>;
    /// Actualiza el hash bcrypt en `UserCredentials` para el `user_id`. Si no
    /// existe una credencial previa, se inserta — soporta el caso borde donde
    /// el user fue creado sin credencial (no debería pasar por `create_user_handler`
    /// pero es defensivo). Retorna `true` si el user existe.
    async fn update_user_password(&self, user_id: &str, password_hash: &str) -> Result<bool, String>;
}

/// Patch parcial para `update_user` — sólo se setean los `Some`.
pub struct UpdateUserPatch {
    pub name: Option<String>,
    pub email: Option<String>,
    pub role: Option<f32>,
    pub can_chat: Option<bool>,
    pub tag: Option<u32>,
}

// ============================================
// 2. ProfileRepository: Clientes
// ============================================
#[async_trait::async_trait]
pub trait ProfileRepository {
    async fn find_customer_by_phone(&self, phone: &str) -> Option<Customer>;
    async fn find_customer_by_id(&self, id: &str) -> Option<CustomerView>;

    async fn find_clients_by_phone(&self, s_phone: &str) -> Result<Vec<Client>, String>;
    async fn find_client_by_id(&self, id: &str) -> Result<Client, String>;
    /// Batch-lookup de `sName` por `_id` para un conjunto de clientes. Devuelve
    /// solo los que existen y tienen nombre no vacío. Usado para resolver el
    /// nombre de contacto en listados de WhatsApp sin caer en N+1.
    async fn get_client_names_by_ids(&self, ids: &[ObjectId]) -> Result<HashMap<ObjectId, String>, String>;
    /// Batch-lookup de `sName` por `sPhone`. Si más de un cliente comparte
    /// teléfono, devuelve el primero que encuentre Mongo. Usado para resolver
    /// el nombre cuando la conversación todavía no tiene `client_id` linkeado.
    async fn get_client_names_by_phones(&self, phones: &[String]) -> Result<HashMap<String, String>, String>;
    async fn find_tax_by_id(&self, id: Option<ObjectId>) -> Result<Option<Tax>, String>;

    async fn get_clients_by_phone_group(&self, phone: String) -> Result<Vec<Document>, MongoError>;
    async fn get_last_payments_by_id_client(
        &self,
        id: String,
    ) -> Result<Vec<ResultGroupedByDate>, MongoError>;

    async fn get_phone(&self, id: &str) -> Result<String, String>;

    async fn find_active_clients_for_closing(&self, owner_id: Option<&str>) -> Result<Vec<ActiveClientBalance>, String>;

    async fn get_solvency_counts(&self, owner_id: Option<&str>) -> Result<SolvencyCounts, String>;

    async fn get_all_clients(&self, owner_id: Option<&str>) -> Result<Vec<ClientListItem>, String>;

    async fn get_client_by_id(
        &self,
        id: &str,
        owner_id: Option<&str>,
    ) -> Result<Option<ClientDetail>, String>;

    async fn get_client_status_history(
        &self,
        client_id: &str,
    ) -> Result<Vec<ClientStatusHistoryItem>, String>;

    async fn get_customers_info(
        &self,
        owner_id: Option<&str>,
    ) -> Result<Vec<CustomerInfoItem>, String>;

    /// Lookup compacto para el tool `lookup_customer` del AI Agent. Acepta
    /// teléfono y/o cédula (`sDni` o `sRif`); cualquiera puede venir vacío.
    /// Devuelve hasta `limit` resultados (cap dentro del impl). Match exacto
    /// sobre `sPhone`; sobre identificación se compara contra `sDni`/`sRif`
    /// tanto crudos como con prefijo (`V-`/`J-`).
    async fn find_clients_for_ai_lookup(
        &self,
        phone: Option<&str>,
        identification: Option<&str>,
    ) -> Result<Vec<AiClientLookup>, String>;
}

// ============================================
// 3. SalesRepository: Ventas, Pagos, Deudas
// ============================================
#[async_trait::async_trait]
pub trait SalesRepository {
    async fn get_latest_exchange_rate(&self) -> Result<f64, MongoError>;

    async fn find_active_debts_by_client_ids(
        &self,
        client_ids: &[ObjectId],
    ) -> Result<Vec<Debt>, String>;

    // Pagos
    async fn find_part_payments_by_debt_ids(
        &self,
        debt_ids: &[ObjectId],
    ) -> Result<Vec<PartPayment>, String>;
    async fn find_payments_by_ids(&self, payment_ids: &[ObjectId]) -> Result<Vec<Payment>, String>;
    async fn find_debt_by_id(&self, id: &str) -> Result<Option<crate::models::db::Debt>, String>;
    async fn find_client_owner_by_id(
        &self,
        client_id: &ObjectId,
    ) -> Result<Option<ClientOwner>, String>;

    // CAMBIOS:
    async fn find_user_payment_info_by_id(
        &self,
        user_id: &str,
    ) -> Result<Option<UserPaymentInfo>, String>;
    async fn find_payment_method_by_id(
        &self,
        id: &ObjectId,
    ) -> Result<Option<PaymentMethod>, String>;
    async fn create_payment_report(
        &self,
        report: PaymentReport,
    ) -> Result<InsertOneResult, MongoError>;

    async fn find_bank_list(&self) -> Result<Vec<Bank>, String>;

    async fn sum_active_payments_in_range(
        &self,
        client_ids: &[mongodb::bson::oid::ObjectId],
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> Result<f64, String>;

    async fn get_latest_payments(&self, limit: u32, owner_id: Option<&str>) -> Result<Vec<LatestPayment>, String>;

    async fn find_pending_reports_by_debt_ids(
        &self,
        debt_ids: &[ObjectId],
    ) -> Result<Vec<PaymentReport>, String>;

    async fn find_rejected_reports_by_debt_id(
        &self,
        debt_id: &ObjectId,
    ) -> Result<Vec<PaymentReport>, String>;

    async fn find_rejected_reports_by_client_id(
        &self,
        client_id: &ObjectId,
    ) -> Result<Vec<PaymentReport>, String>;

    /// Verifica si una referencia ya existe en Payments o PaymentReports
    /// Búsqueda bidireccional de derecha a izquierda (sufijo).
    /// Orden: Payments (mismo cliente) → Payments (global) → PaymentReports (mismo cliente) → PaymentReports (global)
    async fn check_reference(
        &self,
        id_client: &ObjectId,
        s_reference: &str,
    ) -> Result<Option<ReferenceMatchInfo>, String>;
}

// ============================================
// 4. UtilsRepository: Utils
// ============================================
#[async_trait::async_trait]
pub trait UtilsRepository {
    async fn find_latest_version(&self) -> Result<Option<LatestVersion>, String>;

    async fn exists_rate_for_date(
        &self,
        date_start: chrono::DateTime<chrono::Utc>,
        date_end: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, String>;
    async fn save_exchange_rate(
        &self,
        rate: f64,
        date: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), mongodb::error::Error>;

    async fn find_client_olt_position(&self, client_id: &str) -> Result<(String, String), ApiError>;
}

// ============================================
// 5. OnuRepository: Onu
// ============================================
#[async_trait::async_trait]
#[allow(dead_code)]
pub trait OnuRepository {
    // ZTE / Devices
    async fn get_device_serial_numbers(&self) -> Result<Vec<OnuIdentity>, String>;
    async fn save_onu_from_zte(&self, onu: OnuDetected, id_editor: &str) -> Result<(), String>;

    // IP Update
    async fn get_onus_for_update_ip(&self) -> Result<Vec<OnuForUpdateIp>, String>;
    async fn update_onu_ip(&self, onu: OnuIpUpdate, id_editor: &str) -> Result<(), String>;
}

/// Patch tri-state para `update_quick_reply`.
///
/// Para cada campo:
/// - `None` en el `Option` externo ⇒ el cliente no tocó ese campo (no hacer nada).
/// - `Some(None)` en los campos `Option<Option<_>>` ⇒ el cliente envió `null` explícito
///   (borrar el campo con `$unset`).
/// - `Some(Some(v))` ⇒ setear el valor (`$set`).
///
/// Los campos "planos" (`title`, `content`, `workspace_ids`, `active`) son
/// non-nullable en el modelo, por eso usan `Option<T>` directo (no admiten `null`).
pub struct UpdateQuickReplyPatch {
    pub title: Option<String>,
    pub content: Option<String>,
    pub workspace_ids: Option<Vec<ObjectId>>,
    pub active: Option<bool>,
    pub header: Option<Option<QuickReplyHeader>>,
    pub footer: Option<Option<String>>,
    pub buttons: Option<Option<Vec<QuickReplyButton>>>,
    pub list: Option<Option<QuickReplyList>>,
    pub cta_url: Option<Option<QuickReplyCtaUrl>>,
}

/// Patch tri-state para `update_conversation_ai_state`. Cada `Option` externo:
/// `None` ⇒ no tocar el campo; `Some(...)` ⇒ aplicar el valor.
///
/// Para `ai_active_agent_id` se usa anidado:
/// - `Some(Some(oid))` ⇒ `$set ai_active_agent_id = oid`
/// - `Some(None)` ⇒ `$unset ai_active_agent_id`
/// - `None` ⇒ no tocar
pub struct ConversationAiPatch<'a> {
    pub ai_active_agent_id: Option<Option<&'a ObjectId>>,
    pub ai_disabled: Option<bool>,
    /// Tri-state: `Some(Some(s))` setea, `Some(None)` limpia, `None` no toca.
    pub ai_transfer_context: Option<Option<&'a str>>,
}

/// Payload para `touch_conversation`: agrupa todos los campos denormalizados
/// que viven en `WaConversation` para renderizar el preview del listado
/// estilo WhatsApp (icono por tipo, checkmarks, "Tú: …", nombre de archivo).
///
/// La mayoría de los callers vienen en 4 flujos:
/// - webhook inbound → `direction="in"`, `increment_unread=true`
/// - send text/media outbound → `direction="out"`, `status=Some("sent")`
/// - send template outbound → idem
/// - initiate (primer mensaje) → idem
pub struct ConversationTouch<'a> {
    pub preview: &'a str,
    pub msg_type: &'a str,
    /// "in" | "out"
    pub direction: &'a str,
    pub wa_message_id: &'a str,
    /// Sólo outbound: UUID del agente.
    pub from_user_id: Option<&'a str>,
    /// Sólo documentos.
    pub media_filename: Option<&'a str>,
    /// Sólo outbound: "sent" inicial; en inbound va `None`.
    pub status: Option<&'a str>,
    pub increment_unread: bool,
    pub last_message_at: Option<mongodb::bson::DateTime>,
}

// ============================================
// 7. WhatsAppRepository: Soporte / Chat
// ============================================

/// Filtros para `audit_list_messages` (cross-conversation, SUPERADMIN only).
/// `conversation_ids` se resuelve en el handler a partir de `customer_phone`/
/// `business_phone` antes de llamar al repo — el repo no toca `WaConversations`.
pub struct AuditMessageFilter<'a> {
    pub from_date: Option<mongodb::bson::DateTime>,
    pub to_date: Option<mongodb::bson::DateTime>,
    /// UUID del agente — filtra por `sent_by` (sólo aplica a outbound; un
    /// inbound nunca tiene `sent_by` y se descarta cuando se filtra por agente).
    pub agent_id: Option<&'a str>,
    /// `None` = sin filtro de conversación. `Some([])` = filtro vacío que
    /// devuelve 0 resultados (el handler ya resolvió que no hay match).
    pub conversation_ids: Option<&'a [ObjectId]>,
    /// `"in"` o `"out"`. Otros valores se ignoran.
    pub direction: Option<&'a str>,
    pub msg_type: Option<&'a str>,
    /// Substring case-insensitive sobre `body`.
    pub search: Option<&'a str>,
    pub limit: i64,
    pub cursor: Option<&'a str>,
}

/// Filtro común para los aggregates del endpoint `/audit/metrics`. Todas las
/// queries comparten el mismo recorte temporal + opcionalmente un set de
/// `conversation_ids` (resueltos en el handler a partir de `business_phone`/
/// `customer_phone`) y/o un `agent_id` que recorta a `sent_by`.
pub struct AuditMetricsFilter<'a> {
    pub from_date: mongodb::bson::DateTime,
    pub to_date: mongodb::bson::DateTime,
    /// `None` = sin filtro. `Some([])` = sin matches (handler debe atajar antes).
    pub conversation_ids: Option<&'a [ObjectId]>,
    /// UUID del agente — filtra `sent_by`. Aplica scope "mensajes enviados
    /// por este agente"; los inbounds (sin `sent_by`) quedan fuera.
    pub agent_id: Option<&'a str>,
    /// `"day" | "week" | "month"`.
    pub granularity: &'a str,
}

/// Salida agregada por bucket temporal (mensajes inbound/outbound).
#[derive(Debug, Clone)]
pub struct AuditMessagesByDayBucket {
    pub date: String,
    pub inbound: u64,
    pub outbound: u64,
}

/// Salida agregada por agente.
#[derive(Debug, Clone)]
pub struct AuditMessagesByAgentBucket {
    pub agent_id: String,
    pub messages_sent: u64,
    pub conversations_handled: u64,
}

/// Salida agregada por tipo de mensaje.
#[derive(Debug, Clone)]
pub struct AuditMessagesByTypeBucket {
    pub msg_type: String,
    pub count: u64,
}

/// Resumen base de mensajes para el header del endpoint.
#[derive(Debug, Clone)]
pub struct AuditMessagesSummary {
    pub total: u64,
    pub inbound: u64,
    pub outbound: u64,
    /// Conversaciones distintas con ≥1 mensaje en el rango.
    pub distinct_conversations: u64,
}

/// Par primer-inbound/primer-outbound de una conversación dentro del rango,
/// usado para calcular `avg_response_time_seconds`. Si la conversación no
/// tiene un par válido (in seguido de out), la entrada no aparece.
#[derive(Debug, Clone)]
pub struct AuditFirstResponse {
    /// UUID del agente que respondió primero (responsable del delta para `by_agent`).
    pub agent_id: Option<String>,
    pub delta_seconds: i64,
}

/// Bucket temporal del ciclo de vida (eventos `created` / `closed` por día).
#[derive(Debug, Clone)]
pub struct AuditLifecycleByDayBucket {
    pub date: String,
    pub new_conversations: u64,
    pub closed_conversations: u64,
}

#[async_trait::async_trait]
pub trait WhatsAppRepository {
    async fn find_conversation_by_phones(&self, contact_phone: &str, business_phone: &str) -> Result<Option<WaConversation>, String>;
    async fn find_conversation_by_id(&self, id: &ObjectId) -> Result<Option<WaConversation>, String>;
    /// Crea o recupera una conversación identificada por el par `(contact_phone, business_phone)`.
    /// Retorna `(conv, created)` — `created = true` cuando se insertó en esta llamada.
    async fn upsert_conversation(&self, contact_phone: &str, business_phone: &str, name: Option<String>) -> Result<(WaConversation, bool), String>;
    async fn touch_conversation(
        &self,
        id: &ObjectId,
        touch: ConversationTouch<'_>,
    ) -> Result<(), String>;
    /// Si el último mensaje de la conversación tiene `wa_message_id == wa_id`,
    /// propaga el nuevo `status` a `last_message_status`. Devuelve `true`
    /// cuando efectivamente se actualizó (match en DB), `false` en caso
    /// contrario — útil para saber si hay que emitir evento WS.
    async fn update_conversation_status_if_last(
        &self,
        id: &ObjectId,
        wa_message_id: &str,
        status: &str,
    ) -> Result<bool, String>;
    /// Setea `last_inbound_at` en la conversación al timestamp indicado. Se usa
    /// desde el webhook al recibir un mensaje entrante para llevar la ventana
    /// de 24h (freeform) alineada con Meta.
    ///
    /// Atómicamente limpia `meta_throttle_until` (un inbound implica que el
    /// destinatario respondió, por lo que el throttle de engagement se libera).
    async fn update_last_inbound_at(&self, id: &ObjectId, when: mongodb::bson::DateTime) -> Result<(), String>;
    /// Setea `meta_throttle_until` cuando Meta nos rebota con error 131049
    /// (engagement throttle). Mientras `now < until`, el backend bloquea
    /// nuevos envíos a esa conversación.
    async fn set_meta_throttle_until(
        &self,
        id: &ObjectId,
        until: mongodb::bson::DateTime,
    ) -> Result<(), String>;
    /// Setea `client_id` (link al cliente ISP) de una conversación. Usado por
    /// `POST /conversations/initiate` al crear una nueva conversación que
    /// matchea por teléfono con un cliente existente.
    async fn update_conversation_client_id(&self, id: &ObjectId, client_id: &ObjectId) -> Result<(), String>;
    /// Backfill one-shot: rellena `last_inbound_at` en conversaciones que no lo
    /// tengan, usando el `timestamp` más reciente de los mensajes inbound. Se
    /// corre al arrancar para que la ventana de 24h funcione sobre datos
    /// existentes anteriores al deploy de Feature 3. Retorna la cantidad de
    /// documentos actualizados.
    async fn backfill_last_inbound_at(&self) -> Result<u64, String>;
    async fn save_message(&self, message: WaMessage) -> Result<WaMessage, String>;
    /// Cursor-based: `cursor` de la forma `<millis>_<hex_id>` para paginación descendente por `last_message_at`.
    async fn get_conversations(&self, status: Option<&str>, assigned_to: Option<&str>, business_phone: Option<&str>, cursor: Option<&str>, limit: i64) -> Result<Vec<WaConversation>, String>;
    /// Contadores agregados por categoría sobre el scope visible (opcionalmente
    /// acotado por `business_phone`). Resuelve los 5 contadores en una sola
    /// query usando `$facet` — es deliberadamente independiente de los filtros
    /// de la UI para que los números no cambien al filtrar la lista.
    async fn get_conversation_stats(
        &self,
        business_phone: Option<&str>,
        current_user_id: &str,
    ) -> Result<ConversationStats, String>;
    /// Cursor-based: `cursor` de la forma `<millis>_<hex_id>` para paginación descendente por `timestamp`.
    async fn get_messages(&self, conversation_id: &ObjectId, cursor: Option<&str>, limit: i64) -> Result<Vec<WaMessage>, String>;
    async fn update_conversation_status(&self, id: &ObjectId, status: &str) -> Result<(), String>;
    /// Cierra la conversación: status="closed" y libera al agente (`$unset assigned_to`).
    async fn close_conversation(&self, id: &ObjectId) -> Result<(), String>;
    /// Reabre una conversación cerrada → pending. No-op si ya no estaba cerrada.
    /// Retorna `true` si efectivamente cambió el estado.
    async fn reopen_conversation(&self, id: &ObjectId) -> Result<bool, String>;
    /// Actualiza el estado IA de la conversación (`ai_active_agent_id` y/o
    /// `ai_disabled`). Atómico — no toca status ni assigned_to. Devuelve
    /// `true` si el doc existía.
    async fn update_conversation_ai_state(
        &self,
        id: &ObjectId,
        patch: ConversationAiPatch<'_>,
    ) -> Result<bool, String>;
    async fn assign_conversation(&self, id: &ObjectId, assigned_to: Option<&str>) -> Result<(), String>;
    /// Intenta tomar una conversación pendiente. Retorna `None` si ya estaba asignada a otro
    /// (o no estaba en status `pending`), `Some(conv)` si la toma fue exitosa.
    async fn take_conversation(&self, id: &ObjectId, agent_id: &str) -> Result<Option<WaConversation>, String>;
    async fn reset_unread(&self, id: &ObjectId) -> Result<(), String>;
    async fn update_message_status(&self, wa_message_id: &str, status: &str) -> Result<Option<WaMessage>, String>;
    /// Marca todos los inbound de una conversación con status != "read" como "read".
    /// Persiste también `read_by_user_id = agent_id` y `read_at = now` en cada
    /// mensaje que efectivamente cambió — el filtro `status != "read"` garantiza
    /// first-read-wins (lecturas posteriores no sobreescriben al primer agente).
    /// Retorna la lista de `wa_message_id` que cambiaron (para emitir MENSAJES_VISTOS).
    async fn mark_inbound_as_read(
        &self,
        conversation_id: &ObjectId,
        agent_id: &str,
    ) -> Result<Vec<String>, String>;
    /// Busca un mensaje por `(conversation_id, idempotency_key)`. Fuente de verdad
    /// para reintentos idempotentes: permite detectar envíos previos `failed` y reintentarlos.
    async fn find_message_by_idempotency(
        &self,
        conversation_id: &ObjectId,
        idempotency_key: &str,
    ) -> Result<Option<WaMessage>, String>;
    /// Tras reintentar un envío fallido: actualiza `wa_message_id` y `status` del mismo doc.
    async fn update_message_retry(
        &self,
        id: &ObjectId,
        new_wa_message_id: &str,
        status: &str,
    ) -> Result<Option<WaMessage>, String>;
    /// Setea el `url_preview` del mensaje tras el fetch async. Devuelve el doc
    /// actualizado para que el handler arme el `MessageItem` completo y lo
    /// broadcastee por WS.
    async fn set_message_url_preview(
        &self,
        id: &ObjectId,
        preview: &UrlPreview,
    ) -> Result<Option<WaMessage>, String>;
    /// Batch-lookup por `wa_message_id`: devuelve un mapa `wamid → mensaje` para los
    /// que existan. Usado para enriquecer `MessageItem.reply_to` con un preview del
    /// mensaje citado (en una sola query, sin N+1).
    async fn find_messages_by_wa_ids(
        &self,
        wa_ids: &[String],
    ) -> Result<HashMap<String, WaMessage>, String>;
    /// Lookup por `media_id` (el id que Meta reporta en el webhook). Devuelve el
    /// primer mensaje que lo contiene. Usado por el endpoint que sirve el media
    /// para validar autorización y encontrar el `business_phone`.
    async fn find_message_by_media_id(
        &self,
        media_id: &str,
    ) -> Result<Option<WaMessage>, String>;

    // Per-agent "last opened" tracking
    /// Upsert del último momento en que `user_id` abrió `conversation_id`.
    async fn record_conversation_open(&self, user_id: &str, conversation_id: &ObjectId) -> Result<(), String>;
    /// Batch lookup: para un agente, devuelve `last_opened_at` por conversación.
    async fn get_conversation_opens(
        &self,
        user_id: &str,
        conversation_ids: &[ObjectId],
    ) -> Result<HashMap<ObjectId, mongodb::bson::DateTime>, String>;

    // Settings
    async fn find_wa_settings_by_phone(&self, phone: &str) -> Result<Option<WaSettings>, String>;
    /// Lookup por `_id` (hex de ObjectId). Usado por `POST /conversations/initiate`
    /// para resolver el workspace emisor desde el payload. No filtra por `active`.
    async fn find_wa_settings_by_id(&self, id: &ObjectId) -> Result<Option<WaSettings>, String>;
    /// Lookup por `phone_number_id` (el string de Meta, no el E.164). Usado por
    /// el endpoint de templates. No filtra por `active` — un admin puede listar
    /// templates de un número pausado.
    async fn find_wa_settings_by_phone_number_id(&self, phone_number_id: &str) -> Result<Option<WaSettings>, String>;
    /// Listado de WaSettings cuyo `whatsapp_business_account_id` está vacío.
    /// Usado por la tarea de backfill al arrancar.
    async fn find_wa_settings_missing_waba(&self) -> Result<Vec<WaSettings>, String>;
    /// Setea el `whatsapp_business_account_id` para un doc puntual.
    /// Usado sólo por el backfill — el CRUD normal pasa por `update_wa_settings`.
    async fn set_wa_settings_waba_id(&self, id: &ObjectId, waba_id: &str) -> Result<(), String>;
    /// Batch-lookup: `business_phone → workspace_name`. Ignora el flag `active` (es sólo display).
    /// Los números sin `WaSettings` configurado o con `workspace_name` vacío quedan fuera del mapa.
    async fn get_workspace_names(&self, phones: &[String]) -> Result<HashMap<String, String>, String>;
    async fn get_all_wa_settings(&self) -> Result<Vec<WaSettings>, String>;
    async fn create_wa_settings(&self, settings: WaSettings) -> Result<WaSettings, String>;
    /// Actualiza campos mutables de `WaSettings`. Todos opcionales: `None` significa "no tocar".
    /// `access_token_cipher`: debe venir ya cifrado (AES-GCM). Si se pasa `Some("")` se ignora
    /// (para que `PUT` con `access_token: ""` no borre el token).
    async fn update_wa_settings(
        &self,
        id: &ObjectId,
        workspace_name: Option<String>,
        phone_number_id: Option<String>,
        whatsapp_business_account_id: Option<String>,
        access_token_cipher: Option<String>,
        agents: Option<Vec<String>>,
        active: Option<bool>,
        purposes: Option<crate::models::whatsapp::WaPurposesPatch>,
    ) -> Result<(), String>;
    async fn delete_wa_settings(&self, id: &ObjectId) -> Result<(), String>;
    /// Busca `WaSettings` activos con el propósito `purpose` configurado.
    /// `purpose` es uno de: `"otp"`, `"notifications"`, `"payment_reminder"`.
    /// Devuelve todos los candidatos; el caller elige (p.ej. round-robin o el primero).
    async fn find_wa_settings_for_purpose(
        &self,
        purpose: &str,
    ) -> Result<Vec<WaSettings>, String>;

    // Quick replies (snippets)
    /// Devuelve los `WaSettings._id` donde `user_id` aparece en `agents`.
    /// Se usa para chequear membresía al autorizar create/delete/duplicate
    /// de quick replies.
    async fn get_user_workspaces(&self, user_id: &str) -> Result<Vec<ObjectId>, String>;
    /// Devuelve `true` si **todos** los `ids` existen en `WaSettings`. Usado para validar `workspace_ids`.
    async fn wa_settings_exist(&self, ids: &[ObjectId]) -> Result<bool, String>;
    /// Listado de quick-replies. La autorización del caller se resuelve en el
    /// handler vía `bCanChat`; acá no se filtra por membresía de workspace.
    ///
    /// - `filter_workspace_id = None` → devuelve todas las quick replies.
    /// - `filter_workspace_id = Some(id)` → devuelve las que tienen `id` en
    ///   `workspace_ids` **o** las globales (`workspace_ids: []`, aplican a
    ///   cualquier workspace).
    ///
    /// Si `active_filter` viene, filtra por `active = bool` (None ⇒ sin filtro).
    async fn list_quick_replies(
        &self,
        filter_workspace_id: Option<&ObjectId>,
        active_filter: Option<bool>,
    ) -> Result<Vec<WaQuickReply>, String>;
    async fn find_quick_reply_by_id(&self, id: &ObjectId) -> Result<Option<WaQuickReply>, String>;
    async fn create_quick_reply(&self, doc: WaQuickReply) -> Result<WaQuickReply, String>;
    /// Actualización parcial tri-state (ver `UpdateQuickReplyPatch`). Devuelve el doc
    /// actualizado (o `None` si no existe).
    async fn update_quick_reply(
        &self,
        id: &ObjectId,
        patch: UpdateQuickReplyPatch,
    ) -> Result<Option<WaQuickReply>, String>;
    /// Toggle simple de `active`. Devuelve el doc actualizado (o `None` si no existe).
    async fn set_quick_reply_active(
        &self,
        id: &ObjectId,
        active: bool,
    ) -> Result<Option<WaQuickReply>, String>;
    /// `$inc use_count` + `$set last_used_at = now`. Se llama tras un envío exitoso.
    async fn increment_quick_reply_use(&self, id: &ObjectId) -> Result<(), String>;
    async fn delete_quick_reply(&self, id: &ObjectId) -> Result<bool, String>;

    // Conversation lifecycle events (auditoría)
    /// Persiste un evento de ciclo de vida (`created`/`taken`/`transferred`/
    /// `closed`/`reopened`). Los handlers que ejecutan la acción son los que
    /// llaman este método después del UPDATE exitoso de `WaConversations`.
    /// Best-effort: el caller debe loggear-y-seguir si retorna error (no
    /// bloquear la respuesta HTTP por una falla de auditoría).
    async fn record_conversation_event(
        &self,
        input: WaConversationEventInput<'_>,
    ) -> Result<(), String>;

    /// Lista los eventos de una conversación ordenados por `created_at` ASC.
    /// Usado por el endpoint de timeline.
    async fn list_conversation_events(
        &self,
        conversation_id: &ObjectId,
    ) -> Result<Vec<WaConversationEvent>, String>;

    /// Backfill one-shot: para cada `WaConversation` que no tenga ningún evento,
    /// siembra `created` con `created_at` y, si `assigned_to.is_some()`, un
    /// `taken` con `updated_at` (o `last_message_at` como mejor proxy disponible).
    /// Idempotente: skipea conversaciones que ya tengan al menos un evento.
    /// Retorna la cantidad de eventos insertados.
    async fn backfill_conversation_events(&self) -> Result<u64, String>;

    // Audit cross-conversation
    /// Lista mensajes con filtros de auditoría, paginación cursor descendente
    /// por `(timestamp, _id)`. SUPERADMIN-only — el handler valida el rol.
    async fn audit_list_messages(
        &self,
        filter: AuditMessageFilter<'_>,
    ) -> Result<Vec<WaMessage>, String>;

    /// Cuenta mensajes que matchean el filtro (ignora `cursor` y `limit`).
    /// Usado por `/audit/export` para chequear el cap de 100k antes de
    /// materializar el CSV.
    async fn audit_count_messages(
        &self,
        filter: &AuditMessageFilter<'_>,
    ) -> Result<u64, String>;

    /// Resuelve los `_id` de `WaConversations` que matchean por
    /// `customer_phone` (campo `phone`) y/o `business_phone`. Si ambos son
    /// `None`, retorna error — el caller siempre debe pasar al menos uno.
    async fn find_conversation_ids_by_phones(
        &self,
        customer_phone: Option<&str>,
        business_phone: Option<&str>,
    ) -> Result<Vec<ObjectId>, String>;

    /// Batch-lookup: `_id → WaConversation` para los IDs dados. Usado por
    /// el handler de auditoría para resolver `customer_phone`/`business_phone`/
    /// `customer_name` de cada mensaje sin un `$lookup`.
    async fn find_conversations_by_ids(
        &self,
        ids: &[ObjectId],
    ) -> Result<HashMap<ObjectId, WaConversation>, String>;

    /// Cantidad total de mensajes en una conversación. Usado por el endpoint
    /// de timeline para mostrar un contador sin paginar.
    async fn count_messages_for_conversation(
        &self,
        conversation_id: &ObjectId,
    ) -> Result<u64, String>;

    // Métricas (`/audit/metrics`)
    /// Resumen base: totales por dirección + conversaciones distintas con
    /// actividad en el rango.
    async fn audit_messages_summary(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<AuditMessagesSummary, String>;

    /// Mensajes por bucket temporal (granularity).
    async fn audit_messages_by_day(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<Vec<AuditMessagesByDayBucket>, String>;

    /// Mensajes por agente (sólo `out` con `sent_by` no nulo).
    async fn audit_messages_by_agent(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<Vec<AuditMessagesByAgentBucket>, String>;

    /// Distribución por `msg_type`.
    async fn audit_messages_by_type(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<Vec<AuditMessagesByTypeBucket>, String>;

    /// Por cada conversación con un par (in seguido de out) en el rango,
    /// devuelve el delta entre el primer in y el primer out posterior.
    /// Lista vacía si no hubo pares.
    async fn audit_first_responses(
        &self,
        filter: &AuditMetricsFilter<'_>,
    ) -> Result<Vec<AuditFirstResponse>, String>;

    /// Eventos de ciclo de vida agregados por bucket temporal — sólo `created`
    /// y `closed`. Si `business_phone` está, se filtra por ese workspace.
    /// Si `conversation_ids` está, se acota a esas conversaciones (usado al
    /// filtrar por `customer_phone` en `/audit/metrics`).
    async fn audit_lifecycle_by_day(
        &self,
        from: mongodb::bson::DateTime,
        to: mongodb::bson::DateTime,
        business_phone: Option<&str>,
        conversation_ids: Option<&[ObjectId]>,
        granularity: &str,
    ) -> Result<Vec<AuditLifecycleByDayBucket>, String>;

    /// Para cada conversación cerrada en el rango, retorna la duración en
    /// segundos entre el primer evento `created` y el último `closed`. Lista
    /// vacía si no hubo cierres en el rango.
    async fn audit_resolution_times(
        &self,
        from: mongodb::bson::DateTime,
        to: mongodb::bson::DateTime,
        business_phone: Option<&str>,
        conversation_ids: Option<&[ObjectId]>,
    ) -> Result<Vec<i64>, String>;
}

// ============================================
// 8. WaTemplateRepository: Plantillas WhatsApp
// ============================================

/// Filtros para `list_templates_filtered`.
pub struct WaTemplateListFilter<'a> {
    /// Requerido — filtra por `phone_number_id`.
    pub phone_number_id: &'a str,
    /// Filtra por uno o varios estados. `None` trae todos.
    pub status: Option<&'a [WaTemplateStatus]>,
    /// Filtra por categoría. `None` trae todas.
    pub category: Option<WaTemplateCategory>,
    /// Si `true`, filtra sólo `is_system == true`. Default `false`.
    pub only_system: bool,
    /// Substring case-insensitive sobre `display_name` y `name` (OR). `None` sin filtro.
    pub search: Option<&'a str>,
    /// Resultados por página. Máx 100 — el impl aplica hard-cap.
    pub limit: i64,
    /// Cursor opaco (`<millis>_<hex_id>`, mismo patrón que `get_conversations`).
    pub cursor: Option<&'a str>,
}

/// Patch parcial para `update_template`. Sólo se aplican los campos `Some`.
/// Para los campos nullable (`rejection_reason`, `meta_template_id`) se usa
/// tri-state: `Some(None)` limpia el campo, `Some(Some(v))` lo setea.
pub struct WaTemplateUpdatePatch {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub name_input: Option<String>,
    pub category: Option<WaTemplateCategory>,
    pub components: Option<Vec<serde_json::Value>>,
    pub body_placeholders: Option<u32>,
    pub status: Option<WaTemplateStatus>,
    /// Tri-state: `Some(None)` limpia, `Some(Some(s))` setea.
    pub rejection_reason: Option<Option<String>>,
    /// Tri-state: `Some(None)` limpia, `Some(Some(s))` setea.
    pub meta_template_id: Option<Option<String>>,
    pub is_system: Option<bool>,
    pub submit_to_meta: Option<bool>,
}

#[async_trait::async_trait]
#[allow(dead_code)]
pub trait WaTemplateRepository {
    /// Inserta una nueva plantilla. En caso de violación de unicidad
    /// `(phone_number_id, name, language)` retorna `Err("name_already_exists")`.
    async fn create_template(&self, doc: WaTemplate) -> Result<WaTemplate, String>;

    /// Busca una plantilla por su `_id`.
    async fn find_template_by_id(&self, id: &ObjectId) -> Result<Option<WaTemplate>, String>;

    /// Busca por la tripleta única `(phone_number_id, name, language)`.
    async fn find_template_by_phone_name_lang(
        &self,
        phone_number_id: &str,
        name: &str,
        language: &str,
    ) -> Result<Option<WaTemplate>, String>;

    /// Busca por `meta_template_id` (el `id` que expone Meta).
    async fn find_template_by_meta_id(
        &self,
        meta_template_id: &str,
    ) -> Result<Option<WaTemplate>, String>;

    /// Listado paginado con filtros. Sort: `{ created_at: -1, _id: -1 }`.
    async fn list_templates_filtered(
        &self,
        filter: WaTemplateListFilter<'_>,
    ) -> Result<Vec<WaTemplate>, String>;

    /// Actualización parcial tri-state. Devuelve el doc actualizado o `None`
    /// si no existe.
    async fn update_template(
        &self,
        id: &ObjectId,
        patch: WaTemplateUpdatePatch,
    ) -> Result<Option<WaTemplate>, String>;

    /// Actualiza `status` y opcionalmente `rejection_reason` por `meta_template_id`.
    /// Devuelve `(doc_actualizado, status_previo)` — el `prev_status` se usa para
    /// armar el evento WS `WA_TEMPLATE_UPDATED`. `None` si no se encontró el doc.
    async fn update_template_status(
        &self,
        meta_template_id: &str,
        status: WaTemplateStatus,
        rejection_reason: Option<String>,
    ) -> Result<Option<(WaTemplate, WaTemplateStatus)>, String>;

    /// Hard-delete. Retorna `true` si el doc existía.
    async fn delete_template(&self, id: &ObjectId) -> Result<bool, String>;

    /// Busca en `WaSettings.purposes` por `phone_number_id` y `template_name == name`,
    /// devolviendo los propósitos donde está en uso. Usado para bloquear borrados.
    async fn count_templates_in_purposes(
        &self,
        phone_number_id: &str,
        name: &str,
    ) -> Result<Vec<crate::models::whatsapp::WaPurposeUsage>, String>;
}

// ============================================
// 9. WaTemplateMediaRepository: Media para headers de templates
// ============================================

/// Input para persistir un binario de media en GridFS.
pub struct StoreTemplateMediaInput<'a> {
    pub phone_number_id: &'a str,
    /// "IMAGE" | "VIDEO" | "DOCUMENT"
    pub format: &'a str,
    pub mime_type: &'a str,
    /// SHA-256 hex del contenido
    pub sha256: &'a str,
    pub bytes: &'a [u8],
    /// UUID del usuario que sube el archivo
    pub uploaded_by: &'a str,
    pub uploaded_by_name: &'a str,
}

/// Referencia a un archivo de media almacenado en GridFS.
#[allow(dead_code)]
pub struct WaTemplateMediaRef {
    pub id: mongodb::bson::oid::ObjectId,
    pub phone_number_id: String,
    pub mime_type: String,
    pub sha256: String,
    pub file_size: u64,
}

#[allow(dead_code)]
#[async_trait::async_trait]
pub trait WaTemplateMediaRepository {
    /// Persiste el binario en GridFS. Dedup por `(phone_number_id, sha256)`:
    /// si ya existe, retorna el `media_id` existente sin re-subir.
    async fn store_template_media(
        &self,
        input: StoreTemplateMediaInput<'_>,
    ) -> Result<WaTemplateMediaRef, String>;

    /// Busca metadatos de un archivo de media por su `_id` de GridFS.
    async fn find_template_media_by_id(
        &self,
        id: &mongodb::bson::oid::ObjectId,
    ) -> Result<Option<WaTemplateMediaRef>, String>;

    /// Lee el binario completo y el mime_type de un archivo de GridFS.
    /// Retorna `Some((bytes, mime_type))` o `None` si no existe.
    async fn read_template_media_bytes(
        &self,
        id: &mongodb::bson::oid::ObjectId,
    ) -> Result<Option<(Vec<u8>, String)>, String>;

    /// Elimina un archivo de GridFS. Retorna `true` si existía, `false` si no.
    async fn delete_template_media(
        &self,
        id: &mongodb::bson::oid::ObjectId,
    ) -> Result<bool, String>;
}

// ============================================
// 10. WaTicketRepository: Tickets de soporte derivados de WhatsApp
// ============================================

/// Filtros para `list_tickets`. Todos opcionales; el handler colapsa
/// status y rango de fechas según el rol del caller.
pub struct TicketListFilter<'a> {
    pub status: Option<&'a str>,
    /// UUID. Si viene combinado con `created_by_id`, el repo arma un `$or`
    /// (visible al agente porque es asignado **o** creador).
    pub assigned_to_id: Option<&'a str>,
    pub created_by_id: Option<&'a str>,
    pub conversation_id: Option<&'a ObjectId>,
    pub customer_phone: Option<&'a str>,
    pub business_phone: Option<&'a str>,
    pub from_date: Option<mongodb::bson::DateTime>,
    pub to_date: Option<mongodb::bson::DateTime>,
    /// Substring case-insensitive sobre `reason` y `resolution`.
    pub search: Option<&'a str>,
    /// Si vienen tags, el ticket debe contener **todas** (`$all`). El front
    /// no necesita match parcial — los tags son etiquetas exactas.
    pub tags: Option<&'a [String]>,
    pub limit: i64,
    /// Cursor opaco `<millis>_<hex_id>` (descendente por `created_at`).
    pub cursor: Option<&'a str>,
}

/// Patch que aplica una transición de estado sobre un ticket existente +
/// appendea la entry correspondiente al timeline embebido. El repo es
/// responsable de hacer ambos cambios en un único `$set`/`$push`.
pub struct TicketActionUpdate<'a> {
    /// Status nuevo (`open` | `in_progress` | `resolved` | `closed` | `cancelled`).
    pub new_status: &'a str,
    /// Si la acción es `transfer`, el agente destino (Some). En otras
    /// acciones que no tocan asignación, mandar `None` y `assignment_changed=false`.
    pub assigned_to_id: Option<&'a str>,
    pub assigned_to_name: Option<&'a str>,
    /// Si `true` y `assigned_to_id` es `None`, el repo limpia el campo
    /// (`$unset`). Útil para `cancel`/`close`/`reopen` que sueltan al agente.
    pub clear_assignment: bool,
    /// Si la acción setea/limpia asignación efectivamente. Cuando es `false`
    /// el repo deja `assigned_to_*` como estaba.
    pub assignment_changed: bool,
    /// Sólo en `resolve`/`close`: texto libre de la resolución.
    pub resolution: Option<&'a str>,
    /// `true` cuando `new_status == "resolved"` por primera vez. Marca `resolved_at`.
    pub set_resolved_at: bool,
    /// `true` cuando `new_status == "closed"` por primera vez. Marca `closed_at`.
    pub set_closed_at: bool,
    /// Entry del timeline que se appendea con la acción.
    pub timeline_entry: WaTicketTimelineEntry,
}

#[async_trait::async_trait]
pub trait WaTicketRepository {
    /// Inserta un ticket nuevo. El caller ya validó que no haya un ticket
    /// abierto previo + idempotency. Si el back se cae entre dos requests
    /// idénticos, el chequeo de idempotency_key se hace por separado vía
    /// `find_ticket_by_idempotency`.
    async fn create_ticket(&self, ticket: WaTicket) -> Result<WaTicket, String>;

    async fn find_ticket_by_id(&self, id: &ObjectId) -> Result<Option<WaTicket>, String>;

    /// Busca el primer ticket en estado `open` o `in_progress` para una
    /// conversación. Usado para emitir `409 ticket_already_open` con el id
    /// del ticket conflictivo.
    async fn find_open_ticket_for_conversation(
        &self,
        conversation_id: &ObjectId,
    ) -> Result<Option<WaTicket>, String>;

    /// Soporte de `Idempotency-Key` en `POST /tickets`: si el mismo agente
    /// reintenta con la misma key, devolvemos el ticket creado original sin
    /// duplicar. La unicidad es `(created_by_id, idempotency_key)` —
    /// distintos agentes pueden usar la misma key sin colisión.
    async fn find_ticket_by_idempotency(
        &self,
        created_by_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<WaTicket>, String>;

    async fn list_tickets(
        &self,
        filter: TicketListFilter<'_>,
    ) -> Result<Vec<WaTicket>, String>;

    /// Aplica la transición en una sola operación: `$set status` (+ campos
    /// derivados) y `$push timeline`. Devuelve el doc actualizado. `None`
    /// si el `_id` no existe.
    async fn update_ticket_action(
        &self,
        id: &ObjectId,
        patch: TicketActionUpdate<'_>,
    ) -> Result<Option<WaTicket>, String>;
}

// ============================================
// 11. AiAgentRepository: agentes IA y FAQs por agente
// ============================================

#[async_trait::async_trait]
#[allow(dead_code)]
pub trait AiAgentRepository {
    /// Lista agentes. Si `workspace_id` viene, filtra por agentes que tengan
    /// ese id dentro de `workspace_ids`.
    async fn list_ai_agents(
        &self,
        workspace_id: Option<&ObjectId>,
    ) -> Result<Vec<AiAgent>, String>;

    /// Devuelve el agente activo (`enabled=true`) más viejo que atiende ese
    /// workspace. Opción A para selección hasta que llegue el routing
    /// (recepcionista). `None` si no hay agente disponible.
    async fn find_active_agent_for_workspace(
        &self,
        workspace_id: &ObjectId,
    ) -> Result<Option<AiAgent>, String>;

    async fn find_ai_agent_by_id(&self, id: &ObjectId) -> Result<Option<AiAgent>, String>;

    /// Batch-lookup por `_id`. Devuelve sólo los que existen. Usado para
    /// validar `allowed_targets` de `transfer_to_agent` y para resolver el
    /// agente activo elegido cuando una conv tiene `ai_active_agent_id`.
    async fn find_ai_agents_by_ids(&self, ids: &[ObjectId]) -> Result<Vec<AiAgent>, String>;

    /// Recepcionista del workspace: agente con `is_receptionist=true`,
    /// `enabled=true`, que tenga `workspace_id` en su `workspace_ids`. Si hay
    /// más de uno (config inconsistente), devuelve el más viejo. `None` si no
    /// hay ninguno marcado como recepcionista.
    async fn find_receptionist_for_workspace(
        &self,
        workspace_id: &ObjectId,
    ) -> Result<Option<AiAgent>, String>;

    async fn create_ai_agent(&self, agent: AiAgent) -> Result<AiAgent, String>;

    /// Replace full-doc. Caller pasa el doc con `_id`, `created_at` y
    /// `updated_at` ya seteados. Devuelve `None` si no existe.
    async fn replace_ai_agent(
        &self,
        id: &ObjectId,
        agent: AiAgent,
    ) -> Result<Option<AiAgent>, String>;

    /// `true` si el agent existía. También borra todas sus FAQs (cascada).
    async fn delete_ai_agent(&self, id: &ObjectId) -> Result<bool, String>;

    /// FAQs de un agente, ordenadas por `created_at` desc.
    async fn list_ai_agent_faqs(&self, agent_id: &ObjectId) -> Result<Vec<AiAgentFaq>, String>;

    async fn find_ai_agent_faq_by_id(&self, id: &ObjectId) -> Result<Option<AiAgentFaq>, String>;

    async fn create_ai_agent_faq(&self, faq: AiAgentFaq) -> Result<AiAgentFaq, String>;

    async fn update_ai_agent_faq(
        &self,
        id: &ObjectId,
        question: Option<String>,
        answer: Option<String>,
        tags: Option<Vec<String>>,
    ) -> Result<Option<AiAgentFaq>, String>;

    async fn delete_ai_agent_faq(&self, id: &ObjectId) -> Result<bool, String>;

    /// Persiste un turno IA. Usado por `dispatch.rs` (shadow + live).
    async fn create_ai_interaction(&self, interaction: AiInteraction) -> Result<(), String>;

    /// Lee los últimos N mensajes de una conversación, ordenados por
    /// `timestamp` ASC (cronológico). Usado por el dispatch para armar el
    /// history que va al runner.
    async fn list_recent_messages_for_conversation(
        &self,
        conversation_id: &ObjectId,
        limit: i64,
    ) -> Result<Vec<crate::models::whatsapp::WaMessage>, String>;

    // ─── AiPlans (datos de negocio editables) ──────────────────────────────
    /// Lista planes. `only_active=true` recorta a `active=true`. Sort
    /// ascendente por `display_order` y `mbps` como tiebreaker.
    async fn list_ai_plans(&self, only_active: bool) -> Result<Vec<AiPlan>, String>;
    async fn find_ai_plan_by_id(&self, id: &ObjectId) -> Result<Option<AiPlan>, String>;
    async fn create_ai_plan(&self, plan: AiPlan) -> Result<AiPlan, String>;
    async fn replace_ai_plan(&self, id: &ObjectId, plan: AiPlan)
        -> Result<Option<AiPlan>, String>;
    async fn delete_ai_plan(&self, id: &ObjectId) -> Result<bool, String>;
    /// `true` si la colección está vacía. Usado por el seed lazy al arrancar.
    async fn ai_plans_is_empty(&self) -> Result<bool, String>;

    // ─── AiCoverageZones ────────────────────────────────────────────────────
    async fn list_ai_coverage_zones(
        &self,
        only_active: bool,
    ) -> Result<Vec<AiCoverageZone>, String>;
    async fn find_ai_coverage_zone_by_id(
        &self,
        id: &ObjectId,
    ) -> Result<Option<AiCoverageZone>, String>;
    async fn create_ai_coverage_zone(
        &self,
        zone: AiCoverageZone,
    ) -> Result<AiCoverageZone, String>;
    async fn replace_ai_coverage_zone(
        &self,
        id: &ObjectId,
        zone: AiCoverageZone,
    ) -> Result<Option<AiCoverageZone>, String>;
    async fn delete_ai_coverage_zone(&self, id: &ObjectId) -> Result<bool, String>;
    async fn ai_coverage_zones_is_empty(&self) -> Result<bool, String>;
}

// ============================================
// TRAIT MAESTRO
// ============================================
pub trait Db:
    AuthRepository
    + UserRepository
    + ProfileRepository
    + SalesRepository
    + OnuRepository
    + UtilsRepository
    + WhatsAppRepository
    + WaTemplateRepository
    + WaTemplateMediaRepository
    + WaTicketRepository
    + AiAgentRepository
    + Clone
    + Send
    + Sync
    + 'static
{
}
