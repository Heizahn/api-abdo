use crate::{db::WhatsAppRepository, models::whatsapp::WaConversationEventInput, state::AppState};

pub mod handler;
pub mod media_failures;
pub mod normalize;
pub mod status;

/// Persiste un evento de ciclo de vida de conversación. Best-effort:
/// si la inserción falla se loggea pero NO se propaga el error — la
/// auditoría no debe bloquear la respuesta HTTP del agente.
pub(super) async fn record_conv_event(state: &AppState, input: WaConversationEventInput<'_>) {
    if let Err(e) = state.db.record_conversation_event(input).await {
        tracing::warn!("record_conversation_event failed: {}", e);
    }
}
