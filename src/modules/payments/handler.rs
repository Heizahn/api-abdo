use axum::{
    extract::{Multipart, Path as AxumPath, Query, State},
    response::IntoResponse,
    Extension, Json,
};
use serde::Deserialize;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::{
    auth::claims::AccessClaims,
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, SalesRepository, UserRepository},
    error::ApiError,
    models::db::{
        PaymentHistoryFilters, PaymentHistoryListResponse, PaymentHistoryPageResponse,
        PaymentHistoryPaymentType, PaymentHistorySortBy, PaymentHistorySortDir, PaymentReportFull,
        TaxListResponse,
    },
    models::payment::{PagoMovilData, PaymentMethodResponse, PaymentReport},
    modules::payments::service::{PaymentInput, PaymentsService},
    modules::whatsapp::ws::{broadcast_to_roles, ReportePagoPendienteData, WsServerEvent},
    state::AppState,
};

// nRole values eligible for payments-reports endpoints.
// 0.0 = superadmin, 1.0 = contador, 1.5 = contador-mensajero.
// Float equality is safe: these are exact sums of powers of 2.
const REPORT_ROLES: &[f32] = &[0.0_f32, 1.0_f32, 1.5_f32];
const DEFAULT_PAYMENT_REPORT_MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024; // 20MB

/// Returns true if the role is authorised to manage payment reports.
#[inline]
fn has_report_access(role: Option<f32>) -> bool {
    match role {
        Some(r) => REPORT_ROLES.contains(&r),
        None => false,
    }
}

#[derive(Debug, Deserialize)]
pub struct PaymentHistoryQuery {
    pub owner: Option<String>,
    #[serde(rename = "idOwner")]
    pub id_owner: Option<String>,
    pub reference: Option<String>,
    pub search: Option<String>,
    pub q: Option<String>,
    pub client: Option<String>,
    pub reason: Option<String>,
    pub commentary: Option<String>,
    pub state: Option<String>,
    pub creator: Option<String>,
    pub editor: Option<String>,
    #[serde(rename = "type")]
    pub payment_type: Option<String>,
    pub created_from: Option<String>,
    pub created_to: Option<String>,
    pub amount_min: Option<String>,
    pub amount_max: Option<String>,
    pub amount_bs_min: Option<String>,
    pub amount_bs_max: Option<String>,
    pub sort_by: Option<String>,
    pub sort_dir: Option<String>,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

impl PaymentHistoryQuery {
    fn owner_filter(&self) -> Option<&str> {
        self.owner
            .as_deref()
            .or(self.id_owner.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    fn reference_filter(&self) -> Option<&str> {
        self.reference
            .as_deref()
            .or(self.search.as_deref())
            .or(self.q.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    fn complete_filters(&self) -> Result<PaymentHistoryFilters, ApiError> {
        let page = self.page.unwrap_or(1).max(1);
        let per_page = self.per_page.unwrap_or(500).clamp(1, 500);
        let amount_min = parse_optional_amount(&self.amount_min, "amount_min")?;
        let amount_max = parse_optional_amount(&self.amount_max, "amount_max")?;
        let amount_bs_min = parse_optional_amount(&self.amount_bs_min, "amount_bs_min")?;
        let amount_bs_max = parse_optional_amount(&self.amount_bs_max, "amount_bs_max")?;
        let created_from = parse_optional_datetime(&self.created_from, "created_from")?;
        let created_to = parse_optional_datetime(&self.created_to, "created_to")?;

        validate_range(amount_min, amount_max, "amount")?;
        validate_range(amount_bs_min, amount_bs_max, "amount_bs")?;
        validate_datetime_range(created_from, created_to)?;

        Ok(PaymentHistoryFilters {
            reference: trimmed_owned(&self.reference),
            search: trimmed_owned(&self.search),
            client: trimmed_owned(&self.client),
            reason: trimmed_owned(&self.reason),
            commentary: trimmed_owned(&self.commentary),
            state: trimmed_owned(&self.state).map(|state| normalize_payment_state(&state)),
            creator: trimmed_owned(&self.creator),
            editor: trimmed_owned(&self.editor),
            payment_type: parse_payment_type(&self.payment_type)?,
            created_from,
            created_to,
            amount_min,
            amount_max,
            amount_bs_min,
            amount_bs_max,
            sort_by: parse_sort_by(&self.sort_by)?,
            sort_dir: parse_sort_dir(&self.sort_dir)?,
            page,
            per_page,
        })
    }
}

fn trimmed_owned(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn bad_payment_history_query(field: &str, message: &str) -> ApiError {
    ApiError::BadRequest(format!("{}: {}", field, message))
}

fn parse_payment_type(
    value: &Option<String>,
) -> Result<Option<PaymentHistoryPaymentType>, ApiError> {
    let Some(value) = trimmed_owned(value) else {
        return Ok(None);
    };

    match value.to_ascii_lowercase().as_str() {
        "cash" => Ok(Some(PaymentHistoryPaymentType::Cash)),
        "usd" => Ok(Some(PaymentHistoryPaymentType::Usd)),
        "mobile" => Ok(Some(PaymentHistoryPaymentType::Mobile)),
        _ => Err(bad_payment_history_query(
            "type",
            "valor inválido, use cash, usd o mobile",
        )),
    }
}

fn parse_sort_by(value: &Option<String>) -> Result<PaymentHistorySortBy, ApiError> {
    let Some(value) = trimmed_owned(value) else {
        return Ok(PaymentHistorySortBy::CreatedAt);
    };

    match value.to_ascii_lowercase().as_str() {
        "created_at" => Ok(PaymentHistorySortBy::CreatedAt),
        "client" => Ok(PaymentHistorySortBy::Client),
        "reason" => Ok(PaymentHistorySortBy::Reason),
        "state" => Ok(PaymentHistorySortBy::State),
        "creator" => Ok(PaymentHistorySortBy::Creator),
        "editor" => Ok(PaymentHistorySortBy::Editor),
        "amount" => Ok(PaymentHistorySortBy::Amount),
        "amount_bs" => Ok(PaymentHistorySortBy::AmountBs),
        "reference" => Ok(PaymentHistorySortBy::Reference),
        _ => Err(bad_payment_history_query(
            "sort_by",
            "valor inválido para ordenamiento",
        )),
    }
}

fn parse_sort_dir(value: &Option<String>) -> Result<PaymentHistorySortDir, ApiError> {
    let Some(value) = trimmed_owned(value) else {
        return Ok(PaymentHistorySortDir::Desc);
    };

    match value.to_ascii_lowercase().as_str() {
        "asc" => Ok(PaymentHistorySortDir::Asc),
        "desc" => Ok(PaymentHistorySortDir::Desc),
        _ => Err(bad_payment_history_query(
            "sort_dir",
            "valor inválido, use asc o desc",
        )),
    }
}

fn parse_optional_datetime(
    value: &Option<String>,
    field: &str,
) -> Result<Option<DateTime<Utc>>, ApiError> {
    let Some(value) = trimmed_owned(value) else {
        return Ok(None);
    };

    DateTime::parse_from_rfc3339(&value)
        .map(|dt| Some(dt.with_timezone(&Utc)))
        .map_err(|_| bad_payment_history_query(field, "fecha ISO inválida"))
}

fn parse_optional_amount(value: &Option<String>, field: &str) -> Result<Option<f64>, ApiError> {
    let Some(value) = trimmed_owned(value) else {
        return Ok(None);
    };

    let amount = value
        .parse::<f64>()
        .map_err(|_| bad_payment_history_query(field, "monto inválido"))?;
    if !amount.is_finite() {
        return Err(bad_payment_history_query(field, "monto inválido"));
    }

    Ok(Some(amount))
}

fn validate_range(min: Option<f64>, max: Option<f64>, field: &str) -> Result<(), ApiError> {
    if let (Some(min), Some(max)) = (min, max) {
        if min > max {
            return Err(bad_payment_history_query(
                field,
                "el mínimo no puede ser mayor que el máximo",
            ));
        }
    }

    Ok(())
}

fn validate_datetime_range(
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<(), ApiError> {
    if let (Some(from), Some(to)) = (from, to) {
        if from > to {
            return Err(bad_payment_history_query(
                "created_at",
                "created_from no puede ser mayor que created_to",
            ));
        }
    }

    Ok(())
}

fn normalize_payment_state(state: &str) -> String {
    match state.trim().to_lowercase().as_str() {
        "activo" => "Activo".to_string(),
        "anulado" => "Anulado".to_string(),
        "pendiente" => "Pendiente".to_string(),
        "rechazado" => "Rechazado".to_string(),
        normalized => {
            let mut chars = normalized.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

async fn resolve_payments_owner_scope(
    state: &Arc<AppState>,
    claims: &UserProfileClaims,
    owner_param: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let caller = state
        .db
        .find_user_by_id(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    if (caller.role + 1.0_f32).abs() < 0.01 {
        return Err(ApiError::Forbidden);
    }

    let caller_is_provider = (caller.role - 3.0_f32).abs() < 0.01;
    if caller_is_provider {
        if let Some(requested_owner) = owner_param {
            if requested_owner != claims.id {
                return Err(ApiError::Forbidden);
            }
        }
        return Ok(Some(claims.id.clone()));
    }

    let Some(requested_owner) = owner_param else {
        return Ok(None);
    };

    let owner_user = state
        .db
        .find_user_by_id(requested_owner)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::Forbidden)?;

    if (owner_user.role - 3.0_f32).abs() >= 0.01 {
        return Err(ApiError::Forbidden);
    }

    Ok(Some(requested_owner.to_string()))
}

#[inline]
fn payment_report_max_image_bytes() -> usize {
    std::env::var("PAYMENT_REPORT_MAX_IMAGE_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_PAYMENT_REPORT_MAX_IMAGE_BYTES)
}

async fn persist_payment_report_image(
    mut field: axum::extract::multipart::Field<'_>,
) -> Result<String, ApiError> {
    let content_type = field.content_type().unwrap_or("image/jpeg").to_string();
    let extension = match content_type.as_str() {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "jpg",
    };

    let unique_name = format!("{}.{}", Uuid::new_v4(), extension);
    let file_path = format!("uploads/{}", unique_name);
    let mut file = File::create(&file_path)
        .await
        .map_err(|_| ApiError::InternalServerError)?;

    let max_bytes = payment_report_max_image_bytes();
    let mut total_bytes = 0usize;

    loop {
        let chunk = field
            .chunk()
            .await
            .map_err(|_| ApiError::BadRequest("Error leyendo imagen".into()))?;
        let Some(chunk) = chunk else { break };

        total_bytes = total_bytes.saturating_add(chunk.len());
        if total_bytes > max_bytes {
            let _ = tokio::fs::remove_file(&file_path).await;
            return Err(ApiError::domain_with_details(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "image_too_large",
                "La imagen supera el tamaño máximo permitido",
                serde_json::json!({
                    "max_bytes": max_bytes,
                    "received_bytes": total_bytes
                }),
            ));
        }

        file.write_all(&chunk)
            .await
            .map_err(|_| ApiError::InternalServerError)?;
    }

    if total_bytes == 0 {
        let _ = tokio::fs::remove_file(&file_path).await;
        return Err(ApiError::BadRequest(
            "La imagen llego vacia al servidor".into(),
        ));
    }

    file.flush()
        .await
        .map_err(|_| ApiError::InternalServerError)?;

    tracing::info!(
        "Imagen recibida: {} bytes, tipo: {}",
        total_bytes,
        content_type
    );

    Ok(format!("/uploads/{}", unique_name))
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
            saved_image_path = Some(persist_payment_report_image(field).await?);
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
            saved_image_path = Some(persist_payment_report_image(field).await?);
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
// Payment history — list/simple + list/complete
// ============================================================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/payments/list/simple",
    tag = "Payments",
    security(("bearerAuth" = [])),
    params(
        ("owner" = Option<String>, Query, description = "Filtrar por provider/owner permitido"),
        ("idOwner" = Option<String>, Query, description = "Alias legacy de owner"),
        ("reference" = Option<String>, Query, description = "Referencia exacta del pago"),
        ("search" = Option<String>, Query, description = "Alias de reference para búsqueda rápida por referencia"),
    ),
    responses(
        (status = 200, description = "Últimos pagos para historial simple", body = PaymentHistoryListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Owner no permitido para este usuario"),
    )
)]
pub async fn list_payments_simple_handler(
    Extension(claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<PaymentHistoryQuery>,
) -> Result<Json<PaymentHistoryListResponse>, ApiError> {
    let owner_id = resolve_payments_owner_scope(&state, &claims, params.owner_filter()).await?;
    let payments = state
        .db
        .list_payments_simple(owner_id.as_deref(), params.reference_filter())
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(PaymentHistoryListResponse {
        ok: true,
        data: payments,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/payments/list/complete",
    tag = "Payments",
    security(("bearerAuth" = [])),
    params(
        ("owner" = Option<String>, Query, description = "Filtrar por provider/owner permitido"),
        ("idOwner" = Option<String>, Query, description = "Alias legacy de owner"),
        ("reference" = Option<String>, Query, description = "Referencia parcial case-insensitive"),
        ("search" = Option<String>, Query, description = "Búsqueda global parcial case-insensitive"),
        ("client" = Option<String>, Query, description = "Cliente parcial case-insensitive"),
        ("reason" = Option<String>, Query, description = "Motivo parcial case-insensitive"),
        ("commentary" = Option<String>, Query, description = "Comentario parcial case-insensitive"),
        ("state" = Option<String>, Query, description = "Estado exacto normalizado"),
        ("creator" = Option<String>, Query, description = "Operador parcial case-insensitive"),
        ("editor" = Option<String>, Query, description = "Editor parcial case-insensitive"),
        ("type" = Option<String>, Query, description = "Tipo de pago: cash, usd o mobile"),
        ("created_from" = Option<String>, Query, description = "Fecha inicial ISO inclusiva"),
        ("created_to" = Option<String>, Query, description = "Fecha final ISO inclusiva"),
        ("amount_min" = Option<String>, Query, description = "Monto USD mínimo"),
        ("amount_max" = Option<String>, Query, description = "Monto USD máximo"),
        ("amount_bs_min" = Option<String>, Query, description = "Monto VES mínimo"),
        ("amount_bs_max" = Option<String>, Query, description = "Monto VES máximo"),
        ("sort_by" = Option<String>, Query, description = "created_at, client, reason, state, creator, editor, amount, amount_bs o reference"),
        ("sort_dir" = Option<String>, Query, description = "asc o desc"),
        ("page" = Option<u32>, Query, description = "Página, inicia en 1. Default 1"),
        ("per_page" = Option<u32>, Query, description = "Tamaño de página. Default 500, máximo 500"),
    ),
    responses(
        (status = 200, description = "Historial completo paginado de pagos", body = PaymentHistoryPageResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Owner no permitido para este usuario"),
    )
)]
pub async fn list_payments_complete_handler(
    Extension(claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<PaymentHistoryQuery>,
) -> Result<Json<PaymentHistoryPageResponse>, ApiError> {
    let owner_id = resolve_payments_owner_scope(&state, &claims, params.owner_filter()).await?;
    let filters = params.complete_filters()?;

    let payments = state
        .db
        .list_payments_complete(owner_id.as_deref(), filters)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(PaymentHistoryPageResponse {
        ok: true,
        data: payments,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/payments/iva/list",
    tag = "Payments",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Lista de tasas IVA configuradas", body = TaxListResponse),
        (status = 401, description = "No autorizado"),
    )
)]
pub async fn list_payment_iva_handler(
    Extension(_claims): Extension<UserProfileClaims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<TaxListResponse>, ApiError> {
    let taxes = state
        .db
        .list_taxes()
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(TaxListResponse {
        ok: true,
        data: taxes,
    }))
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

    // 2. Acquire lock de aprobación para evitar carreras.
    let lock_token = Uuid::new_v4().to_string();
    let report: PaymentReportFull = state
        .db
        .acquire_report_approval_lock(report_oid, &lock_token, 120_000)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            ApiError::domain_simple(
                axum::http::StatusCode::CONFLICT,
                "report_locked_or_not_found",
                "El reporte está siendo procesado por otro usuario o no existe",
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

    let process_result: Result<&str, ApiError> = if let Some(matched_payment) = matched {
        // MATCH — check whether the payment is already linked to a report
        let already_linked = matched_payment.id_payment_report.is_some();

        if !already_linked {
            state
                .db
                .update_payment_link(matched_payment._id, report_oid, report.id_payment_method)
                .await
                .map_err(ApiError::DatabaseError)?;
        }
        Ok("Reporte marcado como verificado (el pago ya existía en sistema)")
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

        Ok("Reporte aprobado y nuevo pago creado exitosamente")
    };

    let message = match process_result {
        Ok(m) => m,
        Err(e) => {
            let _ = state
                .db
                .release_report_approval_lock(report_oid, &lock_token)
                .await;
            return Err(e);
        }
    };

    // 6. Transition report → Verificado (validando ownership del lock)
    let changed = state
        .db
        .finalize_report_approval(report_oid, &lock_token, &claims.id)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !changed {
        let _ = state
            .db
            .release_report_approval_lock(report_oid, &lock_token)
            .await;
        return Err(ApiError::domain_simple(
            axum::http::StatusCode::CONFLICT,
            "report_lock_lost",
            "Se perdió el lock de aprobación. Reintentá.",
        ));
    }

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
