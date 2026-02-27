use axum::body::Body;
use axum::extract::Path;
use axum::response::{Html, Response};
use axum::{extract::State, Extension, Json};
use hyper::header;
use std::sync::Arc;
use serde::Deserialize;
use crate::auth::claims::AccessClaims;

use crate::db::UtilsRepository;
use crate::models::db::{BcvResponse, LatestVersionResponse};
use crate::{
    db::SalesRepository,
    error::ApiError,
    models::db::PingResponse,
    models::payment::{Bank, BankListResponse},
    state::AppState,
};
use crate::services::get_ip_pppoe_mk::get_ip_pppoe_mk;
use crate::models::zabbix::ZabbixTrafficResponse;
use crate::services::zabbix_service;

pub async fn get_bank_list(
    Extension(_claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<BankListResponse>, ApiError> {
    let banks: Vec<Bank> = state.db.find_bank_list().await.or_else(|e| {
        tracing::error!("Error finding bank list: {}", e);
        Err(ApiError::DatabaseError(e.to_string()))
    })?;
    let banks_formatter = banks
        .into_iter()
        .map(|bank| Bank {
            id: bank.id,
            bank_code: bank.bank_code,
            bank_name: bank.bank_name,
        })
        .collect::<Vec<Bank>>();

    Ok(Json(BankListResponse {
        ok: true,
        data: banks_formatter,
    }))
}

pub async fn get_ping_response() -> Result<Json<PingResponse>, ApiError> {
    Ok(Json(PingResponse {
        ok: true,
        message: "pong".to_string(),
    }))
}

//endpoiint para devolver el latest version de la app
pub async fn get_latest_version_response(
    State(state): State<Arc<AppState>>,
) -> Result<Json<LatestVersionResponse>, ApiError> {
    let latest_version = state.db.find_latest_version().await.or_else(|e| {
        tracing::error!("Error finding latest version: {}", e);
        Err(ApiError::DatabaseError(e.to_string()))
    })?;

    if let Some(version) = latest_version {
        Ok(Json(LatestVersionResponse {
            ok: true,
            data: version,
        }))
    } else {
        Err(ApiError::NotFound)
    }
}

pub async fn get_privacy_policy() -> Result<Html<String>, ApiError> {
    tracing::info!("Handling get_privacy_policy request");
    let privacy_policy = include_str!("../../public/privacy_policy.html");
    Ok(Html(privacy_policy.to_string()))
}

pub async fn get_image(
    Path(filename): Path<String>,
    State(_state): State<Arc<AppState>>,
) -> Result<Response, ApiError> {
    // 1. Evitar ataques de "Directory Traversal" (seguridad básica)
    if filename.contains("..") || filename.starts_with('/') || filename.contains('\\') {
        return Err(ApiError::NotFound);
    }

    // 2. Construir la ruta (coincide con tu carpeta 'uploads' en la raíz)
    let path = format!("uploads/{}", filename);

    // 3. Leer el archivo
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            // 4. Determinar el tipo de contenido (MIME Type) manualmente
            // Nota: Podrías usar la librería 'mime_guess' para esto, pero aquí lo hago manual
            let content_type = if path.ends_with(".png") {
                "image/png"
            } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
                "image/jpeg"
            } else if path.ends_with(".webp") {
                "image/webp"
            } else {
                "application/octet-stream" // Tipo genérico
            };

            // 5. Construir la respuesta con el Header correcto
            let response = Response::builder()
                .header(header::CONTENT_TYPE, content_type)
                // Opcional: Cache-Control para que el navegador guarde la imagen un tiempo
                .header(header::CACHE_CONTROL, "public, max-age=3600")
                .body(Body::from(bytes))
                .unwrap(); // En producción maneja este unwrap mejor

            Ok(response)
        }
        Err(_) => Err(ApiError::NotFound),
    }
}

pub async fn get_bcv(State(state): State<Arc<AppState>>) -> Result<Json<BcvResponse>, ApiError> {
    let bcv = state.db.get_latest_exchange_rate().await;

    match bcv {
        Ok(bcv) => Ok(Json(BcvResponse { bcv })),
        Err(e) => Err(ApiError::DatabaseError(e.to_string())),
    }
}

pub async fn get_ip_pppoe(
    Path(sn): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<String>, ApiError> {
    let port_mk = state.config.port_mk.clone();
    let pass_mk = state.config.pass_mk.clone();

    // Lista de routers donde vamos a buscar
    // (A futuro puedes sacar esto de state.config.routers)
    let routers = vec!["10.255.255.5", "10.255.255.8"];

    let ip_pppoe_result = tokio::task::spawn_blocking(move || {
        let mut last_error = String::new();

        for router_ip in routers {
            // Intentamos buscar en el router actual
            match get_ip_pppoe_mk(&sn, router_ip, port_mk.as_str(), "rust_api", pass_mk.as_str()) {
                Ok(ip) => return Ok(ip), // Lo encontró: sale del loop y devuelve la IP inmediatamente
                Err(e) => {
                    last_error = e;
                    // No está aquí o hubo un fallo de conexión, probamos en el siguiente
                    continue;
                }
            }
        }

        // Si terminó el bucle y no retornó OK, es porque no estaba en ningún router
        Err(last_error)
    })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Evaluación final para Axum
    match ip_pppoe_result {
        Ok(ip) => Ok(Json(ip)),
        Err(e) => {
            if e.contains("no tiene una sesión activa") {
                Err(ApiError::NotFound) // Devuelve HTTP 404
            } else {
                Err(ApiError::Internal(e)) // Devuelve HTTP 500 para otros errores
            }
        }
    }
}
pub async fn get_zabbix(
    Path(id_client): Path<String>,
    State(state): State<Arc<AppState>>
) -> Result<Json<ZabbixTrafficResponse>, ApiError> {

    // 1. Buscar en tu DB el cliente
    // TODO: Reemplaza esto con tu query real a la DB usando state.db
    let (client_zabbix_code, olt_zabbix_name) = state.db.find_client_olt_position(&id_client).await.map_err(|_| ApiError::NotFound)?;



    // 2. Llamar al servicio inyectando el cliente HTTP del State
    let traffic_data = zabbix_service::get_client_traffic(
        &state.reqwest_client,
        &state.config.zabbix_url,
        &state.config.zabbix_token,
        &client_zabbix_code,
        &olt_zabbix_name
    ).await.map_err(|e| {
        // Mapea el error del servicio a tu ApiError personalizado
        eprintln!("Error en Zabbix Service: {}", e);
        ApiError::Internal("Error al consultar el tráfico histórico".to_string())
    })?;

    Ok(Json(traffic_data))
}
