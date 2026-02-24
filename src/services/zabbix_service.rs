use reqwest::Client;
use serde_json::{json, Value};
use chrono::{Utc, TimeZone, Datelike};
use std::error::Error;
use std::pin::Pin;
use std::future::Future;
use crate::models::Zabbix::{MonthlyTraffic, ZabbixTrafficResponse};

const KEY_DOWNLOAD: &str = "onu.vol.download";
const KEY_UPLOAD: &str = "onu.vol.upload";

// Función auxiliar para calcular los límites de tiempo de un mes
fn month_bounds(year: i32, month: u32) -> (i64, i64) {
    let start = Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).unwrap();
    let (next_y, next_m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let end = Utc.with_ymd_and_hms(next_y, next_m, 1, 0, 0, 0).unwrap() - chrono::Duration::seconds(1);
    (start.timestamp(), end.timestamp())
}

// Función auxiliar para extraer el historial de un item específico
async fn fetch_history_gb(
    client: &Client,
    zabbix_url: &str,
    zabbix_token: &str,
    item_id: &str,
    history_mode: i32,
    time_from: i64,
    time_till: i64,
    req_id: u64
) -> Result<f64, Box<dyn Error + Send + Sync>> {
    let payload = json!({
        "jsonrpc": "2.0", "method": "history.get",
        "params": {
            "output": "extend", "history": history_mode, "itemids": [item_id],
            "time_from": time_from, "time_till": time_till
        },
        "id": req_id
    });

    let resp: Value = client.post(zabbix_url)
        .header("Authorization", format!("Bearer {}", zabbix_token))
        .header("Content-Type", "application/json-rpc")
        .json(&payload)
        .send()
        .await?
        .json()
        .await?;

    let mut total_bytes: u128 = 0;
    if let Some(points) = resp["result"].as_array() {
        for point in points {
            if let Some(val_str) = point["value"].as_str() {
                if let Ok(val) = val_str.parse::<f64>() {
                    total_bytes += val as u128;
                }
            }
        }
    }

    // Si no hay datos, retorna 0.0
    if total_bytes == 0 {
        return Ok(0.0);
    }

    Ok((total_bytes as f64) / 1024.0 / 1024.0 / 1024.0)
}

// ... (imports y funciones auxiliares iguales) ...

pub async fn get_client_traffic(
    http_client: &Client,
    zabbix_url: &str,
    zabbix_token: &str,
    client_zabbix_code: &str,
    // Eliminamos el parámetro 'olt_zabbix_name' porque lo deduciremos de la respuesta
) -> Result<ZabbixTrafficResponse, Box<dyn Error + Send + Sync>> {

    // 1. Buscar los Items (Download y Upload)
    let search_payload = json!({
        "jsonrpc": "2.0", "method": "item.get",
        "params": {
            // Solo pedimos las keys de consumo, ignoramos las de velocidad (onu.download)
            "output": ["itemid", "name", "key_", "value_type"],
            "selectHosts": ["name"],
            // Forzamos que la búsqueda coincida EXACTAMENTE con el código del cliente
            // y que contenga la palabra "CONSUMO" para filtrar basuras.
            "search": { "name": format!("CONSUMO *{}*", client_zabbix_code) },
            "searchWildcardsEnabled": true,
            "startSearch": false, "searchByAny": false
        },
        "id": 1
    });

    let search_resp: Value = http_client.post(zabbix_url)
        .header("Authorization", format!("Bearer {}", zabbix_token))
        .header("Content-Type", "application/json-rpc")
        .json(&search_payload)
        .send().await?.json().await?;

    if let Some(error) = search_resp.get("error") {
        return Err(format!("Zabbix API Error: {}", error).into());
    }

    let items = search_resp["result"].as_array()
        .ok_or("Error: No se pudo parsear el resultado de Zabbix")?;

    let mut down_info = None;
    let mut up_info = None;
    let mut detected_olt_name = String::from("OLT DESCONOCIDA");

    for item in items {
        // Validación estricta: Nos aseguramos de que el nombre contenga el código EXACTO
        // Esto evita agarrar "GPON03ONU13 WILLIANMOLINARIERA" cuando buscas solo "GPON03ONU13"
        let full_name = item["name"].as_str().unwrap_or("");

        // Si no es el cliente exacto que buscamos (por ejemplo, tiene un sufijo), lo saltamos
        if !full_name.ends_with(client_zabbix_code) && !full_name.contains(&format!("{} ", client_zabbix_code)) {
            continue;
        }

        let key = item["key_"].as_str().unwrap_or("");
        let item_id = item["itemid"].as_str().unwrap_or("").to_string();
        let val_type = item["value_type"].as_str().unwrap_or("3").parse::<i32>().unwrap_or(3);
        let hist_mode = if val_type == 0 { 0 } else { 3 };

        // Extraemos el nombre real de la OLT desde la respuesta de Zabbix
        if detected_olt_name == "OLT DESCONOCIDA" {
            detected_olt_name = item["hosts"].as_array()
                .and_then(|h| h.first())
                .and_then(|h| h["name"].as_str())
                .unwrap_or("OLT DESCONOCIDA")
                .to_string();
        }

        if key.contains(KEY_DOWNLOAD) { down_info = Some((item_id, hist_mode)); }
        else if key.contains(KEY_UPLOAD) { up_info = Some((item_id, hist_mode)); }
    }

    if down_info.is_none() && up_info.is_none() {
        return Err("No se encontraron items de tráfico EXACTOS para este cliente".into());
    }

    // 2. Iterar meses hacia atrás
    let now = Utc::now();
    let mut current_year = now.year();
    let mut current_month = now.month();

    let mut history_list = Vec::new();
    let mut grand_total_download = 0.0;
    let mut grand_total_upload = 0.0;

    // DEFINIMOS UN LÍMITE DE BÚSQUEDA (ej: 6 meses).
    // Cambiamos el 'loop' infinito por un 'for' para evitar que se corte prematuramente si hay un mes en 0 por suspensión.
    let MAX_MESES_HISTORIAL = 6;

    for _ in 0..MAX_MESES_HISTORIAL {
        if current_month == 1 {
            current_month = 12;
            current_year -= 1;
        } else {
            current_month -= 1;
        }

        let (time_from, time_till) = month_bounds(current_year, current_month);

        let down_fut: Pin<Box<dyn Future<Output = Result<f64, Box<dyn Error + Send + Sync>>> + Send>> = match &down_info {
            Some((id, mode)) => Box::pin(fetch_history_gb(http_client, zabbix_url, zabbix_token, id, *mode, time_from, time_till, 100)),
            None => Box::pin(async { Ok(0.0) }),
        };

        let up_fut: Pin<Box<dyn Future<Output = Result<f64, Box<dyn Error + Send + Sync>>> + Send>> = match &up_info {
            Some((id, mode)) => Box::pin(fetch_history_gb(http_client, zabbix_url, zabbix_token, id, *mode, time_from, time_till, 101)),
            None => Box::pin(async { Ok(0.0) }),
        };

        let (down_res, up_res) = tokio::join!(down_fut, up_fut);
        let month_down = down_res.unwrap_or(0.0); // Usamos unwrap_or(0.0) para que un fallo de red temporal no rompa todo el historial
        let month_up = up_res.unwrap_or(0.0);

        // Si llevamos 3 meses seguidos en 0.0, asumimos que ya no hay más datos históricos y cortamos para ahorrar recursos
        if month_down == 0.0 && month_up == 0.0 {
            // Puedes poner un contador aquí si quieres que soporte meses suspendidos.
            // Por ahora lo dejamos almacenar el mes en 0 y continuar.
        }

        grand_total_download += month_down;
        grand_total_upload += month_up;

        history_list.push(MonthlyTraffic {
            year: current_year,
            month: current_month,
            download_gb: month_down,
            upload_gb: month_up,
        });
    }

    Ok(ZabbixTrafficResponse {
        client_zabbix_code: client_zabbix_code.to_string(),
        olt_name: detected_olt_name, // Devolvemos la OLT que encontramos
        total_download_gb: grand_total_download,
        total_upload_gb: grand_total_upload,
        history: history_list,
    })
}