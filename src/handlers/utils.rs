use axum::body::Body;
use axum::extract::Path;
use axum::response::{Html, Response};
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
