pub mod mongo;
use crate::models::db::{ActiveClientBalance, ClientDetail, ClientListItem, ClientStatusHistoryItem, CustomerInfoItem, LatestPayment, LatestVersion, OnuForUpdateIp, OnuIdentity, OnuIpUpdate, SolvencyCounts, Tax};
use crate::models::whatsapp::{UrlPreview, WaConversation, WaMessage, WaQuickReply, WaSettings};
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

// ============================================
// 7. WhatsAppRepository: Soporte / Chat
// ============================================
#[async_trait::async_trait]
pub trait WhatsAppRepository {
    async fn find_conversation_by_phones(&self, contact_phone: &str, business_phone: &str) -> Result<Option<WaConversation>, String>;
    async fn find_conversation_by_id(&self, id: &ObjectId) -> Result<Option<WaConversation>, String>;
    /// Crea o recupera una conversación identificada por el par `(contact_phone, business_phone)`.
    /// Retorna `(conv, created)` — `created = true` cuando se insertó en esta llamada.
    async fn upsert_conversation(&self, contact_phone: &str, business_phone: &str, name: Option<String>) -> Result<(WaConversation, bool), String>;
    async fn touch_conversation(&self, id: &ObjectId, preview: &str, increment_unread: bool, last_message_at: Option<mongodb::bson::DateTime>) -> Result<(), String>;
    async fn save_message(&self, message: WaMessage) -> Result<WaMessage, String>;
    /// Cursor-based: `cursor` de la forma `<millis>_<hex_id>` para paginación descendente por `last_message_at`.
    async fn get_conversations(&self, status: Option<&str>, assigned_to: Option<&str>, business_phone: Option<&str>, cursor: Option<&str>, limit: i64) -> Result<Vec<WaConversation>, String>;
    /// Cursor-based: `cursor` de la forma `<millis>_<hex_id>` para paginación descendente por `timestamp`.
    async fn get_messages(&self, conversation_id: &ObjectId, cursor: Option<&str>, limit: i64) -> Result<Vec<WaMessage>, String>;
    async fn update_conversation_status(&self, id: &ObjectId, status: &str) -> Result<(), String>;
    /// Cierra la conversación: status="closed" y libera al agente (`$unset assigned_to`).
    async fn close_conversation(&self, id: &ObjectId) -> Result<(), String>;
    /// Reabre una conversación cerrada → pending. No-op si ya no estaba cerrada.
    /// Retorna `true` si efectivamente cambió el estado.
    async fn reopen_conversation(&self, id: &ObjectId) -> Result<bool, String>;
    async fn assign_conversation(&self, id: &ObjectId, assigned_to: Option<&str>) -> Result<(), String>;
    /// Intenta tomar una conversación pendiente. Retorna `None` si ya estaba asignada a otro
    /// (o no estaba en status `pending`), `Some(conv)` si la toma fue exitosa.
    async fn take_conversation(&self, id: &ObjectId, agent_id: &str) -> Result<Option<WaConversation>, String>;
    async fn reset_unread(&self, id: &ObjectId) -> Result<(), String>;
    async fn update_message_status(&self, wa_message_id: &str, status: &str) -> Result<Option<WaMessage>, String>;
    /// Marca todos los inbound de una conversación con status != "read" como "read".
    /// Retorna la lista de `wa_message_id` que cambiaron (para emitir MENSAJES_VISTOS).
    async fn mark_inbound_as_read(&self, conversation_id: &ObjectId) -> Result<Vec<String>, String>;
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
        access_token_cipher: Option<String>,
        agents: Option<Vec<String>>,
        active: Option<bool>,
    ) -> Result<(), String>;
    async fn delete_wa_settings(&self, id: &ObjectId) -> Result<(), String>;

    // Quick replies (snippets)
    /// Devuelve los `WaSettings._id` donde `user_id` aparece en `agents`.
    /// Se usa como scope para filtrar quick-replies y validar permisos de escritura.
    async fn get_user_workspaces(&self, user_id: &str) -> Result<Vec<ObjectId>, String>;
    /// Devuelve `true` si **todos** los `ids` existen en `WaSettings`. Usado para validar `workspace_ids`.
    async fn wa_settings_exist(&self, ids: &[ObjectId]) -> Result<bool, String>;
    /// Listado de quick-replies cuyo `workspace_ids` intersecta con `user_workspaces`.
    /// Si `filter_workspace_id` viene, filtra además por ese workspace puntual.
    async fn list_quick_replies(
        &self,
        user_workspaces: &[ObjectId],
        filter_workspace_id: Option<&ObjectId>,
    ) -> Result<Vec<WaQuickReply>, String>;
    async fn find_quick_reply_by_id(&self, id: &ObjectId) -> Result<Option<WaQuickReply>, String>;
    async fn create_quick_reply(&self, doc: WaQuickReply) -> Result<WaQuickReply, String>;
    /// `None` en un campo ⇒ no tocar. Devuelve el doc actualizado (o `None` si no existe).
    async fn update_quick_reply(
        &self,
        id: &ObjectId,
        title: Option<String>,
        content: Option<String>,
        workspace_ids: Option<Vec<ObjectId>>,
    ) -> Result<Option<WaQuickReply>, String>;
    async fn delete_quick_reply(&self, id: &ObjectId) -> Result<bool, String>;
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
    + Clone
    + Send
    + Sync
    + 'static
{
}
