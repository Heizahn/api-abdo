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
    /// "pending" | "in_progress" | "closed"
    pub status: String,
    /// UUID del agente asignado
    pub assigned_to: Option<String>,
    pub last_message_at: DateTime,
    pub last_message_preview: Option<String>,
    /// Tipo del último mensaje (ej: "text", "image", "audio", "video",
    /// "document", "sticker", "location", "template", "interactive", "button").
    /// Se desnormaliza aquí para que el listado pueda renderizar previews
    /// estilo WhatsApp (icono + texto) sin tener que hacer un join contra
    /// `WaMessages`. `None` en docs viejos.
    #[serde(default)]
    pub last_message_type: Option<String>,
    /// Dirección del último mensaje: `"in"` (del contacto) o `"out"` (del agente).
    #[serde(default)]
    pub last_message_direction: Option<String>,
    /// Estado del último mensaje outbound (`"sent" | "delivered" | "read" | "failed"`).
    /// Sólo es significativo cuando `last_message_direction == "out"`. El front
    /// pinta los ✓ / ✓✓ / ✓✓ azul con este campo.
    #[serde(default)]
    pub last_message_status: Option<String>,
    /// Nombre de archivo del último mensaje si era un documento. Null en otros casos.
    #[serde(default)]
    pub last_message_media_filename: Option<String>,
    /// UUID del agente que envió el último mensaje (sólo para outbound).
    /// El handler resuelve el nombre a demanda.
    #[serde(default)]
    pub last_message_from_user_id: Option<String>,
    /// `wa_message_id` del último mensaje de la conversación. Se usa para saber
    /// si un status-update del webhook corresponde al último mensaje y, de ser
    /// así, propagar el nuevo status a `last_message_status`.
    #[serde(default)]
    pub last_message_wa_id: Option<String>,
    pub unread_count: i32,
    pub created_at: DateTime,
    /// Último mensaje entrante (del contacto). Se usa para calcular la ventana
    /// de 24h en la que Meta permite enviar mensajes freeform. `None` si la
    /// conversación nunca recibió un inbound (raro — se abre con uno).
    #[serde(default)]
    pub last_inbound_at: Option<DateTime>,
    /// Cooldown impuesto cuando Meta rechaza un envío con error
    /// `131049 — engagement throttle`. Mientras `now < meta_throttle_until`,
    /// el backend bloquea cualquier envío (texto o template) hacia esta
    /// conversación devolviendo `template_throttled_by_meta`. Se limpia
    /// automáticamente cuando llega un inbound (el cliente respondió).
    #[serde(default)]
    pub meta_throttle_until: Option<DateTime>,
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

/// Evento de ciclo de vida de una conversación (colección `WaConversationEvents`).
///
/// Cada vez que un agente toma, transfiere, cierra o reabre una conversación
/// se persiste un documento con la acción + actor + target + nota. Sirve para:
/// - Reconstruir el timeline auditable de la conversación.
/// - Métricas históricas (quién atendió qué, cuántos transfers, etc.).
///
/// `business_phone` se desnormaliza al insertar para que el dashboard de
/// auditoría pueda filtrar por número de negocio sin un `$lookup`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaConversationEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub conversation_id: ObjectId,
    pub business_phone: String,
    /// "created" | "taken" | "transferred" | "closed" | "reopened"
    pub event_type: String,
    /// UUID del agente que ejecutó la acción. `None` cuando el evento lo
    /// genera el sistema (p.ej. `created` por webhook entrante o seed de backfill).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_name: Option<String>,
    /// UUID del agente destino en `transferred`, o del nuevo dueño en `taken`
    /// cuando difiere del actor (caso staff que toma un chat ya asignado).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    /// Nota libre del agente al ejecutar la acción (p.ej. motivo del transfer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: DateTime,
}

/// Input para insertar un evento de ciclo de vida. Mantiene el shape
/// independiente del documento Mongo (sin `id`, `created_at` lo pone el repo).
#[derive(Debug, Clone)]
pub struct WaConversationEventInput<'a> {
    pub conversation_id: &'a ObjectId,
    pub business_phone: &'a str,
    pub event_type: &'a str,
    pub actor_id: Option<&'a str>,
    pub actor_name: Option<&'a str>,
    pub target_id: Option<&'a str>,
    pub target_name: Option<&'a str>,
    pub note: Option<&'a str>,
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
    /// Snapshot del payload `interactive` enviado a Meta (sólo cuando
    /// `msg_type == "interactive"`). Incluye action/buttons/list/etc para que
    /// el front pueda rerenderizar la burbuja.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_payload: Option<serde_json::Value>,
    /// Payload de contactos compartidos cuando `msg_type == "contacts"`.
    /// Passthrough del array que envía Meta: cada item tiene `name`,
    /// `phones`, `emails`, `addresses`, `org`, `birthday`, `urls`.
    /// El front lo renderiza como tarjeta tipo vCard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contacts_payload: Option<serde_json::Value>,
    /// Coordenadas y metadata cuando `msg_type == "location"`. El front usa
    /// `latitude`/`longitude` para renderizar el mapa (iframe de OSM, Google
    /// Embed, imagen estática, link a maps, etc) y muestra `name`/`address`
    /// como caption si vienen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationPayload>,
    pub timestamp: DateTime,
}

/// Ubicación compartida vía WhatsApp. `latitude`/`longitude` son siempre no
/// nulos en inbounds válidos; `name`/`address` sólo vienen si el cliente
/// usó "Lugares cercanos" o compartió una dirección con nombre.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct LocationPayload {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
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
    /// Presente cuando `WebhookChange.field == "message_template_status_update"`.
    /// Meta emite este shape al WABA cuando un template cambia de estado
    /// (review completado, flagged, paused, etc.).
    pub event: Option<String>,
    /// Meta envía `message_template_id` como **integer** en webhooks de
    /// template-status (¡aunque en el endpoint REST de templates lo devuelve
    /// como string!). Aceptamos ambos formatos y normalizamos a string —
    /// internamente comparamos contra `WaTemplate.meta_template_id` que es
    /// String.
    #[serde(default, deserialize_with = "deserialize_id_as_string")]
    pub message_template_id: Option<String>,
    pub message_template_name: Option<String>,
    pub message_template_language: Option<String>,
    pub reason: Option<String>,
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
    /// Discriminador: `"text"` (default), `"template"`, `"interactive"`,
    /// `"image"`, `"video"`, `"document"`, `"audio"`, `"sticker"`,
    /// `"location"`, `"contacts"`. Según el valor se usa el sub-objeto
    /// correspondiente y los demás se ignoran.
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
    /// Si es `true` y el texto contiene una URL, Meta fetchea las OG tags del
    /// sitio y renderiza la tarjeta de preview en el chat del cliente. Sólo
    /// aplica a `type == "text"`. Default `false`.
    #[serde(default)]
    pub preview_url: Option<bool>,
    /// Payload de mensaje interactivo (button / list / cta_url) — pasa-piso
    /// directo al objeto `interactive` de la Cloud API de Meta. Requerido
    /// cuando `type == "interactive"`.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub interactive: Option<serde_json::Value>,
    /// Si el mensaje interactivo proviene de un quick-reply guardado, pasar
    /// el `id` aquí para que el backend incremente `use_count` y setee
    /// `last_used_at`. Opcional.
    #[serde(default)]
    pub quick_reply_id: Option<String>,
    /// Imagen a enviar (requerido cuando `type == "image"`). El `media_id` se
    /// obtiene primero vía `POST /whatsapp/media`. `caption` es opcional.
    #[serde(default)]
    pub image: Option<MediaSendPayload>,
    /// Video a enviar (requerido cuando `type == "video"`). Mismo flujo que `image`.
    #[serde(default)]
    pub video: Option<MediaSendPayload>,
    /// Documento a enviar (requerido cuando `type == "document"`). `filename`
    /// define cómo se muestra en el chat del cliente; si se omite, Meta usa
    /// el nombre original subido.
    #[serde(default)]
    pub document: Option<MediaSendPayload>,
    /// Audio a enviar (requerido cuando `type == "audio"`). Meta **no** acepta
    /// `caption` en audio — si viene, se ignora.
    #[serde(default)]
    pub audio: Option<MediaSendPayload>,
    /// Sticker a enviar (requerido cuando `type == "sticker"`). Meta sólo
    /// acepta `image/webp` animado o estático.
    #[serde(default)]
    pub sticker: Option<MediaSendPayload>,
    /// Ubicación a enviar (requerido cuando `type == "location"`).
    #[serde(default)]
    pub location: Option<LocationPayload>,
    /// Tarjetas de contacto a enviar (requerido cuando `type == "contacts"`).
    /// Passthrough directo al array que espera Meta — el backend sólo valida
    /// que sea no-vacío y que cada contacto tenga `name.formatted_name`.
    #[serde(default)]
    #[schema(value_type = Option<Vec<Object>>)]
    pub contacts: Option<Vec<serde_json::Value>>,
}

/// Payload compartido por image/video/document/audio/sticker en
/// `SendMessageRequest`. El handler interpreta los campos según el tipo:
///
/// - `image`/`video`: usa `media_id` + `caption?`
/// - `document`:      usa `media_id` + `caption?` + `filename?`
/// - `audio`/`sticker`: usa sólo `media_id` (los demás se ignoran)
#[derive(Debug, Deserialize, Clone, ToSchema)]
pub struct MediaSendPayload {
    /// ID devuelto por `POST /v1/auth-user/whatsapp/media` (ID de Meta).
    pub media_id: String,
    /// Caption opcional (sólo aplica a image/video/document).
    #[serde(default)]
    pub caption: Option<String>,
    /// Nombre de archivo que verá el cliente (sólo aplica a document).
    #[serde(default)]
    pub filename: Option<String>,
}

/// Datos del media recién subido a Meta.
#[derive(Debug, Serialize, ToSchema)]
pub struct MediaUploadData {
    /// ID de Meta para el media recién subido. TTL ~30 días del lado de Meta.
    pub media_id: String,
    /// MIME type canónico detectado (del header `Content-Type` multipart).
    pub mime_type: String,
    /// Tamaño en bytes del archivo subido.
    pub size: u64,
    /// SHA-256 hex del binario. Calculado en backend; sirve al front para
    /// deduplicar reenvíos idénticos en la UI.
    pub sha256: String,
}

/// Response de `POST /v1/auth-user/whatsapp/media`. El `media_id` se usa en
/// el `POST /conversations/:id/messages` subsiguiente.
#[derive(Debug, Serialize, ToSchema)]
pub struct MediaUploadResponse {
    pub ok: bool,
    pub data: MediaUploadData,
}

/// Límite por tipo de media — devuelto en `GET /whatsapp/media/limits`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MediaTypeLimit {
    /// Tamaño máximo aceptado por el backend (bytes).
    pub max_bytes: u64,
    /// MIME types permitidos por Meta para ese tipo.
    pub mime_types: Vec<String>,
}

/// Response de `GET /v1/auth-user/whatsapp/media/limits`. El front lo cachea
/// y lo usa para validar client-side antes de llamar al upload.
#[derive(Debug, Serialize, ToSchema)]
pub struct MediaLimitsResponse {
    pub ok: bool,
    pub image: MediaTypeLimit,
    pub video: MediaTypeLimit,
    pub audio: MediaTypeLimit,
    pub document: MediaTypeLimit,
    pub sticker: MediaTypeLimit,
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

/// Iniciar una conversación desde el agente (sin esperar mensaje inbound).
/// Siempre envía un template aprobado por Meta — al no haber inbounds
/// previos, la ventana de 24h está cerrada por definición.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InitiateConversationRequest {
    /// Hex de `WaSettings._id` desde donde sale el mensaje (workspace emisor).
    pub business_phone_id: String,
    /// Teléfono del destinatario. Cualquier formato VE es aceptado; el
    /// backend normaliza a E.164 sin "+" (ej: "584141234567").
    pub to: String,
    /// Template aprobado con los parámetros ya interpolados.
    pub template: SendTemplatePayload,
    /// Clave de idempotencia: evita enviar duplicados si el cliente reintenta.
    pub idempotency_key: String,
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
    /// Tipo del último mensaje — el front lo usa para renderizar previews
    /// estilo WhatsApp (📷 Foto, 🎤 Nota de voz, 📄 Documento, ✨ Interactivo…).
    /// Valores posibles: "text", "image", "audio", "video", "document",
    /// "sticker", "location", "template", "interactive", "button".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message_type: Option<String>,
    /// `"in"` (del contacto) o `"out"` (del agente). Permite al front prefijar
    /// "Tú: …" cuando es outbound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message_direction: Option<String>,
    /// Estado del último mensaje outbound. Solo significativo cuando
    /// `last_message_direction == "out"`. Valores: `"sent" | "delivered" | "read" | "failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message_status: Option<String>,
    /// Nombre de archivo (sólo cuando `last_message_type == "document"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message_media_filename: Option<String>,
    /// UUID del agente que envió el último mensaje (sólo outbound).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message_from_user_id: Option<String>,
    /// Nombre del agente que envió el último mensaje (best-effort, resuelto en
    /// el handler). Útil cuando hay varios agentes en el mismo workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message_from_user_name: Option<String>,
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
    /// `true` si Meta está rate-limitando esta conversación con error 131049
    /// (engagement throttle): demasiados templates al mismo destinatario sin
    /// respuesta. Mientras sea `true` el backend rechaza cualquier envío con
    /// el error `template_throttled_by_meta` (HTTP 409). Se libera al recibir
    /// un inbound del cliente o al expirar `meta_throttle_until`.
    pub meta_throttled: bool,
    /// ISO-8601 hasta cuándo dura el cooldown de Meta (`131049`). `null` si la
    /// conversación no está throttle-eada. Útil para el countdown de UI.
    pub meta_throttle_until: Option<String>,
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
    /// Payload `interactive` enviado a Meta (sólo cuando `type == "interactive"`).
    /// Passthrough del mismo objeto que se le pasó a la Cloud API.
    #[schema(value_type = Option<Object>)]
    pub interactive_payload: Option<serde_json::Value>,
    /// Array de contactos compartidos (sólo cuando `type == "contacts"`).
    /// Passthrough del payload de Meta: cada contacto tiene `name`, `phones`,
    /// `emails`, `addresses`, `org`, `birthday`, `urls`. El front lo renderiza
    /// como tarjeta tipo vCard.
    #[schema(value_type = Option<Object>)]
    pub contacts_payload: Option<serde_json::Value>,
    /// Datos estructurados de ubicación (sólo cuando `type == "location"`).
    /// El front renderiza el mapa con `latitude`/`longitude`.
    pub location: Option<LocationPayload>,
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
    pub data: ConversationItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationMessagesResponse {
    pub ok: bool,
    /// Mensajes ordenados del más reciente al más antiguo. Para el detalle de
    /// la conversación, usar `GET /conversations/:id`.
    pub data: Vec<MessageItem>,
    pub next_cursor: Option<String>,
}

/// Payload interno de `SendMessageResponse.data`. Se extrae a struct propio
/// para mantener `{ ok, data }` uniforme con el resto de endpoints.
#[derive(Debug, Serialize, ToSchema)]
pub struct SendMessageData {
    /// Atajo: `_id` del mensaje en la colección (Mongo ObjectId hex). Igual a `message.id`.
    pub message_id: String,
    pub message: MessageItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SendMessageResponse {
    pub ok: bool,
    pub data: SendMessageData,
}

/// Payload interno de `MarkReadResponse.data`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MarkReadData {
    /// Lista de `wa_message_id` que pasaron a `read` en esta llamada.
    /// Vacía si no había inbound sin leer.
    pub message_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MarkReadResponse {
    pub ok: bool,
    pub data: MarkReadData,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TakeConversationResponse {
    pub ok: bool,
    pub data: ConversationItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UpdateResponse {
    pub ok: bool,
}

/// Contadores por categoría — independientes del filtro activo en la UI.
/// `total === pending + in_progress + closed` (invariante).
/// `mine` es ortogonal al estado, no se suma con los otros.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationStats {
    pub total: u64,
    pub mine: u64,
    pub pending: u64,
    pub in_progress: u64,
    pub closed: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConversationStatsResponse {
    pub ok: bool,
    pub data: ConversationStats,
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
    /// Propósitos configurados (OTP, notificaciones, recordatorios de pago).
    /// Cada clave es opcional — un número puede tener uno, varios o ninguno.
    /// Los docs viejos llegan con `WaPurposes::default()` (todos `None`).
    #[serde(default)]
    pub purposes: WaPurposes,
    /// Timestamp del último backfill de templates desde Meta. `None` mientras
    /// no se haya sincronizado nunca; cuando es `Some` y la diferencia con
    /// `now` es < 24h, el GET de templates lee directo de DB sin tocar Meta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub templates_synced_at: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

/// Configuración de un template aprobado en Meta que se usará para un propósito dado.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct WaPurposeConfig {
    /// Nombre del template tal como está registrado y aprobado en Meta
    pub template_name: String,
    /// Código de idioma del template (ej: "es", "en_US")
    pub language: String,
}

/// Propósitos disponibles para un número de WhatsApp. Todos opcionales —
/// un número puede declarar uno, varios o ninguno. Cuando llega un evento
/// (OTP, notificación, recordatorio), el módulo correspondiente busca un
/// `WaSettings` activo con el propósito configurado.
#[derive(Debug, Serialize, Deserialize, Clone, Default, ToSchema)]
pub struct WaPurposes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otp: Option<WaPurposeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<WaPurposeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment_reminder: Option<WaPurposeConfig>,
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
    /// Propósitos configurados. Si se omite, el número no se usará para ningún template.
    #[serde(default)]
    pub purposes: Option<WaPurposes>,
}

/// PATCH-style body. Para `purposes`, usar el sub-patch `WaPurposesPatch`:
/// cada propósito acepta tri-state (`undefined` = no tocar, `null` = limpiar,
/// objeto = setear).
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateSettingsRequest {
    pub workspace_name: Option<String>,
    pub phone_number_id: Option<String>,
    pub whatsapp_business_account_id: Option<String>,
    /// Si viene vacío o ausente, **no** se toca el token guardado.
    pub access_token: Option<String>,
    pub agents: Option<Vec<String>>,
    pub active: Option<bool>,
    #[serde(default)]
    pub purposes: Option<WaPurposesPatch>,
}

/// Patch per-purpose. Cada campo es tri-state:
/// - ausente (`None`) → no tocar
/// - `null` (`Some(None)`) → limpiar ese propósito
/// - objeto (`Some(Some(cfg))`) → setear
#[derive(Debug, Deserialize, ToSchema)]
pub struct WaPurposesPatch {
    #[serde(default, deserialize_with = "deserialize_some_opt")]
    pub otp: Option<Option<WaPurposeConfig>>,
    #[serde(default, deserialize_with = "deserialize_some_opt")]
    pub notifications: Option<Option<WaPurposeConfig>>,
    #[serde(default, deserialize_with = "deserialize_some_opt")]
    pub payment_reminder: Option<Option<WaPurposeConfig>>,
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
    /// Propósitos configurados (OTP, notificaciones, recordatorios).
    pub purposes: WaPurposes,
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

/// Header opcional de un quick-reply (variante discriminada por `type`).
/// Para media (image/video/document) `link` debe ser URL pública https — Meta
/// hace fetch del recurso al renderizar.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum QuickReplyHeader {
    Text { text: String },
    Image { link: String },
    Video { link: String },
    Document { link: String, #[serde(default, skip_serializing_if = "Option::is_none")] filename: Option<String> },
}

/// Un botón de "reply button" (respuesta rápida). Máx 1..3 por mensaje.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct QuickReplyButton {
    /// ID único dentro del array (≤ 256 chars). Meta lo devuelve cuando el
    /// usuario aprieta el botón, y el front lo usa para identificar la opción.
    pub id: String,
    /// Label visible en el botón (≤ 20 chars).
    pub title: String,
}

/// Fila dentro de una sección de una lista interactiva.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct QuickReplyListRow {
    /// ID único en toda la lista (no sólo dentro de la sección).
    pub id: String,
    /// Título visible (≤ 24 chars).
    pub title: String,
    /// Descripción secundaria opcional (≤ 72 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Sección dentro de una lista interactiva. Cada sección agrupa filas.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct QuickReplyListSection {
    /// Título de la sección (≤ 24 chars).
    pub title: String,
    pub rows: Vec<QuickReplyListRow>,
}

/// Lista interactiva: un botón que abre un bottom-sheet con secciones y filas.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct QuickReplyList {
    /// Texto del botón que abre la lista (≤ 20 chars).
    pub button: String,
    pub sections: Vec<QuickReplyListSection>,
}

/// Botón URL (call-to-action). Excluyente con `buttons` y `list`.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct QuickReplyCtaUrl {
    /// Label visible del botón (≤ 20 chars).
    pub display_text: String,
    /// URL destino (http o https; se recomienda https).
    pub url: String,
}

/// Documento de la colección `WaQuickReplies`. Snippet de texto (opcionalmente
/// interactivo) reutilizable que un agente puede insertar en el composer.
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
    /// Activar/desactivar sin borrar el doc. Default `true`.
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<QuickReplyHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
    /// Reply buttons (1..3). Excluyente con `list` y `cta_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buttons: Option<Vec<QuickReplyButton>>,
    /// Lista interactiva. Excluyente con `buttons` y `cta_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<QuickReplyList>,
    /// Botón URL. Excluyente con `buttons` y `list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cta_url: Option<QuickReplyCtaUrl>,
    /// Contador de envíos — para ordenar por popularidad en el front.
    #[serde(default)]
    pub use_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime>,
}

fn default_true() -> bool { true }

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct QuickReplyItem {
    pub id: String,
    pub title: String,
    pub content: String,
    /// Hex de `WaSettings._id` donde aplica el snippet. Array vacío = global
    /// (aplica a todos los workspaces).
    pub workspace_ids: Vec<String>,
    pub created_by: String,
    pub created_by_name: String,
    /// ISO-8601 (RFC 3339) UTC
    pub created_at: String,
    /// ISO-8601 (RFC 3339) UTC
    pub updated_at: String,
    pub active: bool,
    /// `true` si el caller puede **eliminar** este item. Cualquier `can_chat`
    /// puede ver/usar/editar/toggle cualquier quick reply — el delete exige
    /// un gate extra: caller es superadmin (`nRole=0`) o es agente de al
    /// menos uno de los workspaces del item (overlap con `agents[]` del
    /// `WaSettings`). El front usa esta bandera para deshabilitar el botón
    /// de eliminar en las cards donde no aplica; el server valida igual al
    /// intentar el delete (403 si no cumple).
    pub can_edit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<QuickReplyHeader>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buttons: Option<Vec<QuickReplyButton>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<QuickReplyList>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cta_url: Option<QuickReplyCtaUrl>,
    pub use_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateQuickReplyRequest {
    /// 1–100 chars.
    pub title: String,
    /// 1–1024 chars (límite de WhatsApp para texto libre).
    pub content: String,
    /// Hex de `WaSettings._id`. Mínimo 1 — no existen quick replies globales.
    /// Crear exige que el caller sea agente en **todos** estos workspaces
    /// (o superadmin).
    pub workspace_ids: Vec<String>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub header: Option<QuickReplyHeader>,
    #[serde(default)]
    pub footer: Option<String>,
    #[serde(default)]
    pub buttons: Option<Vec<QuickReplyButton>>,
    #[serde(default)]
    pub list: Option<QuickReplyList>,
    #[serde(default)]
    pub cta_url: Option<QuickReplyCtaUrl>,
}

/// PATCH-style body con semántica `null = limpiar`, `undefined = no tocar`.
/// Los campos `Option<Option<T>>` usan `deserialize_some_opt` para distinguir
/// el campo ausente (None) de un `null` explícito (Some(None)).
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateQuickReplyRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub workspace_ids: Option<Vec<String>>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_some_opt")]
    pub header: Option<Option<QuickReplyHeader>>,
    #[serde(default, deserialize_with = "deserialize_some_opt")]
    pub footer: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some_opt")]
    pub buttons: Option<Option<Vec<QuickReplyButton>>>,
    #[serde(default, deserialize_with = "deserialize_some_opt")]
    pub list: Option<Option<QuickReplyList>>,
    #[serde(default, deserialize_with = "deserialize_some_opt")]
    pub cta_url: Option<Option<QuickReplyCtaUrl>>,
}

/// Helper de serde: distingue "campo ausente" de "campo presente con null".
/// Devuelve `Some(None)` cuando el campo viene como `null`, `Some(Some(v))`
/// cuando trae valor, y se combina con `#[serde(default)]` para quedar en
/// `None` si el campo no aparece en el JSON.
fn deserialize_some_opt<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Helper de serde: acepta tanto string como integer y normaliza a `Option<String>`.
/// Existe porque Meta envía algunos IDs (como `message_template_id` en webhooks
/// de status update) como **integer**, mientras que en otros endpoints los
/// devuelve como string. Sin esto, la deserialización del webhook entero falla
/// y dropeamos eventos críticos como APPROVED/REJECTED.
fn deserialize_id_as_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(opt.and_then(|v| match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Null => None,
        _ => None,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ToggleActiveRequest {
    pub active: bool,
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

// ============================================
// CRUD DE TEMPLATES (WaTemplates)
// ============================================
//
// Source of truth híbrido: la colección `WaTemplates` guarda metadatos
// custom (display_name, is_system, created_by, submit_to_meta, etc.); Meta
// es dueña de `name`, `language`, `components`. El webhook
// `message_template_status_update` sincroniza `status` y `rejection_reason`.
//
// Ver `docs/wa-templates-api-spec.md` para el contrato completo.

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum WaTemplateCategory {
    Marketing,
    Utility,
    Authentication,
}

/// Estados expuestos públicamente. Meta emite además `IN_REVIEW` y `FLAGGED`
/// que se mapean internamente a `Pending` y `Rejected` (con
/// `rejection_reason: "flagged_by_meta_quality"`) antes de persistir.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum WaTemplateStatus {
    Draft,
    Pending,
    Approved,
    Rejected,
    Paused,
    Disabled,
}

/// Documento Mongo en colección `WaTemplates`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaTemplate {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub phone_number_id: String,
    /// Nombre Meta (regex `[a-z][a-z0-9_]{0,511}`). Generado por backend a
    /// partir de `name_input` + flag `is_system`.
    pub name: String,
    /// Etiqueta legible para UI (= `name_input`).
    pub display_name: String,
    /// Texto humano original (auditoría + edits posteriores).
    pub name_input: String,
    pub language: String,
    pub category: WaTemplateCategory,
    /// Header + body + footer + buttons. Mismo shape que Meta espera.
    pub components: Vec<serde_json::Value>,
    /// Count de `{{N}}` distintos en BODY.text. Lo computa el back en write.
    pub body_placeholders: u32,
    pub status: WaTemplateStatus,
    /// Razón Meta cuando `status` ∈ {Rejected, Flagged→Rejected}.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
    /// `id` de Meta (alias `hsm_id`). `None` mientras `status == Draft`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta_template_id: Option<String>,
    /// `true` si es plantilla del sistema (prefix `sistema_abdo_`).
    pub is_system: bool,
    /// Si `false`, queda DRAFT en DB sin tocar Meta. Pasa a `true` cuando
    /// se envía retroactivamente vía PATCH.
    pub submit_to_meta: bool,
    /// UUID del user creador (claims.id). En migración inicial es el sentinel
    /// `00000000-0000-0000-0000-000000000000`.
    pub created_by: String,
    /// Snapshot del nombre del creador al momento de crear (no se actualiza).
    pub created_by_name: String,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

/// Shape de response (string IDs + ISO-8601 dates).
#[derive(Debug, Serialize, ToSchema)]
pub struct WaTemplateItem {
    pub id: String,
    pub phone_number_id: String,
    pub name: String,
    pub display_name: String,
    pub name_input: String,
    pub language: String,
    pub category: WaTemplateCategory,
    pub components: Vec<serde_json::Value>,
    pub body_placeholders: u32,
    pub status: WaTemplateStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta_template_id: Option<String>,
    pub is_system: bool,
    pub submit_to_meta: bool,
    pub created_by: String,
    pub created_by_name: String,
    /// ISO-8601 (RFC 3339) UTC.
    pub created_at: String,
    /// ISO-8601 (RFC 3339) UTC.
    pub updated_at: String,
}

/// Header del template en forma flat (más amigable que la estructura
/// `components` de Meta). El back lo transforma a un componente Meta antes
/// de persistir/enviar.
#[derive(Debug, Deserialize, Clone, ToSchema)]
pub struct WaTemplateHeaderInput {
    /// `TEXT` | `IMAGE` | `VIDEO` | `DOCUMENT`. Si `TEXT`, mandar `text`.
    /// Si IMAGE/VIDEO/DOCUMENT, mandar `example.header_handle: ["<media_id>"]`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Sólo cuando `kind == "TEXT"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Para media: `{ "header_handle": ["<media_id_nuestro>"] }`.
    /// Para TEXT con placeholder: `{ "header_text": ["<sample>"] }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<serde_json::Value>,
}

/// Botón en forma flat. El back lo agrupa en un componente `BUTTONS`.
#[derive(Debug, Deserialize, Clone, ToSchema)]
pub struct WaTemplateButtonInput {
    /// `QUICK_REPLY` | `URL` | `PHONE_NUMBER`.
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_number: Option<String>,
    /// Para URL parametrizado (con `{{1}}` en el `url`): ejemplos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateWaTemplateRequest {
    pub phone_number_id: String,
    /// Texto libre humano (max 512 chars). Back lo slugea para el `name` Meta.
    pub name_input: String,
    /// Si `true`, back genera `name = sistema_abdo_<slug>_<YYYYMMDD>`. Si
    /// `false`, `name = slug(name_input)` directo.
    pub is_system: bool,
    pub category: WaTemplateCategory,
    pub language: String,
    /// Header opcional. Si se omite, el template no tiene header.
    #[serde(default)]
    pub header: Option<WaTemplateHeaderInput>,
    /// Body — required. Texto principal del template, soporta placeholders `{{N}}`.
    pub body: String,
    /// Ejemplos para los placeholders del body. Orden importa: `body_samples[0]`
    /// es el ejemplo de `{{1}}`. Meta los pide para revisar la plantilla.
    #[serde(default)]
    pub body_samples: Option<Vec<String>>,
    /// Footer opcional, ≤ 60 chars.
    #[serde(default)]
    pub footer: Option<String>,
    /// Botones — máx 3 QUICK_REPLY o 1 URL o 1 PHONE_NUMBER. NO mezclar tipos.
    #[serde(default)]
    pub buttons: Option<Vec<WaTemplateButtonInput>>,
    /// Si `false` (default), el doc queda en DRAFT sin tocar Meta.
    #[serde(default)]
    pub submit_to_meta: bool,
}

/// PATCH semantics: si CUALQUIERA de los fields de "components"
/// (`header`, `body`, `body_samples`, `footer`, `buttons`) es `Some`, el back
/// reconstruye el array de components completo desde estos fields. En ese caso
/// `body` es obligatorio (BODY siempre requerido en Meta). Si todos son `None`,
/// se preservan los components actuales del doc.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateWaTemplateRequest {
    /// Sólo aplicable en DRAFT/REJECTED — regenera el `name` Meta.
    pub name_input: Option<String>,
    /// Sólo SUPERADMIN puede flippearlo.
    pub is_system: Option<bool>,
    pub category: Option<WaTemplateCategory>,
    #[serde(default)]
    pub header: Option<WaTemplateHeaderInput>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub body_samples: Option<Vec<String>>,
    #[serde(default)]
    pub footer: Option<String>,
    #[serde(default)]
    pub buttons: Option<Vec<WaTemplateButtonInput>>,
    /// Pasar de `false` a `true` dispara el envío retroactivo a Meta
    /// (transición DRAFT → PENDING).
    pub submit_to_meta: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WaTemplateResponse {
    pub ok: bool,
    pub data: WaTemplateItem,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WaTemplatesListResponse {
    pub ok: bool,
    pub data: Vec<WaTemplateItem>,
    /// Cursor opaco. `None` cuando no hay más páginas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeleteWaTemplateResponse {
    pub ok: bool,
    pub data: DeleteWaTemplateData,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeleteWaTemplateData {
    pub id: String,
}

// ============================================
// UPLOAD DE HEADER MEDIA (GridFS)
// ============================================
//
// El `media_id` devuelto es el ObjectId hex del doc en GridFS. El front
// lo mete en `components[i].example.header_handle[0]` al crear/editar un
// template. NO es el handle Meta — ése se genera on-demand al llamar a
// `upload_to_meta_resumable` dentro de create/update de templates.

#[derive(Debug, Serialize, ToSchema)]
pub struct HeaderMediaUploadData {
    /// ObjectId hex del doc GridFS — identificador estable y reusable entre
    /// templates mientras el binario exista en nuestra DB.
    pub media_id: String,
    pub mime_type: String,
    pub file_size: u64,
    /// SHA-256 hex del binario. Garantiza integridad + habilita dedup server-side.
    pub sha256: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HeaderMediaUploadResponse {
    pub ok: bool,
    pub data: HeaderMediaUploadData,
}

/// Propósito del sistema en el que está en uso una plantilla.
/// Devuelto en el error `template_in_use_cannot_delete` para que el front
/// muestre qué propósitos bloquean el borrado.
#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct WaPurposeUsage {
    /// Clave del propósito ("otp" | "notifications" | "payment_reminder")
    pub key: String,
    /// Etiqueta para UI (mismo string user-facing en español)
    pub label: String,
}

// ============================================
// AUDIT (cross-conversation, SUPERADMIN)
// ============================================

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditMessageItem {
    pub id: String,
    pub conversation_id: String,
    pub customer_phone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer_name: Option<String>,
    pub business_phone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    /// `"in"` o `"out"` (mismo shape que `MessageItem.direction`).
    pub direction: String,
    /// `WaMessage.msg_type` — text|image|audio|video|document|template|...
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_user_name: Option<String>,
    /// `WaMessage.status` (None en inbounds; "sent"/"delivered"/"read"/"failed" en outbounds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditMessagesResponse {
    pub ok: bool,
    pub data: Vec<AuditMessageItem>,
    /// Cursor opaco (`<millis>_<hex_id>`) para la página siguiente. `None` cuando no hay más.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditConversationHeader {
    pub id: String,
    pub customer_phone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer_name: Option<String>,
    pub business_phone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    /// `"pending"`, `"in_progress"` o `"closed"`.
    pub status: String,
    pub created_at: String,
    /// `last_message_at` se usa como proxy de `updated_at` (no hay campo
    /// dedicado en `WaConversations`).
    pub updated_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditConversationEventItem {
    pub id: String,
    /// `created` | `taken` | `transferred` | `closed` | `reopened`
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditAssignedToHistoryItem {
    /// UUID del agente que tuvo la conversación durante este intervalo.
    /// `None` cuando el intervalo representa "nadie asignado" (post-cierre/reopen).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    pub from: String,
    /// `None` indica que es el intervalo activo (sin cierre todavía).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditConversationTimeline {
    pub conversation: AuditConversationHeader,
    pub events: Vec<AuditConversationEventItem>,
    pub message_count: u64,
    pub assigned_to_history: Vec<AuditAssignedToHistoryItem>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditConversationTimelineResponse {
    pub ok: bool,
    pub data: AuditConversationTimeline,
}
