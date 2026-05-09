use axum::{
    extract::{Multipart, Path as AxumPath, State},
    response::IntoResponse,
    Extension, Json,
};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::{
    auth::claims::AccessClaims,
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, SalesRepository},
    error::ApiError,
    models::db::PaymentReportFull,
    models::payment::{PagoMovilData, PaymentMethodResponse, PaymentReport},
    modules::payments::service::{PaymentInput, PaymentsService},
    modules::whatsapp::ws::{broadcast_to_roles, ReportePagoPendienteData, WsServerEvent},
    state::AppState,
};

// nRole values eligible for payments-reports endpoints.
// 0.0 = superadmin, 1.0 = contador, 1.5 = contador-mensajero.
// Float equality is safe: these are exact sums of powers of 2.
const REPORT_ROLES: &[f32] = &[0.0_f32, 1.0_f32, 1.5_f32];

/// Returns true if the role is authorised to manage payment reports.
#[inline]
fn has_report_access(role: Option<f32>) -> bool {
    match role {
        Some(r) => REPORT_ROLES.contains(&r),
        None => false,
    }
}

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

    let created_id = result
        .inserted_id
        .as_object_id()
        .map(|o| o.to_hex())
        .unwrap_or_default();

    // EMIT BADGE: REPORTE_PAGO_PENDIENTE
    let pending_total = state.db.count_pending_reports().await.unwrap_or(0);
    let badge_event = WsServerEvent::ReportePagoPendiente {
        data: ReportePagoPendienteData {
            pending_total,
            report_id: created_id.clone(),
            previous_state: None,
            new_state: "Pendiente".to_string(),
        },
    };
    if let Ok(payload) = serde_json::to_string(&badge_event) {
        let _ = broadcast_to_roles(&state, REPORT_ROLES, payload).await;
    }

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

    let created_id = result
        .inserted_id
        .as_object_id()
        .map(|o| o.to_hex())
        .unwrap_or_default();

    // EMIT BADGE: REPORTE_PAGO_PENDIENTE
    let pending_total = state.db.count_pending_reports().await.unwrap_or(0);
    let badge_event = WsServerEvent::ReportePagoPendiente {
        data: ReportePagoPendienteData {
            pending_total,
            report_id: created_id.clone(),
            previous_state: None,
            new_state: "Pendiente".to_string(),
        },
    };
    if let Ok(payload) = serde_json::to_string(&badge_event) {
        let _ = broadcast_to_roles(&state, REPORT_ROLES, payload).await;
    }

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

// ============================================================================
// T20 — list_payment_reports_handler
// ============================================================================

/// Lista los reportes de pago pendientes (y los de los últimos 2 meses).
/// Solo accesible por roles 0 (superadmin), 1 (contador), 1.5 (contador-mensajero).
#[utoipa::path(
    get,
    path = "/v1/auth-user/payments-reports",
    tag = "Payments",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Lista de reportes de pago", body = Vec<PaymentReportListItem>),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Rol no autorizado — solo 0, 1, 1.5"),
    )
)]
pub async fn list_payment_reports_handler(
    Extension(claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !has_report_access(claims.role) {
        return Err(ApiError::Forbidden);
    }

    let reports = state
        .db
        .list_payment_reports()
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(serde_json::json!({ "ok": true, "data": reports })))
}

// ============================================================================
// T21 — approve_payment_report_handler
// ============================================================================

/// Aprueba un reporte de pago (Pendiente/Rechazado → Verificado).
///
/// Realiza fuzzy-match bidireccional por `sReference` contra los pagos activos
/// del cliente. Si hay coincidencia, vincula el pago; si no, crea uno nuevo vía
/// `PaymentsService::create_payment`. Emite `REPORTE_PAGO_PENDIENTE`.
#[utoipa::path(
    post,
    path = "/v1/auth-user/payments-reports/{id}/approve",
    tag = "Payments",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId del reporte de pago")),
    responses(
        (status = 200, description = "Reporte aprobado"),
        (status = 400, description = "ID inválido o reporte ya verificado"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Rol no autorizado"),
        (status = 404, description = "Reporte no encontrado"),
    )
)]
pub async fn approve_payment_report_handler(
    Extension(claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !has_report_access(claims.role) {
        return Err(ApiError::Forbidden);
    }

    // 1. Parse path param
    let report_oid = ObjectId::parse_str(&id).map_err(|_| {
        ApiError::domain_simple(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_id",
            "ID de reporte inválido",
        )
    })?;

    // 2. Fetch report
    let report: PaymentReportFull = state
        .db
        .find_report_by_id(report_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                axum::http::StatusCode::NOT_FOUND,
                "report_not_found",
                "Reporte de pago no encontrado",
            )
        })?;

    // 3. Guard: already Verificado → 400
    if report.state == "Verificado" {
        return Err(ApiError::domain_simple(
            axum::http::StatusCode::BAD_REQUEST,
            "already_verified",
            "El reporte ya fue verificado",
        ));
    }

    let previous_state = report.state.clone();

    // 4. Require id_client
    let client_id = report.id_client.ok_or_else(|| {
        ApiError::domain_simple(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_report",
            "El reporte no tiene cliente asociado",
        )
    })?;

    // 5. Bidirectional fuzzy match on sReference (scoped to this client)
    let candidates = state
        .db
        .find_payments_for_match_by_client(client_id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let report_ref = report.reference.trim().to_string();

    let matched = candidates.into_iter().find(|p| {
        let payment_ref = p.s_reference.trim().to_string();
        !payment_ref.is_empty()
            && !report_ref.is_empty()
            && (payment_ref.ends_with(&report_ref) || report_ref.ends_with(&payment_ref))
    });

    let message: &str = if let Some(matched_payment) = matched {
        // MATCH — check whether the payment is already linked to a report
        let already_linked = matched_payment.id_payment_report.is_some();

        if !already_linked {
            state
                .db
                .update_payment_link(matched_payment._id, report_oid, report.id_payment_method)
                .await
                .map_err(ApiError::DatabaseError)?;
        }
        "Reporte marcado como verificado (el pago ya existía en sistema)"
    } else {
        // NO MATCH — create a new payment
        let now_iso = BsonDateTime::now().to_string();
        let d_creation = {
            let pd = report.payment_date.trim().to_string();
            if pd.is_empty() {
                now_iso
            } else {
                pd
            }
        };

        let commentary = format!(
            "Reporte aprobado. Banco: {}, Tel: {}",
            if report.bank_origin.is_empty() {
                "N/A"
            } else {
                &report.bank_origin
            },
            if report.phone_number.is_empty() {
                "N/A"
            } else {
                &report.phone_number
            },
        );

        let payment_input = PaymentInput {
            id_client: client_id,
            s_reference: report.reference.clone(),
            n_bs: report.amount_bs,
            n_amount: report.amount_usd,
            b_usd: false,
            b_cash: false,
            id_payment_method: report.id_payment_method,
            id_payment_report: Some(report_oid),
            id_creator: claims.id.clone(),
            d_creation: Some(d_creation),
            s_commentary: Some(commentary),
        };

        let svc = PaymentsService::new(state.db.clone());
        svc.create_payment(payment_input, report.id_debt).await?;

        "Reporte aprobado y nuevo pago creado exitosamente"
    };

    // 6. Transition report → Verificado
    state
        .db
        .update_report_state(report_oid, "Verificado", &claims.id, None)
        .await
        .map_err(ApiError::DatabaseError)?;

    // 7. Count pending + emit REPORTE_PAGO_PENDIENTE
    let pending_total = state.db.count_pending_reports().await.unwrap_or(0);

    let ws_payload = serde_json::to_string(&WsServerEvent::ReportePagoPendiente {
        data: ReportePagoPendienteData {
            pending_total,
            report_id: report_oid.to_hex(),
            previous_state: Some(previous_state),
            new_state: "Verificado".to_string(),
        },
    })
    .unwrap_or_default();

    // EMIT BADGE: REPORTE_PAGO_PENDIENTE
    let _ = broadcast_to_roles(&state, REPORT_ROLES, ws_payload).await;

    Ok(Json(
        serde_json::json!({ "ok": true, "data": { "message": message } }),
    ))
}

// ============================================================================
// T22 — reject_payment_report_handler
// ============================================================================

/// Request body para rechazar un reporte de pago.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct RejectReportRequest {
    /// Motivo del rechazo. Requerido, no puede estar vacío.
    pub reason: Option<String>,
}

/// Rechaza un reporte de pago (Pendiente → Rechazado).
///
/// Requiere body `{ "reason": "..." }` no vacío.
/// Solo permite la transición desde `Pendiente`.
/// Emite `REPORTE_PAGO_PENDIENTE` a roles {0, 1, 1.5}.
#[utoipa::path(
    post,
    path = "/v1/auth-user/payments-reports/{id}/reject",
    tag = "Payments",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId del reporte de pago")),
    request_body = RejectReportRequest,
    responses(
        (status = 200, description = "Reporte rechazado"),
        (status = 400, description = "ID inválido o estado no permite rechazo"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Rol no autorizado"),
        (status = 404, description = "Reporte no encontrado"),
        (status = 422, description = "Falta el motivo del rechazo"),
    )
)]
pub async fn reject_payment_report_handler(
    Extension(claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<RejectReportRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !has_report_access(claims.role) {
        return Err(ApiError::Forbidden);
    }

    // Validate reason first (cheap — before any DB call)
    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ApiError::domain_simple(
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "missing_reason",
                "El motivo del rechazo es requerido",
            )
        })?;

    // 1. Parse path param
    let report_oid = ObjectId::parse_str(&id).map_err(|_| {
        ApiError::domain_simple(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_id",
            "ID de reporte inválido",
        )
    })?;

    // 2. Fetch report
    let report: PaymentReportFull = state
        .db
        .find_report_by_id(report_oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                axum::http::StatusCode::NOT_FOUND,
                "report_not_found",
                "Reporte de pago no encontrado",
            )
        })?;

    // 3. Guard: only Pendiente can be rejected
    if report.state != "Pendiente" {
        return Err(ApiError::domain_simple(
            axum::http::StatusCode::BAD_REQUEST,
            "only_pending_can_be_rejected",
            "Solo los reportes en estado Pendiente pueden ser rechazados",
        ));
    }

    let previous_state = report.state.clone();

    // 4. Transition report → Rechazado
    state
        .db
        .update_report_state(report_oid, "Rechazado", &claims.id, Some(&reason))
        .await
        .map_err(ApiError::DatabaseError)?;

    // 5. Count pending + emit REPORTE_PAGO_PENDIENTE
    let pending_total = state.db.count_pending_reports().await.unwrap_or(0);

    let ws_payload = serde_json::to_string(&WsServerEvent::ReportePagoPendiente {
        data: ReportePagoPendienteData {
            pending_total,
            report_id: report_oid.to_hex(),
            previous_state: Some(previous_state),
            new_state: "Rechazado".to_string(),
        },
    })
    .unwrap_or_default();

    // EMIT BADGE: REPORTE_PAGO_PENDIENTE
    let _ = broadcast_to_roles(&state, REPORT_ROLES, ws_payload).await;

    Ok(Json(
        serde_json::json!({ "ok": true, "data": { "message": "Reporte rechazado" } }),
    ))
}
