use axum::extract::Path;
use axum::response::{Html, IntoResponse};
use axum::{extract::State, Extension, Json};
use hyper::header;
use std::sync::Arc;

use crate::auth::claims::AccessClaims;

use crate::db::UtilsRepository;
use crate::models::db::LatestVersionResponse;
use crate::{
    db::SalesRepository,
    error::ApiError,
    models::db::PingResponse,
    models::payment::{Bank, BankListResponse},
    state::AppState,
};

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
) -> Result<impl IntoResponse, ApiError> {
    // 1. Construir la ruta al archivo.
    // IMPORTANTE: 'uploads' está en la raiz, al mismo nivel que src
    let path = format!("/uploads/{}", filename);

    // 2. Seguridad básica: evitar que intenten leer otros archivos con "../"
    if filename.contains("..") {
        // Retorna un error de tu ApiError (ajusta según tu enum)
        return Err(ApiError::BadRequest(
            "Ruta inválida o intento de hackeo".to_string(),
        ));
    }

    // 3. Intentar leer el archivo
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            // 4. Determinar el Content-Type manualmente
            let content_type = if filename.ends_with(".png") {
                "image/png"
            } else if filename.ends_with(".jpg") || filename.ends_with(".jpeg") {
                "image/jpeg"
            } else if filename.ends_with(".webp") {
                "image/webp"
            } else {
                "application/octet-stream" // Tipo genérico
            };

            // 5. Devolver la respuesta con el header correcto
            Ok(([(header::CONTENT_TYPE, content_type)], bytes))
        }
        Err(_) => {
            // Si el archivo no existe, retornamos error 404 (ajusta a tu ApiError)
            Err(ApiError::NotFound)
        }
    }
}
