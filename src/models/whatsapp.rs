use mongodb::bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================
// DOCUMENTOS DE BASE DE DATOS
// ============================================

/// Conversación de WhatsApp (colección `wa_conversations`)
///
/// Un chat queda identificado de forma única por el par
/// `(phone, business_phone)`: el mismo contacto escribiendo a dos números
/// de negocio distintos genera dos conversaciones separadas.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaConversation {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Número del contacto en E.164 sin "+" (ej: "584141234567")
    pub phone: String,
    /// Número de negocio (Meta) que recibió el mensaje, en E.164 sin "+"
    pub business_phone: String,
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

/// Registro "conversación abierta por agente X en fecha Y" (colección `WaConversationOpens`).
/// Se upserta en el primer `GET /messages` de cada agente; es per-user, por conversación.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaConversationOpen {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub user_id: String,
    pub conversation_id: ObjectId,
    pub last_opened_at: DateTime,
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
    #[serde(default)]
    pub body: Option<String>,
    /// ID de media en WhatsApp (para imágenes/documentos)
    #[serde(default)]
    pub media_id: Option<String>,
    /// Solo para outbound: "sent" | "delivered" | "read" | "failed"
    #[serde(default)]
    pub status: Option<String>,
    /// UUID del agente que envió (solo outbound)
    #[serde(default)]
    pub sent_by: Option<String>,
    /// Clave de idempotencia con la que el front disparó el envío. Usada para
    /// asociar respuesta HTTP con evento WS y deduplicar en la UI.
    #[serde(default)]
    pub idempotency_key: Option<String>,
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
pub struct WebhookMetadata {
    pub display_phone_number: Option<String>,
    pub phone_number_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookValue {
    pub metadata: Option<WebhookMetadata>,
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
    pub timestamp: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub text: Option<InboundText>,
    pub image: Option<InboundMedia>,
    pub document: Option<InboundMedia>,
    pub audio: Option<InboundMedia>,
    pub video: Option<InboundMedia>,
    pub sticker: Option<InboundMedia>,
    pub location: Option<InboundLocation>,
    pub contacts: Option<serde_json::Value>,
    pub interactive: Option<serde_json::Value>,
    pub button: Option<serde_json::Value>,
    pub reaction: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InboundLocation {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub name: Option<String>,
    pub address: Option<String>,
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
    pub timestamp: Option<String>,
    #[allow(dead_code)]
    pub recipient_id: Option<String>,
    pub errors: Option<Vec<StatusError>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusError {
    pub code: Option<i64>,
    pub title: Option<String>,
    pub message: Option<String>,
    #[serde(rename = "error_data")]
    pub error_data: Option<serde_json::Value>,
}

// ============================================
// MODELOS HTTP (Request / Response)
// ============================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    /// Texto del mensaje a enviar
    pub content: String,
    /// Clave de idempotencia generada por el front (ej: UUID v4).
    /// Si se repite dentro de 24h, el backend devuelve el mensaje ya creado
    /// en vez de reenviarlo a Meta. Permite al front deduplicar contra el
    /// evento WS `MENSAJE_NUEVO`.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct TransferConversationRequest {
    /// UUID del agente destino. Acepta cualquier staff/admin
    /// (aun si no está en `wa_settings.agents` — es override puntual).
    pub user_id: String,
    /// Nota opcional que acompaña la transferencia.
    pub note: Option<String>,
}

/// Response estándar para conversaciones en listados y detalle.
#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct ConversationItem {
    pub id: String,
    /// Número del contacto (quien escribe) en E.164 sin "+"
    pub customer_phone: String,
    pub customer_name: Option<String>,
    /// Número de negocio (WA) que recibió el mensaje, en E.164 sin "+"
    pub business_phone: String,
    /// Nombre legible del workspace (Meta Business) correspondiente al `business_phone`.
    /// `null` si no hay `WaSettings` configurado para ese número.
    pub workspace_name: Option<String>,
    /// "pending" | "in_progress" | "closed"
    pub status: String,
    pub assigned_to: Option<String>,
    /// ISO-8601 (RFC 3339) UTC, ej: "2026-04-21T14:32:10.123Z"
    pub last_message_at: String,
    pub last_message_preview: Option<String>,
    pub unread_count: i32,
    /// ISO-8601 (RFC 3339) UTC
    pub created_at: String,
    /// Cliente ISP vinculado (si aplica). Solo se rellena en el detalle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Fecha (ISO-8601) en que el agente actual abrió este chat por última vez.
    /// `null` si nunca lo abrió.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_opened_at: Option<String>,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct MessageItem {
    pub id: String,
    pub conversation_id: String,
    pub wa_message_id: String,
    /// "in" | "out"
    pub direction: String,
    /// "text" | "image" | "audio" | "video" | "document" | "sticker" | otros
    #[serde(rename = "type")]
    pub msg_type: String,
    pub content: Option<String>,
    pub media_id: Option<String>,
    /// `pending` (solo optimistic UI) | `sent` | `delivered` | `read` | `failed`.
    /// - En `direction="out"`: refleja el estado de entrega reportado por Meta.
    /// - En `direction="in"`: `read` indica que un agente ya lo vio en la UI
    ///   (marcado vía `POST /:id/mark-read`). Antes de eso, el campo es `null`.
    pub status: Option<String>,
    /// UUID del agente que envió el mensaje (solo cuando direction="out")
    pub sent_by: Option<String>,
    /// Nombre del agente que envió el mensaje (best-effort).
    pub sent_by_name: Option<String>,
    /// Clave de idempotencia provista por el front al enviar (eco en la respuesta).
    /// El front la usa para deduplicar contra el evento WS `MENSAJE_NUEVO`.
    pub idempotency_key: Option<String>,
    /// ISO-8601 (RFC 3339) UTC
    pub created_at: String,
}

/// Respuesta paginable con cursor: el front envía `next_cursor` de nuevo para la siguiente página.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationsListResponse {
    pub ok: bool,
    pub data: Vec<ConversationItem>,
    /// Cursor opaco para la siguiente página. `null` cuando no hay más.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationDetailResponse {
    pub ok: bool,
    pub conversation: ConversationItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationMessagesResponse {
    pub ok: bool,
    pub conversation: ConversationItem,
    /// Mensajes ordenados del más reciente al más antiguo.
    pub messages: Vec<MessageItem>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SendMessageResponse {
    pub ok: bool,
    /// Atajo: `_id` del mensaje en la colección (Mongo ObjectId hex). Igual a `message.id`.
    pub message_id: String,
    pub message: MessageItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MarkReadResponse {
    pub ok: bool,
    /// Lista de `wa_message_id` que pasaron a `read` en esta llamada.
    /// Vacía si no había inbound sin leer.
    pub message_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TakeConversationResponse {
    pub ok: bool,
    pub conversation: ConversationItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UpdateResponse {
    pub ok: bool,
}

// ============================================
// CONFIGURACIÓN DE NÚMEROS (wa_settings)
// ============================================

/// Documento en colección `WaSettings`.
///
/// El `access_token` se guarda **cifrado en reposo** (AES-GCM con
/// `WHATSAPP_SETTINGS_SECRET`). Nunca se devuelve al front — sólo se
/// descifra in-memory para hablar con la API de Meta.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaSettings {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Número en E.164 sin "+" (ej: "584222236777")
    pub phone: String,
    /// Nombre legible del Meta Business / workspace
    #[serde(default)]
    pub workspace_name: String,
    /// Phone Number ID de WhatsApp Cloud API
    #[serde(default)]
    pub phone_number_id: String,
    /// Access token permanente de Meta (AES-GCM ciphertext Base64URL). Nunca se expone al front.
    #[serde(default)]
    pub access_token: String,
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
    /// Nombre legible del workspace / Meta Business
    pub workspace_name: String,
    /// Phone Number ID de WhatsApp Cloud API
    pub phone_number_id: String,
    /// Access token permanente de Meta (se cifra antes de guardar)
    pub access_token: String,
    /// UUIDs de los agentes que atenderán este número
    pub agents: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateSettingsRequest {
    pub workspace_name: Option<String>,
    pub phone_number_id: Option<String>,
    /// Si viene vacío o ausente, **no** se toca el token guardado.
    pub access_token: Option<String>,
    pub agents: Option<Vec<String>>,
    pub active: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SettingsItem {
    pub id: String,
    pub phone: String,
    pub workspace_name: String,
    pub phone_number_id: String,
    /// `true` si hay un token guardado (cifrado). **Nunca** se devuelve el token en claro.
    pub has_access_token: bool,
    pub agents: Vec<String>,
    pub active: bool,
    /// ISO-8601 (RFC 3339) UTC
    pub created_at: String,
    /// ISO-8601 (RFC 3339) UTC
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

// ============================================
// AGENTES TRANSFERIBLES
// ============================================

#[derive(Debug, Serialize, ToSchema)]
pub struct TransferableAgentItem {
    pub id: String,
    pub name: String,
    pub email: String,
    pub role: f32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TransferableAgentsResponse {
    pub ok: bool,
    pub data: Vec<TransferableAgentItem>,
}
