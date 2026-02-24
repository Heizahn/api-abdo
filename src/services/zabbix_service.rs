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

pub async fn get_client_traffic(
    http_client: &Client, // <- Recibimos el cliente desde AppState
    zabbix_url: &str,
    zabbix_token: &str,
    client_zabbix_code: &str,
    olt_zabbix_name: &str
) -> Result<ZabbixTrafficResponse, Box<dyn Error + Send + Sync>> {

    // 1. Buscar los Items (Download y Upload)
    let search_payload = json!({
        "jsonrpc": "2.0", "method": "item.get",
        "params": {
            // Agregamos "name" y "selectHosts" para que sea idéntico a tu script original
            "output": ["itemid", "name", "key_", "value_type"],
            "selectHosts": ["name"],
            "search": { "name": client_zabbix_code, },

            // ⚠️ COMENTAMOS EL FILTRO DEL HOST TEMPORALMENTE ⚠️
            // "host": olt_zabbix_name,

            "startSearch": false, "searchByAny": false
        },
        "id": 1
    });

    let search_resp: Value = http_client.post(zabbix_url)
        .header("Authorization", format!("Bearer {}", zabbix_token))
        .header("Content-Type", "application/json-rpc")
        .json(&search_payload)
        .send().await?.json().await?;

    // 1. 🐛 IMPRIMIR LA RESPUESTA CRUDA PARA DEBUGGEAR
    println!("🔎 RAW Zabbix Response: {}", search_resp);

    // 2. 🛡️ CAPTURAR EL ERROR NATIVO DE ZABBIX
    if let Some(error) = search_resp.get("error") {
        return Err(format!("Zabbix API Error: {}", error).into());
    }

    // 3. CONTINUAR NORMALMENTE
    let items = search_resp["result"].as_array()
        .ok_or("Error: No se pudo parsear el resultado de Zabbix")?;

    let mut down_info = None;
    let mut up_info = None;

    for item in items {
        let key = item["key_"].as_str().unwrap_or("");
        let item_id = item["itemid"].as_str().unwrap_or("").to_string();
        let val_type = item["value_type"].as_str().unwrap_or("3").parse::<i32>().unwrap_or(3);
        let hist_mode = if val_type == 0 { 0 } else { 3 };

        if key.contains(KEY_DOWNLOAD) { down_info = Some((item_id, hist_mode)); }
        else if key.contains(KEY_UPLOAD) { up_info = Some((item_id, hist_mode)); }
    }

    if down_info.is_none() && up_info.is_none() {
        return Err("No se encontraron items de tráfico para este cliente".into());
    }

    // 2. Iterar meses hacia atrás
    let now = Utc::now();
    let mut current_year = now.year();
    let mut current_month = now.month();

    let mut history_list = Vec::new();
    let mut grand_total_download = 0.0;
    let mut grand_total_upload = 0.0;

    loop {
        // Retroceder un mes (saltamos el mes actual en la primera iteración)
        if current_month == 1 {
            current_month = 12;
            current_year -= 1;
        } else {
            current_month -= 1;
        }

        let (time_from, time_till) = month_bounds(current_year, current_month);

        // Preparamos los Futures para ejecutarlos concurrentemente
        // Si no existe uno de los items, creamos un Future que devuelva 0.0 inmediatamente
        let down_fut: Pin<Box<dyn Future<Output = Result<f64, Box<dyn Error + Send + Sync>>> + Send>> = match &down_info {
            Some((id, mode)) => Box::pin(fetch_history_gb(http_client, zabbix_url, zabbix_token, id, *mode, time_from, time_till, 100)),
            None => Box::pin(async { Ok(0.0) }),
        };

        let up_fut: Pin<Box<dyn Future<Output = Result<f64, Box<dyn Error + Send + Sync>>> + Send>> = match &up_info {
            Some((id, mode)) => Box::pin(fetch_history_gb(http_client, zabbix_url, zabbix_token, id, *mode, time_from, time_till, 101)),
            None => Box::pin(async { Ok(0.0) }),
        };

        // ⚡ Ejecutamos ambas peticiones al Zabbix AL MISMO TIEMPO ⚡
        let (down_res, up_res) = tokio::join!(down_fut, up_fut);
        let month_down = down_res?;
        let month_up = up_res?;

        // Si el mes no tiene NADA de tráfico, asumimos que llegamos al límite de los datos históricos y cortamos
        if month_down == 0.0 && month_up == 0.0 {
            break;
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
        olt_name: olt_zabbix_name.to_string(),
        total_download_gb: grand_total_download,
        total_upload_gb: grand_total_upload,
        history: history_list,
    })
}