use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use chrono_tz::America::Caracas;
use serde::{Deserialize, Serialize};

/// Constante para la zona horaria de Venezuela (UTC-4)
pub const VENEZUELA_TZ: chrono_tz::Tz = Caracas;

/// Wrapper para fechas que se manejan en zona horaria de Venezuela
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenezuelaDateTime {
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub utc: DateTime<Utc>,
}

impl VenezuelaDateTime {
    /// Crea una nueva instancia con la fecha/hora actual de Venezuela
    pub fn now() -> Self {
        Self {
            utc: Utc::now(),
        }
    }

    /// Crea desde un DateTime UTC
    pub fn from_utc(dt: DateTime<Utc>) -> Self {
        Self { utc: dt }
    }

    /// Crea desde milisegundos Unix timestamp
    pub fn from_millis(millis: i64) -> Self {
        Self {
            utc: DateTime::from_timestamp_millis(millis).unwrap_or_else(Utc::now),
        }
    }

    /// Obtiene la representación en zona horaria de Venezuela
    pub fn in_venezuela(&self) -> DateTime<chrono_tz::Tz> {
        self.utc.with_timezone(&VENEZUELA_TZ)
    }

    /// Obtiene solo la fecha en formato YYYY-MM-DD en hora de Venezuela
    pub fn date_string_venezuela(&self) -> String {
        let vz_time = self.in_venezuela();
        format!("{:04}-{:02}-{:02}", vz_time.year(), vz_time.month(), vz_time.day())
    }

    /// Obtiene fecha y hora formateada en Venezuela
    pub fn datetime_string_venezuela(&self) -> String {
        let vz_time = self.in_venezuela();
        vz_time.format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// Obtiene el timestamp en milisegundos (UTC para DB)
    pub fn timestamp_millis(&self) -> i64 {
        self.utc.timestamp_millis()
    }

    /// Obtiene el DateTime UTC (para guardar en DB)
    pub fn utc(&self) -> DateTime<Utc> {
        self.utc
    }

    /// Añade duración
    pub fn add_duration(&self, duration: Duration) -> Self {
        Self {
            utc: self.utc + duration,
        }
    }

    /// Resta duración
    pub fn sub_duration(&self, duration: Duration) -> Self {
        Self {
            utc: self.utc - duration,
        }
    }

    /// Compara si es antes que otra fecha
    pub fn is_before(&self, other: &Self) -> bool {
        self.utc < other.utc
    }

    /// Compara si es después que otra fecha
    pub fn is_after(&self, other: &Self) -> bool {
        self.utc > other.utc
    }
}

/// Convierte un DateTime de MongoDB BSON a VenezuelaDateTime
impl From<mongodb::bson::DateTime> for VenezuelaDateTime {
    fn from(bson_dt: mongodb::bson::DateTime) -> Self {
        let millis = bson_dt.timestamp_millis();
        Self::from_millis(millis)
    }
}

/// Convierte VenezuelaDateTime a MongoDB BSON DateTime
impl From<VenezuelaDateTime> for mongodb::bson::DateTime {
    fn from(vz_dt: VenezuelaDateTime) -> Self {
        mongodb::bson::DateTime::from_millis(vz_dt.timestamp_millis())
    }
}

/// Utilidades para fechas en Venezuela
#[allow(dead_code)]
pub mod utils {
    use super::*;

    /// Obtiene el inicio del día actual en Venezuela (00:00:00)
    pub fn start_of_today_venezuela() -> VenezuelaDateTime {
        let now = VenezuelaDateTime::now();
        let vz_now = now.in_venezuela();

        // Crear fecha a las 00:00:00 en Venezuela
        let start_of_day = VENEZUELA_TZ
            .with_ymd_and_hms(
                vz_now.year(),
                vz_now.month(),
                vz_now.day(),
                0,
                0,
                0,
            )
            .unwrap();

        VenezuelaDateTime {
            utc: start_of_day.with_timezone(&Utc),
        }
    }

    /// Obtiene el fin del día actual en Venezuela (23:59:59)
    pub fn end_of_today_venezuela() -> VenezuelaDateTime {
        let now = VenezuelaDateTime::now();
        let vz_now = now.in_venezuela();

        // Crear fecha a las 23:59:59 en Venezuela
        let end_of_day = VENEZUELA_TZ
            .with_ymd_and_hms(
                vz_now.year(),
                vz_now.month(),
                vz_now.day(),
                23,
                59,
                59,
            )
            .unwrap();

        VenezuelaDateTime {
            utc: end_of_day.with_timezone(&Utc),
        }
    }

    /// Crea una fecha específica en zona horaria de Venezuela
    /// y la convierte a UTC
    pub fn venezuela_datetime(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<VenezuelaDateTime> {
        let vz_dt = VENEZUELA_TZ.with_ymd_and_hms(year, month, day, hour, minute, second).single()?;

        Some(VenezuelaDateTime {
            utc: vz_dt.with_timezone(&Utc),
        })
    }

    /// Formatea una fecha de MongoDB para mostrar en Venezuela
    pub fn format_bson_date_venezuela(bson_dt: mongodb::bson::DateTime) -> String {
        let vz_dt = VenezuelaDateTime::from(bson_dt);
        vz_dt.datetime_string_venezuela()
    }

    /// Convierte string de fecha YYYY-MM-DD en Venezuela a inicio del día UTC
    pub fn parse_venezuela_date(date_str: &str) -> Option<VenezuelaDateTime> {
        // Parsear "YYYY-MM-DD"
        let parts: Vec<&str> = date_str.split('-').collect();
        if parts.len() != 3 {
            return None;
        }

        let year = parts[0].parse::<i32>().ok()?;
        let month = parts[1].parse::<u32>().ok()?;
        let day = parts[2].parse::<u32>().ok()?;

        venezuela_datetime(year, month, day, 0, 0, 0)
    }
}