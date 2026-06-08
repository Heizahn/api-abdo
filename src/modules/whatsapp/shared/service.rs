use std::sync::Arc;

use crate::{
    crypto::aes::decrypt_payload, db::WhatsAppRepository, error::ApiError, state::AppState,
};

use crate::modules::whatsapp::service::{MediaRelay, WhatsAppService};

/// Secreto AES para cifrar/descifrar `WaSettings.access_token` en reposo.
/// Reutilizamos `JWT_SECRET` — alta entropía y estrictamente privado del backend.
pub(crate) fn settings_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}

/// Resuelve el `WhatsAppService` para el `business_phone` de una conversación:
/// busca `WaSettings`, descifra el `access_token` y construye el cliente.
pub(crate) async fn resolve_service_for_phone(
    state: &Arc<AppState>,
    business_phone: &str,
) -> Result<WhatsAppService, ApiError> {
    let settings = state
        .db
        .find_wa_settings_by_phone(business_phone)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "wa_settings inactivo o no encontrado para {}",
                business_phone
            ))
        })?;

    if settings.phone_number_id.is_empty() || settings.access_token.is_empty() {
        return Err(ApiError::Internal(
            "wa_settings sin phone_number_id o access_token configurados".into(),
        ));
    }

    let token = decrypt_payload(&settings_secret(), &settings.access_token)
        .ok_or_else(|| ApiError::Internal("no se pudo descifrar access_token".into()))?;

    let svc = WhatsAppService::new(
        state.reqwest_client.clone(),
        settings.phone_number_id,
        token,
    );
    Ok(apply_media_relay(state, svc))
}

/// Aplica el relay de Cloudflare al service si ambas env vars están seteadas
/// en el Config. No-op cuando no hay relay configurado.
pub(crate) fn apply_media_relay(state: &Arc<AppState>, svc: WhatsAppService) -> WhatsAppService {
    match (
        state.config.relay_url.as_ref(),
        state.config.relay_secret.as_ref(),
    ) {
        (Some(url), Some(secret)) => svc.with_media_relay(MediaRelay {
            url: url.clone(),
            secret: secret.clone(),
        }),
        _ => svc,
    }
}
