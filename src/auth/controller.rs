use crate::auth::dto::*;
use crate::auth::service::AuthService;
use crate::crypto::jwt::{JwtCfg, JwtService};
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

    // ✅ si no existe, responde como antes
    if found.is_none() {
        return Response::json(200, &login_response_not_found(&phone));
    }

    let customer = found.unwrap();
    let jwt = JwtService::new(JwtCfg::from_env());

    let (access, access_exp) =
        jwt.issue_encrypted_access(&customer.id, None, &["me:read", "payments:create"]);

    let family = uuid::Uuid::new_v4().to_string();
    let (refresh, refresh_exp, _jti) = jwt.issue_refresh(&customer.id, &family); // 👈 _jti

    // (por ahora no persistimos RT; cuando extiendas el trait Db lo hacemos)

    let json = format!(
        r#"{{"ok":true,"exists":true,"tokens":{{"accessToken":"{}","accessExp":{},"refreshToken":"{}","refreshExp":{}}}}}"#,
        access, access_exp, refresh, refresh_exp
    );
    Response::json(200, &json)
}

pub fn refresh<D: Db + Clone>(req: Request, _db: D) -> Response {
    // 👈 _db
    let body = req.body_string();
    let Some(rt_str) = parse_refresh_body(&body) else {
        return Response::json(400, &bad_request("invalid_json_refresh"));
    };

    let jwt = JwtService::new(JwtCfg::from_env());
    let token = match jwt.verify_refresh(&rt_str) {
        Ok(t) => t,
        Err(_) => return Response::json(401, &bad_request("invalid_refresh")),
    };
    let claims = token.claims;

    // Por ahora asumimos refresh válido (cuando agregues persistencia, valida/revoca aquí)
    // let runtime = tokio::runtime::Runtime::new().unwrap();
    // let ok = runtime.block_on(async { db.is_refresh_valid(&claims.jti).await });
    // if !ok { return Response::json(401, &bad_request("refresh_revoked_or_exp")); }

    let (access, access_exp) =
        jwt.issue_encrypted_access(&claims.sub, None, &["me:read", "payments:create"]);

    let (new_refresh, refresh_exp, _new_jti) = jwt.issue_refresh(&claims.sub, &claims.fam); // 👈 _new_jti

    // Rotación real (cuando tengas persistencia):
    // runtime.block_on(async {
    //     db.revoke_refresh(&claims.jti).await;
    //     db.save_refresh(RefreshRecord { jti: _new_jti.clone(), sub: claims.sub.clone(), fam: claims.fam.clone(), exp: refresh_exp, revoked: false }).await;
    // });

    let json = format!(
        r#"{{"ok":true,"tokens":{{"accessToken":"{}","accessExp":{},"refreshToken":"{}","refreshExp":{}}}}}"#,
        access, access_exp, new_refresh, refresh_exp
    );
    Response::json(200, &json)
}
