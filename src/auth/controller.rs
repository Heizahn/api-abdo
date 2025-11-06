use crate::auth::claims::LoginPayload;
use crate::auth::dto::*;
use crate::auth::service::AuthService;
use crate::crypto::jwt::{JwtCfg, JwtService};
use crate::db::Db;
use crate::http::request::Request;
use crate::http::response::Response;
use chrono::Utc;
use rand::{Rng, rng};
use reqwest;
use std::env;

pub fn parse_bearer(h: &str) -> Option<&str> {
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
    // 1. Validar content-type
    if let Some(ct) = req.header("content-type") {
        if !ct.contains("application/json") {
            return Response::json(400, &bad_request("invalid_content_type"));
        }
    } else {
        return Response::json(400, &bad_request("missing_content_type"));
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

        // 7. Enviar SMS
        //aqui necesito saber si el phone empieza por 0416 o 0426
        //si eso es verdadero el prefijo que se obtine de la env API_SHORT_NUMBER se debe agregar alante el 121 guardar en una variable como complete_short
        //se debe hacer una solicitud https post a API_HOST_SMS
        //se debe enviar en el header Authorization: Basic API_KEY_SMS
        /* el cuerpo de la peticion debe ser  {
          "to": "584144271554",
          "from": complete_short,
          "content": code,
          "dlr": "no",
          "coding":"3"
        }*/

        // 7.1. Obtener variables de entorno
        let (api_host, api_key, short_number) = {
            // Función helper para no repetir el manejo de errores
            let get_env_var = |name: &str| -> Result<String, Response> {
                env::var(name).map_err(|e| {
                    eprintln!(
                        "Error: variable de entorno {} no configurada. {:?}",
                        name, e
                    );
                    let json = serde_json::json!({
                        "ok": false,
                        "status_code": 500,
                        "message": "Error interno del servidor (config)"
                    });
                    Response::json(500, &json.to_string())
                })
            };

            let host = match get_env_var("API_HOST_SMS") {
                Ok(v) => v,
                Err(r) => return r,
            };
            let key = match get_env_var("API_KEY_SMS") {
                Ok(v) => v,
                Err(r) => return r,
            };
            let short = match get_env_var("API_SHORT_NUMBER") {
                Ok(v) => v,
                Err(r) => return r,
            };
            (host, key, short)
        };

        // 7.2. Preparar variables para el SMS
        let complete_short = if phone.starts_with("0416") || phone.starts_with("0426") {
            format!("121{}", short_number)
        } else {
            short_number
        };

        // Formatear el número de "0414..." a "58414..."
        let to_phone = if let Some(stripped_phone) = phone.strip_prefix('0') {
            format!("58{}", stripped_phone)
        } else {
            phone.clone() // Fallback por si ya viene sin el 0
        };

        // 7.3. Construir cliente y cuerpo de la petición
        let client = reqwest::Client::new();
        let sms_content = format!(
            "Inersiones ABDO77: Utiliza el codigo {} para verificar tu identidad. No lo compartas. Expira en 60 minutos.",
            code
        );
        let sms_body = serde_json::json!({
            "to": to_phone,
            "from": complete_short,
            "content": sms_content, // Usamos la variable 'code' del paso 5
            "dlr": "no",
            "coding": "3"
        });

        // 7.4. Enviar la solicitud POST
        let res = client
            .post(&api_host)
            .header("Authorization", format!("Basic {}", api_key))
            .json(&sms_body)
            .send()
            .await;

        // 7.5. Manejar la respuesta del envío de SMS
        match res {
            Ok(response) => {
                if !response.status().is_success() {
                    // El proveedor de SMS devolvió un error (4xx, 5xx)
                    let status = response.status();
                    let error_body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "sin cuerpo".to_string());
                    eprintln!(
                        "Error enviando SMS a {}. Status: {}. Body: {}",
                        api_host, status, error_body
                    );

                    let json = serde_json::json!({
                        "ok": false,
                        "status_code": 500,
                        "message": "Error al comunicarse con el proveedor de SMS"
                    });
                    return Response::json(500, &json.to_string());
                }
            }
            Err(e) => {
                // Error de red o al construir la petición
                eprintln!("Error de red al enviar SMS: {:?}", e);
                let json = serde_json::json!({
                    "ok": false,
                    "status_code": 500,
                    "message": "Error de red al enviar el SMS"
                });
                return Response::json(500, &json.to_string());
            }
        }

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
    // 1. Validar content-type
    if let Some(ct) = req.header("content-type") {
        if !ct.contains("application/json") {
            return Response::json(400, &bad_request("invalid_content_type"));
        }
    } else {
        return Response::json(400, &bad_request("missing_content_type"));
    }

    // --- 2. Parsear el body para `phone` Y `code` ---
    // MODIFICADO: Leemos el body y lo parseamos a `LoginPayload`
    let body_str = req.body_string();
    let payload: LoginPayload = match serde_json::from_str(&body_str) {
        Ok(p) => p,
        Err(_) => {
            // Error si el JSON es inválido o faltan campos
            return Response::json(400, &bad_request("invalid_json_or_missing_fields"));
        }
    };

    // ⚠️ Runtime temporal (sin cambios)
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    // --- 3. (Caso 2) Buscar si el *usuario* (customer) existe ---
    // MODIFICADO: Buscamos por el `payload.phone`
    let found_customer = rt.block_on(AuthService::lookup_by_phone(&db, &payload.phone));

    if found_customer.is_none() {
        // Requisito 2: "si el numero no existe entoces responde que el numero no es valido"
        // Devolvemos un 401 (No Autorizado)
        return Response::json(401, &auth_error("invalid_phone_number"));
    }

    // Si llegamos aquí, el usuario SÍ existe.
    let customer = found_customer.unwrap();

    // --- 4. (Caso 3) Buscar el código de verificación ---
    // NUEVO: Buscamos en `verification_codes`
    let found_code = rt.block_on(AuthService::lookup_verification_code(
        &db,
        &payload.phone,
        &payload.code,
    ));

    if found_code.is_none() {
        // El código no coincide o no existe para ese teléfono
        return Response::json(401, &auth_error("invalid_verification_code"));
    }

    // --- 5. (Caso 3) Verificar si el código ha expirado ---
    // NUEVO: Comparamos la fecha de expiración con la actual
    let verification = found_code.unwrap();
    let now = Utc::now(); // Obtenemos la hora actual en UTC

    if verification.expires_at < now {
        // Requisito 3: "si ya el codigo expirto resnpoder con un mensaje correspondiente"
        return Response::json(401, &auth_error("code_expired"));
    }

    // --- 6. (Caso 1) ¡Éxito! El código es válido y el usuario existe ---

    // (Opcional pero recomendado) Borra el código para que no se reutilice
    if let Some(id_to_delete) = &verification._id {
        rt.block_on(AuthService::delete_verification_code(
            &db,
            id_to_delete, // id_to_delete es de tipo &ObjectId ¡Correcto!
        ));
    }

    // Generar los tokens (lógica original)
    let jwt = JwtService::new(JwtCfg::from_env());

    let (access, access_exp) =
        jwt.issue_encrypted_access(&customer.id, None, &["me:read", "payments:create"]);

    let family = uuid::Uuid::new_v4().to_string();
    let (refresh, refresh_exp, _jti) = jwt.issue_encrypted_refresh(&customer.id, &family);

    // Respuesta de éxito con tokens (sin cambios)
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
