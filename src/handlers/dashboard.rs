use axum::{extract::{Query, State}, Json};
use chrono::{Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    db::{ProfileRepository, SalesRepository},
    error::ApiError,
    state::AppState,
    utils::timezone::VENEZUELA_TZ,
};

#[derive(Deserialize)]
pub struct MonthlyClosingQuery {
    pub month: Option<String>,
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

/// GET /v1/auth-user/dashboard/monthly-closing?month=YYYY-MM
pub async fn monthly_closing_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<MonthlyClosingQuery>,
) -> Result<Json<MonthlyClosingResponse>, ApiError> {
    let now_vz = Utc::now().with_timezone(&VENEZUELA_TZ);
    let current_year = now_vz.year();
    let current_month = now_vz.month();

    // Parsear mes solicitado o usar el actual
    let (selected_year, selected_month) = match &params.month {
        Some(s) => parse_year_month(s)
            .ok_or_else(|| ApiError::BadRequest("Formato de mes inválido, use YYYY-MM".into()))?,
        None => (current_year, current_month),
    };

    let is_current_month = selected_year == current_year && selected_month == current_month;

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
        .find_active_clients_for_closing()
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

    // Lista de últimos 6 meses para el selector del frontend
    let months = last_six_months(current_year, current_month);
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

fn last_six_months(year: i32, month: u32) -> Vec<String> {
    let mut result = Vec::with_capacity(6);
    let mut y = year;
    let mut m = month;
    for _ in 0..6 {
        result.push(format!("{:04}-{:02}", y, m));
        if m == 1 {
            m = 12;
            y -= 1;
        } else {
            m -= 1;
        }
    }
    result
}
