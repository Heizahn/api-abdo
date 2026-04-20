use mongodb::bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================
// DOCUMENTOS DE BASE DE DATOS
// ============================================

/// Conversación de WhatsApp (colección `wa_conversations`)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaConversation {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Número en formato E.164 sin "+" (ej: "584141234567")
    pub phone: String,
    /// Nombre del contacto (provisto por WhatsApp)
    pub name: Option<String>,
    /// Cliente ISP vinculado si el número coincide
    pub client_id: Option<ObjectId>,
    /// "open" | "closed" | "waiting"
    pub status: String,
    /// UUID del agente asignado
    pub assigned_to: Option<String>,
    pub last_message_at: DateTime,
    pub last_message_preview: Option<String>,
    pub unread_count: i32,
    pub created_at: DateTime,
}

/// Mensaje individual de WhatsApp (colección `wa_messages`)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaMessage {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub conversation_id: ObjectId,
    /// ID de mensaje de WhatsApp (wamid...) — usado para deduplicar y actualizar status
    pub wa_message_id: String,
    /// "inbound" | "outbound"
    pub direction: String,
    /// "text" | "image" | "document" | "audio" | "video" | "template" | "interactive" | "unknown"
    pub msg_type: String,
    pub body: Option<String>,
    /// ID de media en WhatsApp (para imágenes/documentos)
    pub media_id: Option<String>,
    /// Solo para outbound: "sent" | "delivered" | "read" | "failed"
    pub status: Option<String>,
    /// UUID del agente que envió (solo outbound)
    pub sent_by: Option<String>,
    pub timestamp: DateTime,
}

// ============================================
// PAYLOAD DEL WEBHOOK DE META
// ============================================

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookPayload {
    #[allow(dead_code)]
    pub object: Option<String>,
    pub entry: Option<Vec<WebhookEntry>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookEntry {
    pub changes: Option<Vec<WebhookChange>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookChange {
    pub value: Option<WebhookValue>,
    pub field: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookValue {
    pub contacts: Option<Vec<WebhookContact>>,
    pub messages: Option<Vec<InboundMessage>>,
    pub statuses: Option<Vec<MessageStatus>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookContact {
    pub profile: Option<WebhookProfile>,
    pub wa_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookProfile {
    pub name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InboundMessage {
    pub from: String,
    pub id: String,
    #[allow(dead_code)]
    pub timestamp: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub text: Option<InboundText>,
    pub image: Option<InboundMedia>,
    pub document: Option<InboundMedia>,
    pub audio: Option<InboundMedia>,
    pub video: Option<InboundMedia>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InboundText {
    pub body: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InboundMedia {
    pub id: Option<String>,
    pub caption: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MessageStatus {
    pub id: String,
    pub status: String,
    #[allow(dead_code)]
    pub timestamp: String,
    #[allow(dead_code)]
    pub recipient_id: Option<String>,
}

// ============================================
// MODELOS HTTP (Request / Response)
// ============================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    /// Texto del mensaje a enviar
    pub body: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateConversationStatusRequest {
    /// "open" | "closed" | "waiting"
    pub status: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AssignConversationRequest {
    /// UUID del agente. Null para desasignar.
    pub assigned_to: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationListItem {
    pub id: String,
    pub phone: String,
    pub name: Option<String>,
    pub status: String,
    pub assigned_to: Option<String>,
    pub last_message_at: String,
    pub last_message_preview: Option<String>,
    pub unread_count: i32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationDetail {
    pub id: String,
    pub phone: String,
    pub name: Option<String>,
    pub client_id: Option<String>,
    pub status: String,
    pub assigned_to: Option<String>,
    pub last_message_at: String,
    pub unread_count: i32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MessageItem {
    pub id: String,
    pub wa_message_id: String,
    pub direction: String,
    pub msg_type: String,
    pub body: Option<String>,
    pub status: Option<String>,
    pub sent_by: Option<String>,
    pub timestamp: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationMessagesResponse {
    pub ok: bool,
    pub conversation: ConversationDetail,
    pub messages: Vec<MessageItem>,
    pub total: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationsListResponse {
    pub ok: bool,
    pub data: Vec<ConversationListItem>,
    pub total: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SendMessageResponse {
    pub ok: bool,
    pub message_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UpdateResponse {
    pub ok: bool,
}

// ============================================
// CONFIGURACIÓN DE NÚMEROS (wa_settings)
// ============================================

/// Documento en colección `wa_settings`
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaSettings {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Número en E.164 sin "+" (ej: "584222236777")
    pub phone: String,
    /// UUIDs de los agentes asignados a este número
    pub agents: Vec<String>,
    pub active: bool,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateSettingsRequest {
    /// Número en formato venezolano o E.164 (se normaliza automáticamente)
    pub phone: String,
    /// UUIDs de los agentes que atenderán este número
    pub agents: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateSettingsRequest {
    pub agents: Option<Vec<String>>,
    pub active: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SettingsItem {
    pub id: String,
    pub phone: String,
    pub agents: Vec<String>,
    pub active: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SettingsListResponse {
    pub ok: bool,
    pub data: Vec<SettingsItem>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SettingsResponse {
    pub ok: bool,
    pub data: SettingsItem,
}
