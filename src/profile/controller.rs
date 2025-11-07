use tokio::runtime::Runtime;
use std::collections::HashMap;
use serde_json::json;

use crate::{
    auth::{controller::parse_bearer, service::AuthService},
    crypto::jwt::{JwtCfg, JwtService},
    db::Db,
    http::{request::Request, response::Response},
    profile::structers::{ObjectId, ActiveDebtResponse},
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

pub fn me_last_payments<D: Db + Clone>(req: &Request, db: D) -> Response {
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
            eprintln!("[/profile/last_payment] access verify failed: {e:?}");
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

    let user_id = claims.sub;

    let rt = Runtime::new().expect("Failed to create Tokio runtime");

    let final_response: Response = rt.block_on(async {
        //Todo conectar a la base de datos
        let result_db = match db.get_last_payments_by_id(user_id).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[/profile/last_payment] DB error: {:?}", e); // Mejor log de error
                return Response::json(500, r#"{"ok":false,"error":"rate_service_error"}"#);
            }
        };

        let data_json = match serde_json::to_string(&result_db) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[/profile/last_payment] JSON serialization error: {:?}", e);
                return Response::json(500, r#"{"ok":false,"error":"json_serialization_error"}"#);
            }
        };

        let json = format!(r#"{{"ok":true,"data": {}}}"#, data_json);
        Response::json(200, &json)
    });

    final_response
}

pub fn me_receivable_list<D: Db + Clone>(req: &Request, db: D) -> Response {
    // 1️⃣ Manejo de autenticación (CÓDIGO ORIGINAL)
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
            eprintln!("[/profile/last_payment] access verify failed: {e:?}");
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

    let user_id = claims.sub;

    let rt = Runtime::new().expect("Failed to create Tokio runtime");

    // --- Lógica de Base de Datos y Cálculo ---

    let result = rt.block_on(async {
        // A. Obtener el sPhone del cliente asociado al user_id
        let user_client = match db.find_client_by_user_id(&user_id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Err(Response::json(
                    404,
                    r#"{"ok":false,"error":"client_not_found"}"#,
                ));
            }
            Err(e) => {
                eprintln!("DB Error finding user client: {e}");
                return Err(Response::json(
                    500,
                    r#"{"ok":false,"error":"database_error"}"#,
                ));
            }
        };

        let client_phone = user_client.s_phone;

        // B. Obtener todos los clientes que tienen ese sPhone
        let clients = match db.find_clients_by_phone(&client_phone).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("DB Error finding related clients: {e}");
                return Err(Response::json(
                    500,
                    r#"{"ok":false,"error":"database_error"}"#,
                ));
            }
        };

        if clients.is_empty() {
            return Ok(vec![]); // No hay clientes con ese teléfono, no hay deudas.
        }

        let client_ids: Vec<ObjectId> = clients.iter().map(|c| c._id.clone()).collect();

        // C. Obtener todas las deudas para estos IDs de cliente
        let all_debts = match db.find_debts_by_client_ids(&client_ids).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("DB Error finding debts: {e}");
                return Err(Response::json(
                    500,
                    r#"{"ok":false,"error":"database_error"}"#,
                ));
            }
        };

        let debt_ids: Vec<ObjectId> = all_debts.iter().map(|d| d._id.clone()).collect();
        if debt_ids.is_empty() {
            return Ok(vec![]); // No hay deudas.
        }

        // D. Obtener todas las partes de pago para estas deudas
        let part_payments = match db.find_part_payments_by_debt_ids(&debt_ids).await {
            Ok(pp) => pp,
            Err(e) => {
                eprintln!("DB Error finding part payments: {e}");
                return Err(Response::json(
                    500,
                    r#"{"ok":false,"error":"database_error"}"#,
                ));
            }
        };

        // E. Obtener todos los pagos activos
        let payment_ids: Vec<ObjectId> = part_payments
            .iter()
            .map(|pp| pp.id_payment.clone())
            .collect();
        let active_payments = match db.find_payments_by_ids(&payment_ids).await {
            Ok(p) => p
                .into_iter()
                .filter(|p| p.s_state == "Activo")
                .collect::<Vec<_>>(),
            Err(e) => {
                eprintln!("DB Error finding payments: {e}");
                return Err(Response::json(
                    500,
                    r#"{"ok":false,"error":"database_error"}"#,
                ));
            }
        };

        // F. Mapear pagos activos a un HashMap para una búsqueda rápida
        // Key: Payment ID, Value: Payment amount
        let active_payment_map: HashMap<ObjectId, f64> = active_payments
            .into_iter()
            .map(|p| (p._id, p.n_amount))
            .collect();

        // G. Calcular la deuda activa por cada deuda
        // Key: Debt ID, Value: Suma de pagos activos
        let mut paid_amount_per_debt: HashMap<ObjectId, f64> = HashMap::new();

        for pp in part_payments {
            if let Some(payment_amount) = active_payment_map.get(&pp.id_payment) {
                // Si el pago es activo, sumamos el monto del pago a la deuda correspondiente.
                // Es importante notar que la lógica del negocio asume que el `nAmount` del
                // Payment y el `nAmount` del PartPayment son iguales para la parte pagada.
                // Usaremos el `n_amount` del Payment activo para la suma.
                *paid_amount_per_debt.entry(pp.id_debt).or_default() += payment_amount;
            }
        }

        // H. Filtrar las deudas con saldo activo
        let mut active_debts_list: Vec<ActiveDebtResponse> = Vec::new();

        for debt in all_debts {
            let total_paid = paid_amount_per_debt.get(&debt._id).copied().unwrap_or(0.0);
            let active_debt_amount = debt.n_amount - total_paid;

            // La condición de filtro: solo si la deuda es mayor que cero
            if active_debt_amount > 0.001 {
                // Usamos un margen pequeño para float comparison
                active_debts_list.push(ActiveDebtResponse {
                    debt,
                    active_debt_amount,
                });
            }
        }

        Ok(active_debts_list)
    });

    match result {
        Ok(active_debts) => Response::json(
            200,
            &serde_json::to_string(&json!({
                "ok": true,
                "data": active_debts
            }))
            .unwrap(),
        ),
        Err(response) => response,
    }
}
