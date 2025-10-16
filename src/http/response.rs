pub struct Response {
    pub status: u16,
    pub status_text: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body_json: &str) -> Self {
        let status_text = status_text(status);
        let mut headers = vec![
            (
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
            ("Content-Length".to_string(), body_json.len().to_string()),
        ];
        // CORS dev (ajusta luego):
        headers.push(("Access-Control-Allow-Origin".to_string(), "*".to_string()));
        headers.push((
            "Access-Control-Allow-Headers".to_string(),
            "Content-Type, Authorization".to_string(),
        ));
        headers.push((
            "Access-Control-Allow-Methods".to_string(),
            "GET,POST,OPTIONS".to_string(),
        ));

        Self {
            status,
            status_text,
            headers,
            body: body_json.as_bytes().to_vec(),
        }
    }

    pub fn options_ok() -> Self {
        let mut r = Self::json(204, "");
        r.headers
            .push(("Content-Length".to_string(), "0".to_string()));
        r
    }

    pub fn not_found() -> Self {
        Self::json(404, r#"{"ok":false,"error":"not_found"}"#)
    }

    pub fn method_not_allowed() -> Self {
        Self::json(405, r#"{"ok":false,"error":"method_not_allowed"}"#)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, self.status_text);
        for (k, v) in &self.headers {
            out.push_str(&format!("{}: {}\r\n", k, v));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    }
}
