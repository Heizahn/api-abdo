use axum::{
    extract::{Multipart, Path as AxumPath, State},
    response::IntoResponse,
    Extension, Json,
};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mongodb::bson::oid::ObjectId;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::{
    auth::claims::AccessClaims,
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, SalesRepository},
    error::ApiError,
    models::payment::{PagoMovilData, PaymentMethodResponse, PaymentReport},
    state::AppState,
};

#[utoipa::path(
    get,
    path = "/v1/payments/methods/payment/{debt_id}",
    tag = "Payments",
    security(("bearerAuth" = [])),
    params(("debt_id" = String, Path, description = "ObjectId de la deuda")),
    responses(
        (status = 200, description = "Datos de pago móvil del proveedor dueño del cliente. `data` puede ser null si el proveedor no configuró método.", body = PaymentMethodResponse),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "Deuda/cliente/proveedor no encontrados"),
    )
)]
pub async fn get_pago_movil_data_handler(
    Extension(_claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    AxumPath(debt_id): AxumPath<String>,
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

#[utoipa::path(
    get,
    path = "/v1/payments/methods/payment/by-client/{client_id}",
    tag = "Payments",
    security(("bearerAuth" = [])),
    params(("client_id" = String, Path, description = "ObjectId del cliente")),
    responses(
        (status = 200, description = "Datos de pago móvil del proveedor dueño del cliente", body = PaymentMethodResponse),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "Cliente/proveedor no encontrados"),
    )
)]
pub async fn get_pago_movil_data_by_client_handler(
    Extension(_claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    AxumPath(client_id): AxumPath<String>,
) -> Result<Json<PaymentMethodResponse>, ApiError> {
    tracing::info!(
        "💸 Buscando pago móvil (por ID) para cliente: {}",
        client_id
    );

    let client_id_oid = ObjectId::parse_str(&client_id).map_err(|_| ApiError::NotFound)?;

    let client = state
        .db
        .find_client_owner_by_id(&client_id_oid)
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

#[utoipa::path(
    get,
    path = "/v1/auth-user/payments/methods/by-client/{client_id}",
    tag = "Payments",
    security(("bearerAuth" = [])),
    params(("client_id" = String, Path, description = "ObjectId del cliente")),
    responses(
        (status = 200, description = "Datos de pago móvil del proveedor dueño del cliente (endpoint staff)", body = PaymentMethodResponse),
        (status = 401, description = "No autorizado"),
        (status = 404, description = "Cliente/proveedor no encontrados"),
    )
)]
pub async fn get_pago_movil_data_by_client_user_handler(
    Extension(_claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
    AxumPath(client_id): AxumPath<String>,
) -> Result<Json<PaymentMethodResponse>, ApiError> {
    tracing::info!(
        "💸 Buscando pago móvil (por ID) para cliente: {}",
        client_id
    );

    let client_id_oid = ObjectId::parse_str(&client_id).map_err(|_| ApiError::NotFound)?;

    let client = state
        .db
        .find_client_owner_by_id(&client_id_oid)
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

#[utoipa::path(
    post,
    path = "/v1/payments/payment/report",
    tag = "Payments",
    security(("bearerAuth" = [])),
    request_body(
        content = String,
        content_type = "multipart/form-data",
        description = "multipart/form-data con: `reference` (str), `amount_bs` (f64 como str), `date` (RFC3339 opcional), `bank` (str), `phone` (str), `image` (file), `id_payment_method` (ObjectId), y exactamente uno de `id_debt` o `id_client`."
    ),
    responses(
        (status = 200, description = "Reporte de pago registrado en estado Pendiente"),
        (status = 400, description = "Faltan datos básicos o IDs inválidos"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn report_payment_handler(
    Extension(_claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    tracing::info!("📸 Iniciando reporte de pago (Abono o Deuda)");

    let mut reference = None;
    let mut date_str = None;
    let mut amount_bs = None;
    let mut bank = None;
    let mut phone = None;
    let mut saved_image_path = None;
    let mut id_debt_str: Option<String> = None;
    let mut id_client_str: Option<String> = None;
    let mut id_payment_method_str: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        tracing::error!("Error multipart: {}", e);
        ApiError::BadRequest("Error leyendo formulario".into())
    })? {
        let name = field.name().unwrap_or("").to_string();

        if name == "image" {
            let content_type = field.content_type().unwrap_or("image/jpeg").to_string();
            let extension = match content_type.as_str() {
                "image/png" => "png",
                "image/webp" => "webp",
                "image/gif" => "gif",
                _ => "jpg",
            };
            let unique_name = format!("{}.{}", Uuid::new_v4(), extension);
            let file_path = format!("uploads/{}", unique_name);

            let data = field
                .bytes()
                .await
                .map_err(|_| ApiError::InternalServerError)?;

            if data.is_empty() {
                tracing::error!("Imagen recibida esta vacia (0 bytes)");
                return Err(ApiError::BadRequest(
                    "La imagen llego vacia al servidor".into(),
                ));
            }

            tracing::info!(
                "Imagen recibida: {} bytes, tipo: {}",
                data.len(),
                content_type
            );

            let mut file = File::create(&file_path)
                .await
                .map_err(|_| ApiError::InternalServerError)?;
            file.write_all(&data)
                .await
                .map_err(|_| ApiError::InternalServerError)?;
            file.flush()
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
                "id_client" => id_client_str = Some(text),
                "id_payment_method" => id_payment_method_str = Some(text),
                _ => {}
            }
        }
    }

    if reference.is_none()
        || amount_bs.is_none()
        || saved_image_path.is_none()
        || id_payment_method_str.is_none()
    {
        return Err(ApiError::BadRequest(
            "Faltan datos básicos (ref, monto, imagen o método)".into(),
        ));
    }

    if id_debt_str.is_none() && id_client_str.is_none() {
        return Err(ApiError::BadRequest(
            "Debe especificar una Deuda o un Cliente para el abono".into(),
        ));
    }

    let id_pm_oid = ObjectId::parse_str(&id_payment_method_str.unwrap())
        .map_err(|_| ApiError::BadRequest("Método de pago inválido".into()))?;

    let mut id_debt_oid: Option<ObjectId> = None;
    let real_client_id: ObjectId;

    if let Some(d_str) = id_debt_str {
        let oid = ObjectId::parse_str(&d_str)
            .map_err(|_| ApiError::BadRequest("ID deuda malformado".into()))?;
        let debt_doc = state
            .db
            .find_debt_by_id(&oid.to_string())
            .await
            .map_err(|e| ApiError::DatabaseError(e))?
            .ok_or(ApiError::BadRequest("La deuda no existe".into()))?;

        real_client_id = debt_doc.id_client;
        id_debt_oid = Some(oid);
    } else {
        let c_str = id_client_str.unwrap();
        real_client_id = ObjectId::parse_str(&c_str)
            .map_err(|_| ApiError::BadRequest("ID cliente malformado".into()))?;
    }

    let client = state
        .db
        .find_client_by_id(&real_client_id.to_string())
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let default_rate = 1.0;

    let iva_rate = if let Some(tax_id) = client.id_tax {
        state
            .db
            .find_tax_by_id(Some(tax_id))
            .await
            .map_err(|e| ApiError::DatabaseError(e.to_string()))?
            .map(|t| t.iva)
            .unwrap_or(default_rate)
    } else {
        default_rate
    };

    let exchange_rate = state
        .db
        .get_latest_exchange_rate()
        .await
        .map_err(|_| ApiError::InternalServerError)?;
    let amount_bs_val = amount_bs.unwrap();
    let amount_bs_neto = amount_bs_val / iva_rate;
    let amount_usd = (amount_bs_neto / exchange_rate * 100.0).round() / 100.0;

    let new_report = PaymentReport {
        id: None,
        id_client: Some(real_client_id),
        id_debt: id_debt_oid,
        id_payment_method: Some(id_pm_oid),
        reference: reference.unwrap(),
        payment_date: date_str
            .and_then(|d| d.parse::<DateTime<Utc>>().ok())
            .unwrap_or(Utc::now()),
        amount_bs: amount_bs_val,
        bank_origin: bank.unwrap_or_default(),
        phone_number: phone.unwrap_or_default(),
        image_url: saved_image_path.unwrap(),
        amount_usd,
        exchange_rate,
        state: "Pendiente".to_string(),
        rejection_reason: None,
        id_creator: None,
        id_issuing_bank: None,
        created_at: Utc::now(),
    };

    let result = state
        .db
        .create_payment_report(new_report)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": if id_debt_oid.is_some() { "Pago a deuda registrado" } else { "Abono a cuenta registrado" },
        "data": {
            "id": result.inserted_id,
            "amount_usd": amount_usd,
            "is_advance": id_debt_oid.is_none()
        }
    })))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/payments/report",
    tag = "Payments",
    security(("bearerAuth" = [])),
    request_body(
        content = String,
        content_type = "multipart/form-data",
        description = "Mismos campos que `/v1/payments/payment/report` pero desde el dashboard staff."
    ),
    responses(
        (status = 200, description = "Reporte de pago registrado en estado Pendiente"),
        (status = 400, description = "Faltan datos básicos o IDs inválidos"),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn report_payment_user_handler(
    Extension(_claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    tracing::info!("📸 Iniciando reporte de pago (Abono o Deuda)");

    let mut reference = None;
    let mut date_str = None;
    let mut amount_bs = None;
    let mut bank = None;
    let mut phone = None;
    let mut saved_image_path = None;
    let mut id_debt_str: Option<String> = None;
    let mut id_client_str: Option<String> = None;
    let mut id_payment_method_str: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        tracing::error!("Error multipart: {}", e);
        ApiError::BadRequest("Error leyendo formulario".into())
    })? {
        let name = field.name().unwrap_or("").to_string();

        if name == "image" {
            let content_type = field.content_type().unwrap_or("image/jpeg").to_string();
            let extension = match content_type.as_str() {
                "image/png" => "png",
                "image/webp" => "webp",
                "image/gif" => "gif",
                _ => "jpg",
            };
            let unique_name = format!("{}.{}", Uuid::new_v4(), extension);
            let file_path = format!("uploads/{}", unique_name);

            let data = field
                .bytes()
                .await
                .map_err(|_| ApiError::InternalServerError)?;

            if data.is_empty() {
                tracing::error!("Imagen recibida esta vacia (0 bytes)");
                return Err(ApiError::BadRequest(
                    "La imagen llego vacia al servidor".into(),
                ));
            }

            tracing::info!(
                "Imagen recibida: {} bytes, tipo: {}",
                data.len(),
                content_type
            );

            let mut file = File::create(&file_path)
                .await
                .map_err(|_| ApiError::InternalServerError)?;
            file.write_all(&data)
                .await
                .map_err(|_| ApiError::InternalServerError)?;
            file.flush()
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
                "id_client" => id_client_str = Some(text),
                "id_payment_method" => id_payment_method_str = Some(text),
                _ => {}
            }
        }
    }

    if reference.is_none()
        || amount_bs.is_none()
        || saved_image_path.is_none()
        || id_payment_method_str.is_none()
    {
        return Err(ApiError::BadRequest(
            "Faltan datos básicos (ref, monto, imagen o método)".into(),
        ));
    }

    if id_debt_str.is_none() && id_client_str.is_none() {
        return Err(ApiError::BadRequest(
            "Debe especificar una Deuda o un Cliente para el abono".into(),
        ));
    }

    let id_pm_oid = ObjectId::parse_str(&id_payment_method_str.unwrap())
        .map_err(|_| ApiError::BadRequest("Método de pago inválido".into()))?;

    let mut id_debt_oid: Option<ObjectId> = None;
    let real_client_id: ObjectId;

    if let Some(d_str) = id_debt_str {
        let oid = ObjectId::parse_str(&d_str)
            .map_err(|_| ApiError::BadRequest("ID deuda malformado".into()))?;
        let debt_doc = state
            .db
            .find_debt_by_id(&oid.to_string())
            .await
            .map_err(|e| ApiError::DatabaseError(e))?
            .ok_or(ApiError::BadRequest("La deuda no existe".into()))?;

        real_client_id = debt_doc.id_client;
        id_debt_oid = Some(oid);
    } else {
        let c_str = id_client_str.unwrap();
        real_client_id = ObjectId::parse_str(&c_str)
            .map_err(|_| ApiError::BadRequest("ID cliente malformado".into()))?;
    }

    let client = state
        .db
        .find_client_by_id(&real_client_id.to_string())
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let default_rate = 1.0;

    let iva_rate = if let Some(tax_id) = client.id_tax {
        state
            .db
            .find_tax_by_id(Some(tax_id))
            .await
            .map_err(|e| ApiError::DatabaseError(e.to_string()))?
            .map(|t| t.iva)
            .unwrap_or(default_rate)
    } else {
        default_rate
    };

    let exchange_rate = state
        .db
        .get_latest_exchange_rate()
        .await
        .map_err(|_| ApiError::InternalServerError)?;
    let amount_bs_val = amount_bs.unwrap();
    let amount_bs_neto = amount_bs_val / iva_rate;
    let amount_usd = (amount_bs_neto / exchange_rate * 100.0).round() / 100.0;

    let new_report = PaymentReport {
        id: None,
        id_client: Some(real_client_id),
        id_debt: id_debt_oid,
        id_payment_method: Some(id_pm_oid),
        reference: reference.unwrap(),
        payment_date: date_str
            .and_then(|d| d.parse::<DateTime<Utc>>().ok())
            .unwrap_or(Utc::now()),
        amount_bs: amount_bs_val,
        bank_origin: bank.unwrap_or_default(),
        phone_number: phone.unwrap_or_default(),
        image_url: saved_image_path.unwrap(),
        amount_usd,
        exchange_rate,
        state: "Pendiente".to_string(),
        rejection_reason: None,
        id_creator: None,
        id_issuing_bank: None,
        created_at: Utc::now(),
    };

    let result = state
        .db
        .create_payment_report(new_report)
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": if id_debt_oid.is_some() { "Pago a deuda registrado" } else { "Abono a cuenta registrado" },
        "data": {
            "id": result.inserted_id,
            "amount_usd": amount_usd,
            "is_advance": id_debt_oid.is_none()
        }
    })))
}
