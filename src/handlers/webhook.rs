use axum::{
    extract::Query,
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct WebhookVerifyParams {
    #[serde(rename = "hub.mode")]
    pub mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub challenge: Option<String>,
}

/// GET /v1/webhook/whatsapp
/// Verificación inicial del webhook por parte de Meta.
/// Meta envía hub.mode=subscribe, hub.verify_token y hub.challenge;
/// si el token coincide se devuelve el challenge y la ruta queda activa.
pub async fn verify_webhook(
    Query(params): Query<WebhookVerifyParams>,
) -> impl IntoResponse {
    let verify_token = std::env::var("WHATSAPP_VERIFY_TOKEN").unwrap_or_default();

    if params.mode.as_deref() == Some("subscribe")
        && params.verify_token.as_deref() == Some(verify_token.as_str())
    {
        let challenge = params.challenge.unwrap_or_default();
        tracing::info!("WhatsApp webhook verificado correctamente");
        (StatusCode::OK, challenge)
    } else {
        tracing::warn!("WhatsApp webhook: token de verificación inválido");
        (StatusCode::FORBIDDEN, "Token inválido".to_string())
    }
}

/// POST /v1/webhook/whatsapp
/// Recibe notificaciones de mensajes entrantes de WhatsApp Business.
/// Meta espera siempre HTTP 200; cualquier otra respuesta provoca reenvíos.
pub async fn receive_webhook(
    body: axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    tracing::info!("WhatsApp webhook recibido: {:?}", body);

    // Estructura del payload:
    // body["entry"][0]["changes"][0]["value"]["messages"][0]
    //   .type  → "text" | "button" | "interactive" | ...
    //   .from  → número del remitente (E.164 sin "+")
    //   .text.body → contenido del mensaje

    StatusCode::OK
}
