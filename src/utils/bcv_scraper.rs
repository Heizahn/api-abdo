use anyhow::Result;
use reqwest::Client;
use scraper::{Html, Selector};

pub async fn fetch_bcv_rate() -> Result<f64> {
    let client =  Client::builder()
        .danger_accept_invalid_certs(true)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
        .build()?;

    let response = client
        .get("https://www.bcv.org.ve/")
        .send()
        .await?
        .text()
        .await?;

    let document = Html::parse_document(&response);

    let selector =
        Selector::parse("#dolar strong").map_err(|e| anyhow::anyhow!("Error selector: {:?}", e))?;

    if let Some(value) = document.select(&selector).next() {
        let text = value.text().collect::<Vec<_>>().join("");
        let cleaned_text = text.trim().replace(",", "");

        let rate = cleaned_text
            .parse::<f64>()
            .map_err(|e| anyhow::anyhow!("Error parsing rate: {:?}", e))?;

        return Ok(rate);
    }

    Err(anyhow::anyhow!(
        "No se pudo encontrar la tasa del dólar en la página del BCV"
    ))
}
