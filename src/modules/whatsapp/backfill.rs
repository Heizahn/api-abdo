use std::sync::Arc;

use crate::{
    crypto::aes::decrypt_payload,
    db::WhatsAppRepository,
    state::AppState,
};

use super::service::WhatsAppService;

/// Backfill one-shot al arrancar: rellena `last_inbound_at` en conversaciones
/// que no lo tengan (doc anterior al deploy de Feature 3), usando el timestamp
/// máximo de los mensajes inbound. Sin esto, la ventana de 24h queda cerrada
/// para toda conversación legacy aunque el cliente haya escrito hace minutos.
pub async fn run_last_inbound_backfill(state: Arc<AppState>) {
    match state.db.backfill_last_inbound_at().await {
        Ok(0) => tracing::info!("last-inbound-backfill: nada que hacer"),
        Ok(n) => tracing::info!("last-inbound-backfill: {} conversaciones actualizadas", n),
        Err(e) => tracing::warn!("last-inbound-backfill: {}", e),
    }
}

/// Backfill one-shot al arrancar: por cada `WaSettings` sin
/// `whatsapp_business_account_id`, consulta a Meta con el access_token propio
/// del workspace y persiste el WABA ID. Best-effort: un fallo por número no
/// detiene al resto.
pub async fn run_waba_backfill(state: Arc<AppState>) {
    let settings_list = match state.db.find_wa_settings_missing_waba().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("waba-backfill: no se pudo listar WaSettings: {}", e);
            return;
        }
    };

    if settings_list.is_empty() {
        return;
    }

    tracing::info!(
        "waba-backfill: procesando {} WaSettings sin WABA ID",
        settings_list.len()
    );

    let secret = settings_secret();

    for s in settings_list {
        let id = match s.id {
            Some(oid) => oid,
            None => continue,
        };
        let phone_number_id = s.phone_number_id.trim().to_string();
        if phone_number_id.is_empty() || s.access_token.is_empty() {
            tracing::info!(
                "waba-backfill: {} sin phone_number_id o access_token, se omite",
                id.to_hex()
            );
            continue;
        }

        let token = match decrypt_payload(&secret, &s.access_token) {
            Some(t) => t,
            None => {
                tracing::warn!(
                    "waba-backfill: {} no se pudo descifrar access_token",
                    id.to_hex()
                );
                continue;
            }
        };

        let svc = WhatsAppService::new(
            state.reqwest_client.clone(),
            phone_number_id.clone(),
            token,
        );

        match svc.get_whatsapp_business_account_id().await {
            Ok(waba_id) if !waba_id.is_empty() => {
                match state.db.set_wa_settings_waba_id(&id, &waba_id).await {
                    Ok(()) => tracing::info!(
                        "waba-backfill: {} WABA ID = {}",
                        id.to_hex(),
                        waba_id
                    ),
                    Err(e) => tracing::warn!(
                        "waba-backfill: {} falló al persistir: {}",
                        id.to_hex(),
                        e
                    ),
                }
            }
            Ok(_) => tracing::warn!(
                "waba-backfill: {} Meta devolvió WABA vacío",
                id.to_hex()
            ),
            Err(e) => tracing::warn!(
                "waba-backfill: {} Meta error: {:#}",
                id.to_hex(),
                e
            ),
        }
    }

    tracing::info!("waba-backfill: finalizado");
}

fn settings_secret() -> String {
    std::env::var("JWT_SECRET").unwrap_or_default()
}
