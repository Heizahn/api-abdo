use axum::{extract::{Path, State}, Extension, Json};
use std::sync::Arc;

use crate::{
    auth::claims::AccessClaims,
    db::SalesRepository,
    error::ApiError,
    models::payment::PaymentMethodResponse,
    state::AppState,
};

/// GET /v1/payments/methods/pago-movil/:debt_id
pub async fn get_pago_movil_data_handler(
    Extension(_claims): Extension<AccessClaims>,
    State(state): State<Arc<AppState>>,
    Path(debt_id): Path<String>,
) -> Result<Json<PaymentMethodResponse>, ApiError> {
    tracing::info!("💸 Buscando pago móvil (por ID) para deuda: {}", debt_id);

    // 1. Buscar Deuda -> idClient
    let debt = state.db.find_debt_by_id(&debt_id)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 2. Buscar Cliente -> idOwner
    let client = state.db.find_client_owner_by_id(&debt.id_client)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or(ApiError::NotFound)?;

    // 3. Buscar Usuario -> idPaymentMethod
    // Nota: idOwner en Client es String, en User _id es String.
    let user_info = state.db.find_user_payment_info_by_id(&client.id_owner)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(|| {
            tracing::error!("❌ Proveedor no encontrado: {}", client.id_owner);
            ApiError::NotFound
        })?;

    // Verificar si el usuario tiene un método de pago asignado
    let payment_method_id = match user_info.id_payment_method {
        Some(id) => id,
        None => {
            tracing::warn!("⚠️ El usuario {} no tiene idPaymentMethod configurado", client.id_owner);
            return Ok(Json(PaymentMethodResponse { ok: true, data: None }));
        }
    };

    // 4. Buscar Método de Pago por ID
    let payment_method_opt = state.db.find_payment_method_by_id(&payment_method_id)
        .await
        .map_err(ApiError::DatabaseError)?;

    let data = payment_method_opt.map(|pm| {
        // Asegúrate de que los campos a la derecha (pm.xxx) coincidan con tu modelo de DB
        crate::models::payment::PagoMovilData {
            bank_name: pm.bank_name, // o pm.bank, según tu modelo
            id_number: pm.id_number, // o pm.cedula / pm.rif
            phone: pm.phone,         // o pm.telephone
        }
    });

    if data.is_none() {
        tracing::warn!("⚠️ Método de pago {} no encontrado o inactivo", payment_method_id);
    } else {
        tracing::info!("✅ Datos de pago recuperados correctamente");
    }


    Ok(Json(PaymentMethodResponse {
        ok: true,
        data
    }))
}