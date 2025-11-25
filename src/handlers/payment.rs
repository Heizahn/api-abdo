use axum::{
    extract::{Multipart, Path as AxumPath, State}, // Renombrado para evitar conflicto
    Extension, Json,
    response::IntoResponse,
};
use std::sync::Arc;
use std::path::Path; // Este es el Path para manejar archivos y extensiones

use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use chrono::{DateTime, Utc};
use mongodb::bson::oid::ObjectId;

use crate::{
    auth::claims::AccessClaims,
    db::SalesRepository,
    error::ApiError,
    // Importamos ambos modelos desde 'payment' como indicaste
    models::payment::{PaymentMethodResponse, PaymentReport, PagoMovilData},
    state::AppState,
};

/// GET /v1/payments/methods/pago-movil/:debt_id
pub async fn get_pago_movil_data_handler(
    Extension(_claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    AxumPath(debt_id): AxumPath<String>, // Usamos el alias AxumPath
) -> Result<Json<PaymentMethodResponse>, ApiError> {
    tracing::info!("💸 Buscando pago móvil (por ID) para deuda: {}", debt_id);

    let debt = state.db.find_debt_by_id(&debt_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let client = state.db.find_client_owner_by_id(&debt.id_client)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let user_info = state.db.find_user_payment_info_by_id(&client.id_owner)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            tracing::error!("❌ Proveedor no encontrado: {}", client.id_owner);
            ApiError::NotFound
        })?;

    let payment_method_id = match user_info.id_payment_method {
        Some(id) => id,
        None => {
            tracing::warn!("⚠️ El usuario {} no tiene idPaymentMethod configurado", client.id_owner);
            return Ok(Json(PaymentMethodResponse { ok: true, data: None }));
        }
    };

    let payment_method_opt = state.db.find_payment_method_by_id(&payment_method_id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let data = payment_method_opt.map(|pm| {
        PagoMovilData {
            bank_name: pm.bank_name,
            id_number: pm.id_number,
            phone: pm.phone,
        }
    });

    if data.is_none() {
        tracing::warn!("⚠️ Método de pago {} no encontrado o inactivo", payment_method_id);
    } else {
        tracing::info!("✅ Datos de pago recuperados correctamente");
    }

    Ok(Json(PaymentMethodResponse {
        ok: true,
        data
    }))
}

/// POST /v1/payments/report
pub async fn report_payment_handler(
    Extension(claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    tracing::info!("📸 Iniciando reporte de pago para usuario: {}", claims.sub);

    // 1. Variables para capturar los datos
    let mut reference = None;
    let mut date_str = None;
    let mut amount_bs = None;
    let mut bank = None;
    let mut phone = None;
    let mut saved_image_path = None;
    let mut id_debt: Option<ObjectId> = None;
    let mut id_payment_method: Option<ObjectId> = None;

    // 2. Procesar el Multipart
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        tracing::error!("Error multipart: {}", e);
        ApiError::BadRequest("Error leyendo formulario".into())
    })? {
        let name = field.name().unwrap_or("").to_string();

        if name == "image" {
            let file_name = field.file_name().unwrap_or("unknown.jpg").to_string();
            let extension = Path::new(&file_name).extension().and_then(|s| s.to_str()).unwrap_or("jpg");

            let content_type = field.content_type().unwrap_or("application/octet-stream");
            if !content_type.starts_with("image/") && content_type != "application/pdf" {
                return Err(ApiError::BadRequest("El archivo debe ser una imagen o PDF".into()));
            }

            let unique_name = format!("{}.{}", Uuid::new_v4(), extension);
            let file_path = format!("uploads/{}", unique_name);

            let data = field.bytes().await.map_err(|_| ApiError::InternalServerError)?;

            let mut file = File::create(&file_path).await.map_err(|e| {
                tracing::error!("❌ Error guardando archivo en disco: {}", e);
                ApiError::InternalServerError
            })?;
            file.write_all(&data).await.map_err(|_| ApiError::InternalServerError)?;

            saved_image_path = Some(format!("/uploads/{}", unique_name));

        } else {
            let text = field.text().await.unwrap_or_default();
            match name.as_str() {
                "reference" => reference = Some(text),
                "date" => date_str = Some(text),
                "amount_bs" => amount_bs = text.parse::<f64>().ok(),
                "bank" => bank = Some(text),
                "phone" => phone = Some(text),
                "id_debt" => { if let Ok(oid) = ObjectId::parse_str(&text) { id_debt = Some(oid); } },
                "id_payment_method" => { if let Ok(oid) = ObjectId::parse_str(&text) { id_payment_method = Some(oid); } },
                _ => {}
            }
        }
    }

    // 3. Validaciones
    if reference.is_none() || amount_bs.is_none() || saved_image_path.is_none() {
        return Err(ApiError::BadRequest("Faltan datos: referencia, monto o imagen".into()));
    }

    // CORRECCIÓN 1: Agregamos el ? al final
    let client_id = ObjectId::parse_str(&claims.sub).map_err(|_| ApiError::Unauthorized);
    let amount_bs_val = amount_bs.unwrap();

    // 4. Tasa de cambio
    let exchange_rate = state.db.get_latest_exchange_rate().await.map_err(|e| {
        tracing::error!("❌ Error obteniendo tasa BCV: {:?}", e);
        ApiError::InternalServerError
    })?;

    let amount_usd = (amount_bs_val / exchange_rate * 100.0).round() / 100.0;

    // 5. Parsear fecha
    let payment_date = match date_str {
        Some(d) => d.parse::<DateTime<Utc>>().unwrap_or(Utc::now()),
        None => Utc::now(),
    };

    // 6. Construir Modelo
    let new_report = PaymentReport {
        id: None,
        id_client: client_id.ok(), // CORRECCIÓN 2: Usamos client_id directo (es ObjectId)
        id_debt,
        id_payment_method,
        reference: reference.unwrap(),
        payment_date,
        amount_bs: amount_bs_val,
        bank_origin: bank.unwrap_or_default(),
        phone_number: phone.unwrap_or_default(),
        image_url: saved_image_path.unwrap(),
        amount_usd,
        exchange_rate,
        state: "Pendiente".to_string(),
        created_at: Utc::now(),
    };

    // 7. Guardar
    // CORRECCIÓN 3: Usamos un closure |e| para map_err
    let result = state.db.create_payment_report(new_report).await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": "Pago reportado correctamente",
        "data": {
            "id": result.inserted_id,
            "amount_usd": amount_usd,
            "status": "Pendiente"
        }
    })))
}