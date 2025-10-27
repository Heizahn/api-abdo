use tokio::runtime::Runtime;

use crate::{
    auth::{controller::parse_bearer, service::AuthService},
    crypto::jwt::{JwtCfg, JwtService},
    db::Db,
    http::{request::Request, response::Response},
};

pub fn me<D: Db + Clone>(req: &Request, db: D) -> Response {
    // 1️⃣ Validación de header Authorization
    let Some(h) = req.header("authorization") else {
        return Response::json(401, r#"{"ok":false,"error":"missing_authorization"}"#);
    };
    let Some(token) = parse_bearer(h) else {
        return Response::json(401, r#"{"ok":false,"error":"invalid_authorization"}"#);
    };

    // 2️⃣ Decodificar y descifrar JWT
    let jwt = JwtService::new(JwtCfg::from_env());
    let claims = match jwt.decode_encrypted_verbose(token) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[/me] access verify failed: {e:?}");
            return Response::json(401, r#"{"ok":false,"error":"invalid_token"}"#);
        }
    };

    // 3️⃣ Verificar expiración y permisos
    if claims.exp < JwtService::now() {
        return Response::json(401, r#"{"ok":false,"error":"token_expired"}"#);
    }
    if !claims.scope.iter().any(|s| s == "me:read") {
        return Response::json(403, r#"{"ok":false,"error":"insufficient_scope"}"#);
    }

    // 4️⃣ Buscar cliente por ID (para obtener el teléfono)
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let customer_opt = rt.block_on(async { AuthService::lookup_by_id(&db, &claims.sub).await });
    let Some(customer) = customer_opt else {
        return Response::json(404, r#"{"ok":false,"error":"customer_not_found"}"#);
    };

    // 5️⃣ Buscar resumen por teléfono (nombre)
    let summary = rt.block_on(async { db.summary_by_phone(&customer.phone).await });

    if let Some(s) = summary {
        // ✅ Mostrar nombre del primero + suma total + cuántos hay
        let json = format!(
            r#"{{"ok":true,"customer":{{"name":"{}","phone":"{}"}}}}"#,
            s.primary_name, s.phone
        );
        Response::json(200, &json)
    } else {
        // Fallback si no hay coincidencias (debería ser raro)
        let json = format!(
            r#"{{"ok":true,"customer":{{"name":"{}","phone":"{}"}}}}"#,
            customer.full_name, customer.phone
        );
        Response::json(200, &json)
    }
}

pub fn me_balance<D: Db + Clone>(req: &Request, db: D) -> Response {
    // ... (1️⃣ Validación de header Authorization)
    let Some(h) = req.header("authorization") else {
        return Response::json(401, r#"{"ok":false,"error":"missing_authorization"}"#);
    };
    let Some(token) = parse_bearer(h) else {
        return Response::json(401, r#"{"ok":false,"error":"invalid_authorization"}"#);
    };

    // ... (2️⃣ Decodificar y descifrar JWT)
    let jwt = JwtService::new(JwtCfg::from_env());
    let claims = match jwt.decode_encrypted_verbose(token) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[/me] access verify failed: {e:?}");
            return Response::json(401, r#"{"ok":false,"error":"invalid_token"}"#);
        }
    };

    // ... (3️⃣ Verificar expiración y permisos)
    if claims.exp < JwtService::now() {
        return Response::json(401, r#"{"ok":false,"error":"token_expired"}"#);
    }
    if !claims.scope.iter().any(|s| s == "me:read") {
        return Response::json(403, r#"{"ok":false,"error":"insufficient_scope"}"#);
    }

    // Clonamos el db y el user_id para que puedan ser usados dentro del bloque async
    let db_clone = db.clone();
    let user_id = claims.sub;
    let user_id_for_log = user_id.clone();

    // ⬇️ Inicializar y Bloquear el Runtime ⬇️
    let rt = Runtime::new().expect("Failed to create Tokio runtime");

    // Usaremos un tipo Result<Response, Response> dentro del bloque async
    // para poder devolver un Response en caso de error o los balances en caso de éxito.
    let final_response: Response = rt.block_on(async {
        // 4️⃣ OBTENER BALANCE EN USD
        let usd_balance: f64 = match db_clone.get_user_balance_usd(user_id).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[/me/balance] DB error for user {user_id_for_log}: {e:?}");
                // Devolvemos el Response de error
                return Response::json(500, r#"{"ok":false,"error":"db_read_error"}"#);
            }
        };

        // 5️⃣ OBTENER TASA DE CAMBIO
        let exchange_rate: f64 = match db_clone.get_latest_exchange_rate().await {
            Ok(rate) => rate,
            Err(e) => {
                eprintln!("[/me/balance] Exchange rate DB error: {e:?}");
                // Devolvemos el Response de error
                return Response::json(500, r#"{"ok":false,"error":"rate_service_error"}"#);
            }
        };

        // 6️⃣ CALCULAR y CONSTRUIR LA RESPUESTA DE ÉXITO
        let ves_balance = usd_balance * exchange_rate * 1.08;

        let json = format!(r#"{{"ok":true,"balance_ves":{:.2}}}"#, ves_balance,);

        // Devolvemos el Response de éxito
        Response::json(200, &json)
    });

    // Devolver el Response final que salió del bloque block_on
    final_response
}
