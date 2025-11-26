use axum::{
    extract::{Multipart, Path as AxumPath, State}, // Renombrado para evitar conflicto
    response::IntoResponse,
    Extension,
    Json,
};
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mongodb::bson::oid::ObjectId;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::{
    auth::claims::AccessClaims,
    db::{ProfileRepository, SalesRepository},
    error::ApiError,
    // Importamos ambos modelos desde 'payment' como indicaste
    models::payment::{PagoMovilData, PaymentMethodResponse, PaymentReport},
    state::AppState,
};

/// GET /v1/payments/methods/pago-movil/:debt_id
pub async fn get_pago_movil_data_handler(
    Extension(_claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    AxumPath(debt_id): AxumPath<String>, // Usamos el alias AxumPath
) -> Result<Json<PaymentMethodResponse>, ApiError> {
    tracing::info!("💸 Buscando pago móvil (por ID) para deuda: {}", debt_id);

    let debt = state
        .db
        .find_debt_by_id(&debt_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let client = state
        .db
        .find_client_owner_by_id(&debt.id_client)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    let user_info = state
        .db
        .find_user_payment_info_by_id(&client.id_owner)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            tracing::error!("❌ Proveedor no encontrado: {}", client.id_owner);
            ApiError::NotFound
        })?;

    let payment_method_id = match user_info.id_payment_method {
        Some(id) => id,
        None => {
            tracing::warn!(
                "⚠️ El usuario {} no tiene idPaymentMethod configurado",
                client.id_owner
            );
            return Ok(Json(PaymentMethodResponse {
                ok: true,
                data: None,
            }));
        }
    };

    let payment_method_opt = state
        .db
        .find_payment_method_by_id(&payment_method_id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let data = payment_method_opt.map(|pm| PagoMovilData {
        id: pm.id.map(|oid| oid.to_string()).unwrap_or_default(),
        bank_name: pm.bank_name,
        id_number: pm.id_number,
        phone: pm.phone,
    });

    if data.is_none() {
        tracing::warn!(
            "⚠️ Método de pago {} no encontrado o inactivo",
            payment_method_id
        );
    } else {
        tracing::info!("✅ Datos de pago recuperados correctamente");
    }

    Ok(Json(PaymentMethodResponse { ok: true, data }))
}

/// POST /v1/payments/report
pub async fn report_payment_handler(
    Extension(_claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    tracing::info!("📸 Iniciando reporte de pago (Cliente determinado por deuda)");

    // 1. Variables
    let mut reference = None;
    let mut date_str = None;
    let mut amount_bs = None;
    let mut bank = None;
    let mut phone = None;
    let mut saved_image_path = None;
    let mut id_debt_str: Option<String> = None;
    let mut id_payment_method: Option<ObjectId> = None;

    // 2. Procesar Multipart
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        tracing::error!("Error multipart: {}", e);
        ApiError::BadRequest("Error leyendo formulario".into())
    })? {
        let name = field.name().unwrap_or("").to_string();

        if name == "image" {
            let file_name = field.file_name().unwrap_or("unknown.jpg").to_string();
            let extension = Path::new(&file_name)
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("jpg");

            let content_type = field.content_type().unwrap_or("application/octet-stream");
            if !content_type.starts_with("image/") && content_type != "application/pdf" {
                return Err(ApiError::BadRequest(
                    "El archivo debe ser una imagen o PDF".into(),
                ));
            }

            let unique_name = format!("{}.{}", Uuid::new_v4(), extension);
            let file_path = format!("uploads/{}", unique_name);

            let data = field
                .bytes()
                .await
                .map_err(|_| ApiError::InternalServerError)?;

            let mut file = File::create(&file_path).await.map_err(|e| {
                tracing::error!("❌ Error guardando archivo: {}", e);
                ApiError::InternalServerError
            })?;
            file.write_all(&data)
                .await
                .map_err(|_| ApiError::InternalServerError)?;

            saved_image_path = Some(format!("/uploads/{}", unique_name));
        } else {
            let text = field.text().await.unwrap_or_default();
            match name.as_str() {
                "reference" => reference = Some(text),
                "date" => date_str = Some(text),
                "amount_bs" => amount_bs = text.parse::<f64>().ok(),
                "bank" => bank = Some(text),
                "phone" => phone = Some(text),
                "id_debt" => id_debt_str = Some(text),
                "id_payment_method" => {
                    id_payment_method = Some(ObjectId::parse_str(&text).unwrap())
                }
                _ => {}
            }
        }
    }

    // 3. Validaciones (BLINDADO)
    // Verificamos que existan TODOS los campos obligatorios antes de usar .unwrap()
    if reference.is_none()
        || amount_bs.is_none()
        || saved_image_path.is_none()
        || id_debt_str.is_none()
        || id_payment_method.is_none()
    {
        return Err(ApiError::BadRequest(
            "Faltan datos obligatorios (referencia, monto, imagen, deuda o método)".into(),
        ));
    }

    // Extracción segura
    let amount_bs_val = amount_bs.unwrap();
    let id_debt_raw = id_debt_str.unwrap();
    let id_pm_raw = id_payment_method.unwrap();

    // Parseo seguro de ObjectIds (Evita crash si el ID está malformado)
    let id_debt_oid = ObjectId::parse_str(&id_debt_raw)
        .map_err(|_| ApiError::BadRequest("ID de deuda inválido".into()))?;

    let id_pm_oid = id_pm_raw;

    // 4. Buscar Deuda y Cliente Real
    // Nota: Asumo que tu función find_debt_by_id acepta un ObjectId. Si acepta String, pasa &id_debt_raw
    let debt_document = state
        .db
        .find_debt_by_id(&id_debt_oid.to_string())
        .await
        .map_err(|e| {
            tracing::error!("❌ Error DB buscando deuda: {:?}", e);
            ApiError::InternalServerError
        })?;

    let real_client_id = debt_document
        .ok_or(ApiError::BadRequest(
            "La deuda seleccionada no existe".into(),
        ))?
        .id_client; // Aquí obtenemos el ID del cliente real

    let client = state
        .db
        .find_client_by_id(&real_client_id.to_string())
        .await
        .map_err(|e| ApiError::DatabaseError(e))?;

    let default_rate = 1.08;

    let iva_rate = match client.id_tax {
        Some(tax_id) => {
            // El cliente tiene un ID de impuesto configurado
            let tax_doc = state
                .db
                .find_tax_by_id(&tax_id)
                .await
                .map_err(|e| ApiError::DatabaseError(e))?; // Error de conexión

            match tax_doc {
                Some(t) => {
                    tracing::info!(
                        "🧾 Impuesto personalizado encontrado: {} ({})",
                        t.target,
                        t.iva
                    );
                    t.iva // Retorna el valor de la BD (ej. 0.16)
                }
                None => {
                    tracing::warn!(
                        "⚠️ Cliente tiene idTax {} pero no existe en 'taxes'. Usando defecto {:.2}",
                        tax_id,
                        default_rate
                    );
                    default_rate
                }
            }
        }
        None => {
            // El cliente no tiene ID de impuesto (campo vacío o nulo)
            tracing::info!(
                "ℹ️ Cliente sin configuración de impuestos. Usando defecto {:.2}",
                default_rate
            );
            default_rate
        }
    };

    // 5. Tasa de cambio
    let exchange_rate = state.db.get_latest_exchange_rate().await.map_err(|e| {
        tracing::error!("❌ Error tasa BCV: {:?}", e);
        ApiError::InternalServerError
    })?;

    let amount_bs_neto = amount_bs_val / iva_rate;

    let amount_usd = (amount_bs_neto / exchange_rate * 100.0).round() / 100.0;

    tracing::info!(
        "💰 Matemáticas: Total: {} Bs | IVA: {} | Neto: {:.2} Bs | Tasa: {} | Final: {} USD",
        amount_bs_val,
        iva_rate,
        amount_bs_neto,
        exchange_rate,
        amount_usd
    );

    // 6. Fecha
    let payment_date = match date_str {
        Some(d) => d.parse::<DateTime<Utc>>().unwrap_or(Utc::now()),
        None => Utc::now(),
    };

    // 7. Construir Modelo
    let new_report = PaymentReport {
        id: None,
        id_client: Some(real_client_id), // Usamos el ID recuperado de la deuda
        id_debt: Some(id_debt_oid),      // Usamos el OID parseado arriba
        id_payment_method: Some(id_pm_oid), // Usamos el OID parseado arriba
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

    // 8. Guardar
    let result = state
        .db
        .create_payment_report(new_report)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    tracing::info!("✅ Pago reportado. ID: {}", result.inserted_id);

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
