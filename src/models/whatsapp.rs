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
    /// Último mensaje entrante (del contacto). Se usa para calcular la ventana
    /// de 24h en la que Meta permite enviar mensajes freeform. `None` si la
    /// conversación nunca recibió un inbound (raro — se abre con uno).
    #[serde(default)]
    pub last_inbound_at: Option<DateTime>,
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
    /// MIME type reportado por Meta en el webhook (ej. "image/jpeg", "application/pdf").
    /// Útil para que el front decida cómo renderizar sin esperar la descarga.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_mime_type: Option<String>,
    /// Nombre original del archivo (solo documentos).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_filename: Option<String>,
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
    /// `wa_message_id` del mensaje al que responde (cita). `None` si no es respuesta.
    /// En outbound: lo setea el agente al enviar. En inbound: viene de Meta en
    /// `context.id` cuando el cliente cita un mensaje del negocio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_wa_message_id: Option<String>,
    /// Preview de URL (OG/Twitter Card). Se rellena async tras guardar el mensaje:
    /// el handler persiste el mensaje con `None`, dispara un job que fetchea la
    /// primera URL del cuerpo, y cuando termina hace `$set` aquí y emite
    /// `URL_PREVIEW_READY` por WS. `None` si el mensaje no tiene URL o el fetch
    /// no produjo un preview válido.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_preview: Option<UrlPreview>,
    /// `true` si es una nota de voz (push-to-talk). Poblado 100% desde
    /// `audio.voice` del webhook de Meta. Para `msg_type != "audio"` es `false`.
    #[serde(default)]
    pub voice: bool,
    /// Nombre del template (solo cuando `msg_type == "template"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_name: Option<String>,
    /// Código de idioma del template (ej: "es", "en_US").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_language: Option<String>,
    /// Snapshot de los `components` enviados a Meta (con `parameters`
    /// ya interpolados). Permite rerenderizar la burbuja en el futuro.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_components: Option<serde_json::Value>,
    pub timestamp: DateTime,
}

/// Preview de URL extraído server-side del cuerpo de un mensaje.
///
/// El fetch se hace desde el backend (no desde el browser del agente) para:
/// - Evitar CORS del servidor de destino.
/// - Cachear por URL (Redis, SHA-256 de la URL, TTL 24h).
/// - No filtrar la IP del agente al sitio de destino.
/// - Aplicar SSRF guard: no se permite resolver a IPs privadas / loopback.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct UrlPreview {
    /// URL canónica (después de seguir redirects; hasta 3 hops).
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// URL absoluta (og:image / twitter:image). El front la consume directo.
    pub image_url: Option<String>,
    /// `og:site_name` o, si falta, el hostname final.
    pub site_name: Option<String>,
    pub favicon_url: Option<String>,
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
    /// Cuando el usuario cita un mensaje, Meta incluye `context.id` con el
    /// `wamid` del mensaje original.
    pub context: Option<InboundContext>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InboundContext {
    pub id: String,
    #[allow(dead_code)]
    pub from: Option<String>,
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
    pub mime_type: Option<String>,
    pub filename: Option<String>,
    /// Sólo relevante en `audio`: `true` = nota de voz (push-to-talk),
    /// `false` = archivo de audio subido. Meta siempre lo incluye en audio.
    pub voice: Option<bool>,
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
    /// Discriminador: `"text"` (default) o `"template"`. Cuando viene
    /// `"template"` el backend ignora `content` y usa `template`.
    #[serde(default, rename = "type")]
    pub msg_type: Option<String>,
    /// Texto del mensaje a enviar. Requerido cuando `type == "text"`.
    #[serde(default)]
    pub content: Option<String>,
    /// Clave de idempotencia generada por el front (ej: UUID v4).
    /// Si se repite dentro de 24h, el backend devuelve el mensaje ya creado
    /// en vez de reenviarlo a Meta. Permite al front deduplicar contra el
    /// evento WS `MENSAJE_NUEVO`.
    pub idempotency_key: Option<String>,
    /// `wa_message_id` (wamid…) del mensaje al que se está respondiendo.
    /// Si está presente, Meta lo recibe como `context.message_id` y la
    /// burbuja sale citada en el chat del cliente.
    pub reply_to: Option<String>,
    /// Plantilla aprobada — obligatoria cuando `type == "template"`.
    pub template: Option<SendTemplatePayload>,
}

/// Plantilla lista para enviar. El front obtiene `name`/`language` desde
/// `GET /templates` y pasa los `components` con los parámetros ya
/// interpolados (según lo que indique Meta para cada template).
#[derive(Debug, Deserialize, Clone, ToSchema)]
pub struct SendTemplatePayload {
    pub name: String,
    /// Código de idioma tal cual lo expone Meta (ej: "es", "en_US").
    pub language: String,
    /// Componentes del template (header / body / buttons) con los
    /// `parameters` interpolados por el front. Se hace passthrough a Meta.
    #[schema(value_type = Vec<Object>)]
    #[serde(default)]
    pub components: Option<Vec<serde_json::Value>>,
    /// Texto ya renderizado que debe mostrarse en la burbuja del agente
    /// (el front calcula esto a partir del BODY del template + parámetros).
    /// Si no se envía, el backend usa un placeholder legible como fallback.
    #[serde(default)]
    pub rendered_text: Option<String>,
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
    /// ISO-8601 del último mensaje entrante. El front lo usa para mostrar la
    /// ventana de 24h y computar su propio countdown local. `null` si no hay
    /// inbounds registrados (caso borde — la conversación nace con uno).
    pub last_inbound_at: Option<String>,
    /// `true` si `now - last_inbound_at <= 24h`. Cuando es `false` Meta rechaza
    /// mensajes freeform y el front debe usar un template aprobado.
    pub can_send_freeform: bool,
    /// ISO-8601 de cuándo expira la ventana (`last_inbound_at + 24h`). `null`
    /// si no hay inbound previo. Ideal para countdown de UI.
    pub freeform_expires_at: Option<String>,
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
    /// MIME type del media (ej. "image/jpeg"). `null` cuando no aplica.
    /// El front lo usa como hint; la descarga real va por
    /// `GET /auth-user/whatsapp/media/:media_id`.
    pub media_mime_type: Option<String>,
    /// Nombre original del archivo (solo documentos).
    pub media_filename: Option<String>,
    /// `pending` (solo optimistic UI) | `sent` | `delivered` | `read` | `failed`.
    /// - En `direction="out"`: refleja el estado de entrega reportado por Meta.
    /// - En `direction="in"`: `read` indica que un agente ya lo vio en la UI
    ///   (marcado vía `POST /:id/mark-read`). Antes de eso, el campo es `null`.
    pub status: Option<String>,
    /// UUID del agente que envió el mensaje (solo cuando direction="out")
    pub from_user_id: Option<String>,
    /// Nombre del agente que envió el mensaje (best-effort).
    pub from_user_name: Option<String>,
    /// Clave de idempotencia provista por el front al enviar (eco en la respuesta).
    /// El front la usa para deduplicar contra el evento WS `MENSAJE_NUEVO`.
    pub idempotency_key: Option<String>,
    /// Mensaje citado (quoted reply). `null` si no es respuesta o si el
    /// mensaje original ya no existe en la DB.
    pub reply_to: Option<ReplyToItem>,
    /// Preview de URL (OG/Twitter Card). `null` mientras el job de fetch no
    /// haya terminado, si el mensaje no tenía URL, o si el fetch falló.
    /// Cuando llega, el front lo recibe también por WS (`URL_PREVIEW_READY`).
    pub url_preview: Option<UrlPreview>,
    /// `true` si es nota de voz (push-to-talk) reportada por Meta en el
    /// webhook (`audio.voice`). `false` en archivos de audio subidos y en
    /// cualquier mensaje que no sea de tipo `audio`.
    pub voice: bool,
    /// Nombre del template (solo cuando `type == "template"`). `null` si no.
    pub template_name: Option<String>,
    /// Código de idioma del template.
    pub template_language: Option<String>,
    /// `components` enviados a Meta (passthrough del payload original).
    /// El front los usa para renderizar la burbuja cuando quiere customizar.
    #[schema(value_type = Option<Object>)]
    pub template_components: Option<serde_json::Value>,
    /// ISO-8601 (RFC 3339) UTC
    pub created_at: String,
}

/// Resumen del mensaje citado al armar `MessageItem.reply_to`.
#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct ReplyToItem {
    pub wa_message_id: String,
    /// Primeros ~80 chars del contenido original (texto o caption). `null` si
    /// el original no tiene cuerpo (ej. imagen sin caption).
    pub preview_content: Option<String>,
    /// Tipo del mensaje original: "text" | "image" | "audio" | "video" |
    /// "document" | "sticker" | otros.
    pub preview_type: String,
    /// "in" | "out" — para que el front sepa de qué lado citar la burbuja.
    pub direction: String,
    /// Nombre del agente que envió el original (solo si era outbound).
    pub from_user_name: Option<String>,
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
    /// Mensajes ordenados del más reciente al más antiguo. Para el detalle de
    /// la conversación, usar `GET /conversations/:id`.
    pub data: Vec<MessageItem>,
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
    /// WhatsApp Business Account ID (WABA). Necesario para listar message templates.
    /// Puede venir vacío en docs viejos — se rellena por backfill al arrancar.
    #[serde(default)]
    pub whatsapp_business_account_id: String,
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
    /// WhatsApp Business Account ID (WABA). Requerido para poder listar templates.
    pub whatsapp_business_account_id: String,
    /// Access token permanente de Meta (se cifra antes de guardar)
    pub access_token: String,
    /// UUIDs de los agentes que atenderán este número
    pub agents: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateSettingsRequest {
    pub workspace_name: Option<String>,
    pub phone_number_id: Option<String>,
    pub whatsapp_business_account_id: Option<String>,
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
    /// Puede venir vacío si el doc es viejo y el backfill todavía no corrió.
    pub whatsapp_business_account_id: String,
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

// ============================================
// MENSAJES RÁPIDOS (WaQuickReplies)
// ============================================

/// Documento de la colección `WaQuickReplies`. Snippet de texto reutilizable
/// que un agente puede insertar rápidamente en el composer.
///
/// Scope: `workspace_ids` — lista de `WaSettings._id` donde este snippet está
/// disponible. Al listar, el filtro es "intersección con los workspaces del
/// agente que pregunta".
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaQuickReply {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub title: String,
    pub content: String,
    /// `_id` de los `WaSettings` en los que aplica este snippet.
    pub workspace_ids: Vec<ObjectId>,
    /// UUID del creador (`User._id`).
    pub created_by: String,
    /// Nombre del creador al momento de crear (snapshot — no se actualiza si el user cambia de nombre).
    pub created_by_name: String,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct QuickReplyItem {
    pub id: String,
    pub title: String,
    pub content: String,
    /// Hex de `WaSettings._id` donde aplica el snippet.
    pub workspace_ids: Vec<String>,
    pub created_by: String,
    pub created_by_name: String,
    /// ISO-8601 (RFC 3339) UTC
    pub created_at: String,
    /// ISO-8601 (RFC 3339) UTC
    pub updated_at: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateQuickReplyRequest {
    /// 1–100 chars.
    pub title: String,
    /// 1–1024 chars (límite de WhatsApp para texto libre).
    pub content: String,
    /// Hex de `WaSettings._id`. Mínimo 1. El usuario debe ser agente en todos ellos.
    pub workspace_ids: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateQuickReplyRequest {
    pub title: Option<String>,
    pub content: Option<String>,
    pub workspace_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DuplicateQuickReplyRequest {
    /// Si viene, sobreescribe el título de la copia. Por defecto `<original> (copia)`.
    pub title: Option<String>,
    /// Si viene, usa estos workspaces en vez de los del original. Debe validarse igual que en `create`.
    pub workspace_ids: Option<Vec<String>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct QuickRepliesListResponse {
    pub ok: bool,
    pub data: Vec<QuickReplyItem>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct QuickReplyResponse {
    pub ok: bool,
    pub data: QuickReplyItem,
}

// ============================================
// TEMPLATES DE META
// ============================================

/// Plantilla aprobada por Meta. Se sirve tal cual viene del endpoint de Meta,
/// filtrando sólo `status: "APPROVED"`.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct WhatsAppTemplate {
    pub name: String,
    pub language: String,
    /// "UTILITY" | "MARKETING" | "AUTHENTICATION" | otros (lo que Meta devuelva).
    pub category: String,
    /// Siempre "APPROVED" en el response (filtramos los demás).
    pub status: String,
    /// Estructura de Meta: array con items `{ type, format?, text?, buttons?, ... }`.
    /// Se pasa tal cual — el front conoce el shape (ver spec del endpoint).
    pub components: Vec<serde_json::Value>,
    /// Cantidad de placeholders `{{n}}` detectados en el `text` del componente
    /// `BODY` (N distintos). 0 si no hay BODY o no tiene placeholders.
    pub body_placeholders: u32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TemplatesListResponse {
    pub ok: bool,
    pub data: Vec<WhatsAppTemplate>,
}
