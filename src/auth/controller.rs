use crate::auth::dto::*;
use crate::auth::service::AuthService;
use crate::db::Db;
use crate::http::request::Request;
use crate::http::response::Response;

pub fn login<D: Db + Clone>(req: Request, db: D) -> Response {
    if req.header("content-type") != Some("application/json") {
        if let Some(ct) = req.header("content-type") {
            if !ct.contains("application/json") {
                return Response::json(400, &bad_request("invalid_content_type"));
            }
        } else {
            return Response::json(400, &bad_request("missing_content_type"));
        }
    }

    let body = req.body_string();
    let Some(phone) = parse_login_body(&body) else {
        return Response::json(400, &bad_request("invalid_json_or_phone"));
    };

    // ⚠️ Runtime temporal por request (simple y funciona ya)
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let found = rt.block_on(AuthService::lookup_by_phone(&db, &phone));

    match found {
        Some(c) => {
            let id_str = c.id.as_ref().map(|oid| oid.to_hex()).unwrap_or_default();
            Response::json(200, &login_response_exists(&id_str, &c.full_name, &c.phone))
        }
        None => Response::json(200, &login_response_not_found(&phone)),
    }
}
