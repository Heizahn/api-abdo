use axum::extract::Path;
use axum::response::{Html, IntoResponse};
use axum::{extract::State, Extension, Json};
use hyper::header;
use std::env;
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
    
    // 1. Obtener el directorio actual de trabajo (CWD) para debuggear
    let current_dir = env::current_dir().unwrap_or_default();
    
    // NOTA: Quité la barra inicial "/" antes de uploads. 
    // Si pones "/uploads", busca en la raíz del sistema operativo.
    // Al poner "uploads/", busca relativo a donde ejecutaste el comando cargo run.
    let path_str = format!("uploads/{}", filename); 
    
    // Tratamos de obtener la ruta absoluta para ver en el log
    let absolute_path = current_dir.join(&path_str);

    println!("---------------- DEBUG IMAGEN ----------------");
    println!("1. Directorio de ejecución (CWD): {:?}", current_dir);
    println!("2. Filename recibido: {}", filename);
    println!("3. Ruta relativa construida: {}", path_str);
    println!("4. Ruta absoluta intentada: {:?}", absolute_path);

    // 2. Seguridad básica
    if filename.contains("..") {
        println!("ERROR: Intento de Path Traversal detectado");
        return Err(ApiError::BadRequest(
            "Ruta inválida o intento de hackeo".to_string(),
        ));
    }

    // 3. Intentar leer el archivo
    match tokio::fs::read(&path_str).await {
        Ok(bytes) => {
            println!("EXITO: Imagen encontrada y leída. Bytes: {}", bytes.len());
            
            let content_type = if filename.ends_with(".png") {
                "image/png"
            } else if filename.ends_with(".jpg") || filename.ends_with(".jpeg") {
                "image/jpeg"
            } else if filename.ends_with(".webp") {
                "image/webp"
            } else {
                "application/octet-stream"
            };

            Ok(([(header::CONTENT_TYPE, content_type)], bytes))
        }
        Err(e) => {
            // AQUÍ VERÁS EL ERROR REAL DEL SISTEMA OPERATIVO
            println!("ERROR FATAL LEYENDO ARCHIVO: {:?}", e);
            println!("¿El archivo realmente existe en {:?}?", absolute_path);
            println!("----------------------------------------------");
            
            // Si el error es NotFound, retornamos NotFound, si es permisos, etc.
            Err(ApiError::NotFound)
        }
    }
}