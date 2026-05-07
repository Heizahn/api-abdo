use super::{parser as mikrotik_parse_update, service as mikrotik_service};
use crate::db::OnuRepository;
use crate::state::AppState;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub async fn run_mikrotik_sync_task(state: Arc<AppState>) {
    tracing::info!("📡 Servicio MikroTik iniciado (Ejecución cada 20 min)");

    let id_simcot = state.config.id_simcot.clone();
    let mut interval = tokio::time::interval(Duration::from_secs(20 * 60));

    loop {
        interval.tick().await;

        tracing::info!("🔄 Ejecutando sincronización de leases MikroTik...");

        let ip = "10.255.255.5".to_string();
        let port = state.config.port_mk.clone();
        let user = "rust_api".to_string();
        let pass = state.config.pass_mk.clone();

        let file_path = "mk_reports/leases_5.txt".to_string();
        let file_path_clone = file_path.clone();

        let handle = tokio::task::spawn_blocking(move || {
            if let Some(parent) = Path::new(&file_path).parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    return Err(anyhow::anyhow!("No se pudo crear directorio: {}", e));
                }
            }
            mikrotik_service::fetch_mikrotik_leases_to_file(&ip, &port, &user, &pass, &file_path)
        })
        .await;

        match handle {
            Ok(Ok(_)) => {
                tracing::info!("✅ Leases descargados en: {}", &file_path_clone);

                match state.db.get_onus_for_update_ip().await {
                    Ok(onus) => {
                        tracing::info!("📦 DB retornó {} ONUs para comparar.", onus.len());

                        if onus.is_empty() {
                            tracing::warn!("⚠️ No hay ONUs en la DB con sMac para comparar. Revisa tu colección.");
                            continue;
                        }

                        match mikrotik_parse_update::parse_mikrotik_leases(&file_path_clone, &onus)
                        {
                            Ok(onus_to_update) => {
                                if onus_to_update.is_empty() {
                                    tracing::info!(
                                        "✨ Todo está sincronizado. No hay cambios de IP."
                                    );
                                } else {
                                    for onu in onus_to_update {
                                        match state.db.update_onu_ip(onu, &id_simcot).await {
                                            Ok(_) => {}
                                            Err(e) => {
                                                tracing::error!("❌ Falló update en DB: {}", e)
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("❌ Error parseando el archivo de leases: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("❌ Error obteniendo ONUs de la DB: {}", e);
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!("❌ Error lógico en servicio MikroTik (SSH/File): {}", e);
            }
            Err(e) => {
                tracing::error!("❌ Panic en el hilo de MikroTik: {}", e);
            }
        }
    }
}
