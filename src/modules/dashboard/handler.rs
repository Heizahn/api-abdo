use axum::{
    extract::{Extension, Query, State},
    Json,
};
use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::{
    auth::user_jwt::UserProfileClaims,
    db::{ProfileRepository, SalesRepository, UserRepository},
    error::ApiError,
    models::db::{DailyPaymentChartPoint, LatestPayment, SolvencyCounts},
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
    pub currency: Option<String>,
}

#[derive(Deserialize)]
pub struct PaymentsChartQuery {
    pub date: Option<String>,
    pub owner: Option<String>,
}



async fn resolve_owner_id(
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

#[derive(Serialize, ToSchema)]
pub struct MonthlyClosingResponse {
    pub months: Vec<String>,
    pub selected_month: String,
    pub data: MonthlyClosingData,
}

#[derive(Serialize, ToSchema)]
pub struct MonthlyClosingData {
    pub collected: f64,
    pub pending: f64,
    pub efficiency: Option<f64>,
}



#[utoipa::path(
    get,
    path = "/v1/auth-user/dashboard/latest-payments",
    tag = "Dashboard",
    security(("bearerAuth" = [])),
    params(("owner" = Option<String>, Query, description = "Filtrar por owner permitido para el caller. Si no tiene permiso, responde 403")),
    responses(
        (status = 200, description = "Últimos 10 pagos activos", body = Vec<LatestPayment>),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Owner no permitido para este usuario"),
    )
)]
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

#[utoipa::path(
    get,
    path = "/v1/auth-user/dashboard/solvency",
    tag = "Dashboard",
    security(("bearerAuth" = [])),
    params(("owner" = Option<String>, Query, description = "Filtrar por owner permitido para el caller. Si no tiene permiso, responde 403")),
    responses(
        (status = 200, description = "Conteos de clientes por estado (solventes, morosos, suspendidos)", body = SolvencyCounts),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Owner no permitido para este usuario"),
    )
)]
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

#[utoipa::path(
    get,
    path = "/v1/auth-user/dashboard/payments/chart",
    tag = "Dashboard",
    security(("bearerAuth" = [])),
    params(
        ("date" = Option<String>, Query, description = "Fecha final en formato YYYY-MM-DD. Si no se envía, retorna hoy y 6 días hacia atrás"),
        ("owner" = Option<String>, Query, description = "Filtrar por owner permitido para el caller. Si no tiene permiso, responde 403"),
    ),
    responses(
        (status = 200, description = "Serie diaria de recaudación (USD/Bs) para graficar", body = Vec<DailyPaymentChartPoint>),
        (status = 400, description = "Formato de fecha inválido o fecha futura"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Owner no permitido para este usuario"),
    )
)]
pub async fn payments_chart_handler(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<UserProfileClaims>,
    Query(params): Query<PaymentsChartQuery>,
) -> Result<Json<Vec<DailyPaymentChartPoint>>, ApiError> {
    let owner_id = resolve_owner_id(&state, &claims, params.owner.as_deref()).await?;
    let now_vz = Utc::now().with_timezone(&VENEZUELA_TZ);
    let today = now_vz.date_naive();

    let end_day = match &params.date {
        Some(s) => {
            let selected = parse_year_month_day(s).ok_or_else(|| {
                ApiError::BadRequest("Formato de fecha inválido, use YYYY-MM-DD".into())
            })?;

            if selected > today {
                return Err(ApiError::BadRequest(
                    "La fecha seleccionada no puede ser mayor al día actual".into(),
                ));
            }

            selected
        }
        None => today,
    };
    let start_day = end_day - Duration::days(6);

    let start_utc = VENEZUELA_TZ
        .with_ymd_and_hms(start_day.year(), start_day.month(), start_day.day(), 0, 0, 0)
        .single()
        .ok_or(ApiError::InternalServerError)?
        .with_timezone(&Utc);

    let end_utc = VENEZUELA_TZ
        .with_ymd_and_hms(end_day.year(), end_day.month(), end_day.day(), 23, 59, 59)
        .single()
        .ok_or(ApiError::InternalServerError)?
        .with_timezone(&Utc);

    let raw_points = state
        .db
        .get_daily_payments_chart(start_utc, end_utc, owner_id.as_deref())
        .await
        .map_err(ApiError::DatabaseError)?;

    let mut by_day: HashMap<String, DailyPaymentChartPoint> = HashMap::new();
    for p in raw_points {
        by_day.insert(p.date.clone(), p);
    }

    let mut points: Vec<DailyPaymentChartPoint> = Vec::new();
    let mut day = start_day;
    while day <= end_day {
        let date = day.format("%Y-%m-%d").to_string();
        if let Some(p) = by_day.get(&date) {
            points.push(DailyPaymentChartPoint {
                date,
                amount_usd: (p.amount_usd * 100.0).round() / 100.0,
                amount_bs: (p.amount_bs * 100.0).round() / 100.0,
            });
        } else {
            points.push(DailyPaymentChartPoint {
                date,
                amount_usd: 0.0,
                amount_bs: 0.0,
            });
        }
        day += Duration::days(1);
    }

    Ok(Json(points))
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/dashboard/monthly-closing",
    tag = "Dashboard",
    security(("bearerAuth" = [])),
    params(
        ("month" = Option<String>, Query, description = "Mes en formato YYYY-MM (default: mes actual)"),
        ("owner" = Option<String>, Query, description = "Filtrar por owner permitido para el caller. Si no tiene permiso, responde 403"),
        ("currency" = Option<String>, Query, description = "Moneda solicitada para total: bs | usd. Si se omite, se usa total general en USD"),
    ),
    responses(
        (status = 200, description = "Cierre mensual (cobrado, pendiente, eficiencia) + selector de meses", body = MonthlyClosingResponse),
        (status = 400, description = "Formato de mes inválido o mes futuro"),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Owner no permitido para este usuario"),
    )
)]
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

    let (selected_year, selected_month) = match &params.month {
        Some(s) => parse_year_month(s)
            .ok_or_else(|| ApiError::BadRequest("Formato de mes inválido, use YYYY-MM".into()))?,
        None => (current_year, current_month),
    };

    let selected_idx = month_to_index(selected_year, selected_month);
    if selected_idx > current_idx {
        return Err(ApiError::BadRequest(
            "El mes seleccionado no puede ser mayor al mes actual".into(),
        ));
    }

    let is_current_month = selected_idx == current_idx;

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

    let active_clients = state
        .db
        .find_active_clients_for_closing(owner_id.as_deref())
        .await
        .map_err(ApiError::DatabaseError)?;

    let client_ids: Vec<_> = active_clients.iter().map(|c| c.id).collect();
    let total_collected_usd = if client_ids.is_empty() {
        0.0
    } else {
        state
            .db
            .sum_active_payments_in_range(&client_ids, start_utc, end_utc)
            .await
            .map_err(ApiError::DatabaseError)?
    };

    let (_, total_paid_usd, total_paid_bs) = state
        .db
        .get_monthly_closing_summary(start_utc, end_utc, owner_id.as_deref())
        .await
        .map_err(ApiError::DatabaseError)?;
    let collected = match params.currency.as_deref() {
        Some(raw) if raw.eq_ignore_ascii_case("bs") => total_paid_bs,
        Some(raw) if raw.eq_ignore_ascii_case("usd") => total_paid_usd,
        Some(_) => {
            return Err(ApiError::BadRequest(
                "Moneda inválida. Use currency=bs o currency=usd".into(),
            ))
        }
        None => total_collected_usd,
    };



    let pending = if is_current_month {
        active_clients
            .iter()
            .filter(|c| c.n_balance < 0.0)
            .map(|c| c.n_balance.abs())
            .sum::<f64>()
    } else {
        0.0
    };

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



fn build_month_selector(
    sel_year: i32,
    sel_month: u32,
    cur_year: i32,
    cur_month: u32,
) -> Vec<String> {
    let sel_idx = month_to_index(sel_year, sel_month);
    let cur_idx = month_to_index(cur_year, cur_month);

    let mut set: BTreeSet<i32> = BTreeSet::new();

    for offset in -3i32..=3 {
        let idx = sel_idx + offset;
        if idx <= cur_idx {
            set.insert(idx);
        }
    }

    set.insert(cur_idx);

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

fn parse_year_month_day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
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
