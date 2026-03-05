use axum::{extract::{Extension, Query, State}, Json};
use chrono::{Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, SalesRepository, UserRepository},
    error::ApiError,
    models::db::{LatestPayment, SolvencyCounts},
    state::AppState,
    utils::timezone::VENEZUELA_TZ,
};

#[derive(Deserialize)]
pub struct DashboardQuery {
    pub owner: Option<String>,
}

#[derive(Deserialize)]
pub struct MonthlyClosingQuery {
    pub month: Option<String>,
    pub owner: Option<String>,
}

/// Determina el idOwner efectivo según el rol del usuario autenticado.
/// Si es provider (nRole == 3): siempre usa su propio ID del JWT.
/// Si es superadmin u otro: usa el parámetro `owner` si se proporcionó.
async fn resolve_owner_id(
    state: &Arc<AppState>,
    claims: &UserProfileClaims,
    owner_param: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let user = state
        .db
        .find_user_by_id(&claims.id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| ApiError::Unauthorized("Usuario no encontrado".to_string()))?;

    if (user.role - 3.0_f32).abs() < 0.01 {
        // Provider/Owner: siempre filtra por su propio ID
        Ok(Some(claims.id.clone()))
    } else {
        // Superadmin u otro rol: usa el parámetro opcional
        Ok(owner_param.map(|s| s.to_string()))
    }
}

#[derive(Serialize)]
pub struct MonthlyClosingResponse {
    pub months: Vec<String>,
    pub selected_month: String,
    pub data: MonthlyClosingData,
}

#[derive(Serialize)]
pub struct MonthlyClosingData {
    pub collected: f64,
    pub pending: f64,
    pub efficiency: Option<f64>,
}

/// GET /v1/auth-user/dashboard/latest-payments?owner=<id>
pub async fn latest_payments_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(params): Query<DashboardQuery>,
) -> Result<Json<Vec<LatestPayment>>, ApiError> {
    let owner_id = resolve_owner_id(&state, &claims, params.owner.as_deref()).await?;
    state
        .db
        .get_latest_payments(10, owner_id.as_deref())
        .await
        .map(Json)
        .map_err(ApiError::DatabaseError)
}

/// GET /v1/auth-user/dashboard/solvency?owner=<id>
pub async fn solvency_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(params): Query<DashboardQuery>,
) -> Result<Json<SolvencyCounts>, ApiError> {
    let owner_id = resolve_owner_id(&state, &claims, params.owner.as_deref()).await?;
    state
        .db
        .get_solvency_counts(owner_id.as_deref())
        .await
        .map(Json)
        .map_err(ApiError::DatabaseError)
}

/// GET /v1/auth-user/dashboard/monthly-closing?month=YYYY-MM&owner=<id>
pub async fn monthly_closing_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(params): Query<MonthlyClosingQuery>,
) -> Result<Json<MonthlyClosingResponse>, ApiError> {
    let owner_id = resolve_owner_id(&state, &claims, params.owner.as_deref()).await?;
    let now_vz = Utc::now().with_timezone(&VENEZUELA_TZ);
    let current_year = now_vz.year();
    let current_month = now_vz.month();
    let current_idx = month_to_index(current_year, current_month);

    // Parsear mes solicitado o usar el actual
    let (selected_year, selected_month) = match &params.month {
        Some(s) => parse_year_month(s)
            .ok_or_else(|| ApiError::BadRequest("Formato de mes inválido, use YYYY-MM".into()))?,
        None => (current_year, current_month),
    };

    // Validar que no sea un mes futuro
    let selected_idx = month_to_index(selected_year, selected_month);
    if selected_idx > current_idx {
        return Err(ApiError::BadRequest(
            "El mes seleccionado no puede ser mayor al mes actual".into(),
        ));
    }

    let is_current_month = selected_idx == current_idx;

    // Rango del mes seleccionado en Venezuela (primer y último día, 00:00:00 / 23:59:59)
    let last_day = days_in_month(selected_year, selected_month);

    let start_utc = VENEZUELA_TZ
        .with_ymd_and_hms(selected_year, selected_month, 1, 0, 0, 0)
        .single()
        .ok_or(ApiError::InternalServerError)?
        .with_timezone(&Utc);

    let end_utc = VENEZUELA_TZ
        .with_ymd_and_hms(selected_year, selected_month, last_day, 23, 59, 59)
        .single()
        .ok_or(ApiError::InternalServerError)?
        .with_timezone(&Utc);

    // Obtener todos los clientes activos con su balance
    let active_clients = state
        .db
        .find_active_clients_for_closing(owner_id.as_deref())
        .await
        .map_err(ApiError::DatabaseError)?;

    let client_ids: Vec<_> = active_clients.iter().map(|c| c.id).collect();

    // Calcular lo recaudado: pagos con sState=Activo en el rango del mes
    let collected = if client_ids.is_empty() {
        0.0
    } else {
        state
            .db
            .sum_active_payments_in_range(&client_ids, start_utc, end_utc)
            .await
            .map_err(ApiError::DatabaseError)?
    };

    // Para meses pasados sin ningún dato recaudado, no tiene sentido mostrarlos
    if !is_current_month && collected == 0.0 {
        return Err(ApiError::NotFound);
    }

    // Calcular pendiente: solo si es el mes actual
    let pending = if is_current_month {
        active_clients
            .iter()
            .filter(|c| c.n_balance < 0.0)
            .map(|c| c.n_balance.abs())
            .sum::<f64>()
    } else {
        0.0
    };

    // Eficiencia de cobro: solo cuando tenemos datos de pendiente (mes actual)
    let efficiency = if is_current_month {
        let total = collected + pending;
        if total > 0.0 {
            Some((collected / total * 100.0 * 100.0).round() / 100.0)
        } else {
            Some(0.0)
        }
    } else {
        None
    };

    // Lista de meses: 3 antes + seleccionado + 3 después (capeados al actual) + siempre el actual
    let months = build_month_selector(selected_year, selected_month, current_year, current_month);
    let selected_month_str = format!("{:04}-{:02}", selected_year, selected_month);

    Ok(Json(MonthlyClosingResponse {
        months,
        selected_month: selected_month_str,
        data: MonthlyClosingData {
            collected: (collected * 100.0).round() / 100.0,
            pending: (pending * 100.0).round() / 100.0,
            efficiency,
        },
    }))
}

/// Construye la lista de meses para el selector:
/// 3 meses antes del seleccionado + el seleccionado + 3 después (capeados al actual)
/// El mes actual siempre está incluido. Orden: más reciente primero.
fn build_month_selector(
    sel_year: i32,
    sel_month: u32,
    cur_year: i32,
    cur_month: u32,
) -> Vec<String> {
    let sel_idx = month_to_index(sel_year, sel_month);
    let cur_idx = month_to_index(cur_year, cur_month);

    let mut set: BTreeSet<i32> = BTreeSet::new();

    // 3 antes + seleccionado + 3 después, capado al mes actual
    for offset in -3i32..=3 {
        let idx = sel_idx + offset;
        if idx <= cur_idx {
            set.insert(idx);
        }
    }

    // El mes actual siempre aparece
    set.insert(cur_idx);

    // Orden descendente (más reciente primero)
    set.into_iter()
        .rev()
        .map(|idx| {
            let (y, m) = index_to_month(idx);
            format!("{:04}-{:02}", y, m)
        })
        .collect()
}

fn month_to_index(year: i32, month: u32) -> i32 {
    year * 12 + month as i32 - 1
}

fn index_to_month(idx: i32) -> (i32, u32) {
    let year = idx / 12;
    let month = (idx % 12 + 1) as u32;
    (year, month)
}

fn parse_year_month(s: &str) -> Option<(i32, u32)> {
    let mut parts = s.splitn(2, '-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    if month < 1 || month > 12 {
        return None;
    }
    Some((year, month))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}
