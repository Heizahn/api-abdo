use std::collections::HashMap;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .get(&key.to_ascii_lowercase())
            .map(|s| s.as_str())
    }

    // Obtiene el body como String (UTF-8)
    pub fn body_string(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap_or_default()
    }
}

// Parser HTTP MUY simple (solo lo necesario)
pub fn parse_request(mut stream: &std::net::TcpStream) -> std::io::Result<Request> {
    use std::io::{BufRead, BufReader, Read};
    let mut reader = BufReader::new(&mut stream);

    let mut start_line = String::new();
    reader.read_line(&mut start_line)?;
    if start_line.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "empty request",
        ));
    }
    let parts: Vec<_> = start_line.split_whitespace().collect();
    let method = parts.get(0).unwrap_or(&"GET").to_string();
    let path = parts.get(1).unwrap_or(&"/").to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line_trim = line.trim_end();
        if line_trim.is_empty() {
            break;
        }
        if let Some((k, v)) = line_trim.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}
