use crate::auth::dto::*;
use crate::auth::service::AuthService;
use crate::crypto::jwt::{JwtCfg, JwtService};
use crate::db::Db;
use crate::http::request::Request;
use crate::http::response::Response;
use rand::{Rng, rng};

fn parse_bearer(h: &str) -> Option<&str> {
    // "Bearer <token>"
    let p = h.split_whitespace().collect::<Vec<_>>();
    if p.len() == 2 && p[0].eq_ignore_ascii_case("bearer") {
        Some(p[1])
    } else {
        None
    }
}

fn generate_verification_code() -> u32 {
    let code: u32 = rng().random_range(100_000..1_000_000);
    return code;
}

pub fn verify_number<D: Db + Clone>(req: &Request, db: D) -> Response {

    println!("verify_number called");
    println!("Request headers: {:?}", req.headers);
    println!("Request body: {}", req.body_string());
    // 1. Validar content-type
    if req.header("content-type") != Some("application/json") {
        return match req.header("content-type") {
            Some(ct) if !ct.contains("application/json") => {
                Response::json(400, &bad_request("invalid_content_type"))
            }
            _ => Response::json(400, &bad_request("missing_content_type")),
        };
    }

    // 2. Parsear body
    let body = req.body_string();
    let Some(phone) = parse_login_body(&body) else {
        return Response::json(400, &bad_request("invalid_json_or_phone"));
    };

    // 3. Crear runtime local para llamar async
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    let result = rt.block_on(async {
        // 4. Verificar si el usuario existe
        let found = AuthService::lookup_by_phone(&db, &phone).await;
        if found.is_none() {
            return Response::json(200, &login_response_not_found(&phone));
        }

        // 5. Generar código
        let code = generate_verification_code();

        // 6. Guardar en Mongo
        if let Err(e) = db.store_verification_code(&phone, &code).await {
            eprintln!("Error guardando código en Mongo: {:?}", e);
            let json = serde_json::json!({
                "ok": false,
                "status_code": 500,
                "message": "Eror al guardar en la db"
            });
            return Response::json(500, &json.to_string());
        }

        // 7. Enviar SMS (solo si tenés implementado)
        println!("{}", code);

        // 8. Retornar respuesta
        let json = serde_json::json!({
            "ok": true,
            "exists": true,
            "message": "verification_code_sent"
        });

        Response::json(200, &json.to_string())
    });

    result
}

pub fn login<D: Db + Clone>(req: &Request, db: D) -> Response {
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
    let (refresh, refresh_exp, _jti) = jwt.issue_encrypted_refresh(&customer.id, &family); // (por ahora no persistimos RT; cuando extiendas el trait Db lo hacemos)

    let json = format!(
        r#"{{"ok":true,"exists":true,"tokens":{{"accessToken":"{}","accessExp":{},"refreshToken":"{}","refreshExp":{}}}}}"#,
        access, access_exp, refresh, refresh_exp
    );
    Response::json(200, &json)
}

pub fn refresh<D: Db + Clone>(req: &Request, _db: D) -> Response {
    let jwt = JwtService::new(JwtCfg::from_env());

    // Body obligatorio: { "refresh_token": "..." }
    let body = req.body_string();
    let Some(rt_str) = parse_refresh_body(&body) else {
        return Response::json(400, &bad_request("missing_refresh_token"));
    };

    // Si llega Authorization, verificar firma+descifrado del access (aunque esté expirado)
    if let Some(h) = req.header("authorization") {
        if let Some(access_raw) = parse_bearer(h) {
            // Si el access falla por firma/descifrado → 401 (no aceptes refresh sin poseer access íntegro)
            let Some(access_claims) = jwt.decode_encrypted_allow_exp(access_raw) else {
                return Response::json(401, &bad_request("invalid_access_header"));
            };
            // Luego comparamos sub con el del refresh (cuando lo obtengamos).
            // Guardamos para comparar más abajo:
            // Nota: lo metemos en Option para no complicar el scope
            let _ = access_claims; // lo usaremos tras verificar el refresh
        }
    }

    // Verificar firma HS256 del REFRESH, descifrar y validar iss/exp
    let refresh_claims = match jwt.verify_encrypted_refresh_verbose(&rt_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[refresh] verify failed: {e:?}");
            return Response::json(401, &bad_request("invalid_refresh"));
        }
    };

    // Si vino Authorization, cotejar sub
    if let Some(h) = req.header("authorization") {
        if let Some(access_raw) = parse_bearer(h) {
            if let Some(access_claims) = jwt.decode_encrypted_allow_exp(access_raw) {
                if access_claims.sub != refresh_claims.sub {
                    return Response::json(401, &bad_request("sub_mismatch"));
                }
            } else {
                return Response::json(401, &bad_request("invalid_access_header"));
            }
        }
    }

    // Emitir nuevos tokens CIFRADOS
    let (access, access_exp) =
        jwt.issue_encrypted_access(&refresh_claims.sub, None, &["me:read", "payments:create"]);
    let (new_refresh, refresh_exp, _new_jti) =
        jwt.issue_encrypted_refresh(&refresh_claims.sub, &refresh_claims.fam);

    let json = format!(
        r#"{{"ok":true,"tokens":{{"accessToken":"{}","accessExp":{},"refreshToken":"{}","refreshExp":{}}}}}"#,
        access, access_exp, new_refresh, refresh_exp
    );
    Response::json(200, &json)
}

// pub fn me<D: Db + Clone>(req: Request, db: D) -> Response {
//     // 1️⃣ Validación de header Authorization
//     let Some(h) = req.header("authorization") else {
//         return Response::json(401, r#"{"ok":false,"error":"missing_authorization"}"#);
//     };
//     let Some(token) = parse_bearer(h) else {
//         return Response::json(
//             401,
//             r#"{"ok":false,"error":"invalid_authorization_format"}"#,
//         );
//     };

//     // 2️⃣ Decodificar y descifrar JWT
//     let jwt = JwtService::new(JwtCfg::from_env());
//     let claims = match jwt.decode_encrypted_verbose(token) {
//         Ok(c) => c,
//         Err(e) => {
//             eprintln!("[/me] access verify failed: {e:?}");
//             return Response::json(401, r#"{"ok":false,"error":"invalid_token"}"#);
//         }
//     };

//     // 3️⃣ Verificar expiración y permisos
//     if claims.exp < JwtService::now() {
//         return Response::json(401, r#"{"ok":false,"error":"token_expired"}"#);
//     }
//     if !claims.scope.iter().any(|s| s == "me:read") {
//         return Response::json(403, r#"{"ok":false,"error":"insufficient_scope"}"#);
//     }

//     // 4️⃣ Buscar cliente por ID (para obtener el teléfono)
//     let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
//     let customer_opt = rt.block_on(async { AuthService::lookup_by_id(&db, &claims.sub).await });
//     let Some(customer) = customer_opt else {
//         return Response::json(404, r#"{"ok":false,"error":"customer_not_found"}"#);
//     };

//     // 5️⃣ Buscar resumen por teléfono (nombre + suma de balances)
//     let summary = rt.block_on(async { db.summary_by_phone(&customer.phone).await });

//     if let Some(s) = summary {
//         // ✅ Mostrar nombre del primero + suma total + cuántos hay
//         let json = format!(
//             r#"{{"ok":true,"customer":{{"name":"{}","phone":"{}","balance":{},"matches":{}}}}}"#,
//             s.primary_name, s.phone, s.total_balance, s.count
//         );
//         Response::json(200, &json)
//     } else {
//         // Fallback si no hay coincidencias (debería ser raro)
//         let json = format!(
//             r#"{{"ok":true,"customer":{{"name":"{}","phone":"{}","balance":{}}}}}"#,
//             customer.full_name, customer.phone, customer.balance
//         );
//         Response::json(200, &json)
//     }
// }
