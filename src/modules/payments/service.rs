use std::future::Future;
use std::pin::Pin;

use axum::http::StatusCode;
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};

use crate::db::Db;
use crate::error::ApiError;
use crate::models::db::PaymentCreateResult;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Redondea un f64 a 2 decimales (half-away-from-zero, suficiente para dinero).
#[inline]
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

// ---------------------------------------------------------------------------
// PaymentInput — parámetros de entrada para `create_payment`
// ---------------------------------------------------------------------------

/// Campos requeridos para crear un pago. `id_creator` DEBE provenir del JWT
/// (nunca del body de la petición HTTP).
#[derive(Debug, Clone)]
pub struct PaymentInput {
    /// ObjectId del cliente al que pertenece el pago.
    pub id_client: ObjectId,
    /// Referencia bancaria / número de comprobante.
    pub s_reference: String,
    /// Monto en bolívares.
    pub n_bs: f64,
    /// Monto en USD (divisa base del sistema).
    pub n_amount: f64,
    /// Si el pago es en USD efectivo.
    pub b_usd: bool,
    /// Si el pago es en efectivo.
    pub b_cash: bool,
    /// ObjectId del método de pago (banco destino / MO / etc.).
    pub id_payment_method: Option<ObjectId>,
    /// ObjectId del reporte de pago que origina este pago (si aplica).
    pub id_payment_report: Option<ObjectId>,
    /// UUID del usuario staff que crea el pago (extraído del JWT).
    pub id_creator: String,
    /// Fecha de creación opcional (ISO string). Si es `None` se usa `now()`.
    pub d_creation: Option<String>,
    /// Comentario libre del operador.
    pub s_commentary: Option<String>,
}

// ---------------------------------------------------------------------------
// PaymentsService
// ---------------------------------------------------------------------------

/// Servicio de negocio para la creación y procesamiento de pagos.
///
/// Genérico sobre `DB: Db` porque el trait maestro `Db` requiere `Clone` y
/// no puede usarse como `dyn Db`. En la práctica `DB` siempre es `MongoDB`.
///
/// Instanciación: `PaymentsService::new(state.db.clone())`.
pub struct PaymentsService<DB: Db> {
    db: DB,
}

impl<DB: Db> PaymentsService<DB> {
    pub fn new(db: DB) -> Self {
        Self { db }
    }

    // -----------------------------------------------------------------------
    // T11 — update_balance
    // -----------------------------------------------------------------------

    /// Recomputa `nBalance` del cliente a partir de:
    /// `nBalance = Σ Payments(Activo).nAmount − Σ Debts(Activo).nAmount`
    /// Resultado redondeado a 2 decimales. Escribe en `Clients.nBalance`.
    ///
    /// Retorna el nuevo balance (útil para logs y el caller que quiera loguearlo).
    pub async fn update_balance(&self, client_id: ObjectId) -> Result<f64, ApiError> {
        let (debts_res, payments_res) = tokio::join!(
            self.db.find_active_debt_amounts_by_client(client_id),
            self.db.find_active_payment_amounts_by_client(client_id),
        );

        let debt_amounts = debts_res.map_err(|e| {
            tracing::error!(
                "update_balance: error fetching debts for {}: {}",
                client_id,
                e
            );
            ApiError::Internal("balance_update_failed".to_string())
        })?;

        let payment_amounts = payments_res.map_err(|e| {
            tracing::error!(
                "update_balance: error fetching payments for {}: {}",
                client_id,
                e
            );
            ApiError::Internal("balance_update_failed".to_string())
        })?;

        let total_debt: f64 = debt_amounts.iter().sum();
        let total_payment: f64 = payment_amounts.iter().sum();
        let balance = round2(total_payment - total_debt);

        self.db
            .update_client_balance(client_id, balance)
            .await
            .map_err(|e| {
                tracing::error!(
                    "update_balance: error writing balance for {}: {}",
                    client_id,
                    e
                );
                ApiError::Internal("balance_update_failed".to_string())
            })?;

        tracing::debug!("update_balance: client={} balance={}", client_id, balance);
        Ok(balance)
    }

    // -----------------------------------------------------------------------
    // T12 — calculate_payment_reason
    // -----------------------------------------------------------------------

    /// Calcula y escribe `sReason` en el `Payment`.
    ///
    /// Reglas (réplica exacta del TS de LoopBack):
    /// - Sin `PartPayments` → `"Abono"` (o `"Abono (Anulado)"` si el pago está anulado).
    /// - Con `PartPayments` → une los `sReason` de las `Debt`s vinculadas en mayúsculas,
    ///   separadas por `", "`, con `" (Anulado)"` como sufijo si el pago está anulado.
    ///   Prefiere deudas `Activo`; si no hay ninguna activa, usa todas.
    pub async fn calculate_payment_reason(&self, payment_id: ObjectId) -> Result<(), ApiError> {
        // 1. Fetch el pago para saber su sState.
        let payment = self
            .db
            .find_payment_by_id(payment_id)
            .await
            .map_err(|e| {
                tracing::error!(
                    "calculate_payment_reason: error fetching payment {}: {}",
                    payment_id,
                    e
                );
                ApiError::Internal("calculate_reason_failed".to_string())
            })?
            .ok_or_else(|| {
                ApiError::domain_simple(
                    StatusCode::NOT_FOUND,
                    "payment_not_found",
                    "Pago no encontrado",
                )
            })?;

        let suffix = if payment.s_state == "Anulado" {
            " (Anulado)"
        } else {
            ""
        };

        // 2. Fetch PartPayments del pago.
        let parts = self
            .db
            .find_part_payments_by_payment_id(payment_id)
            .await
            .map_err(|e| {
                tracing::error!(
                    "calculate_payment_reason: error fetching part_payments for {}: {}",
                    payment_id,
                    e
                );
                ApiError::Internal("calculate_reason_failed".to_string())
            })?;

        if parts.is_empty() {
            let reason = format!("Abono{}", suffix);
            return self.write_payment_reason(payment_id, &reason).await;
        }

        // 3. Obtener los IDs de Debt de los PartPayments.
        let debt_ids: Vec<ObjectId> = parts.iter().map(|p| p.id_debt).collect();

        // 4. Query Debts con el filtro especial:
        //    sState=Activo OR (sState=Anulado AND idPayment=payment_id).
        let candidate_debts = self
            .db
            .find_debts_for_reason(debt_ids, payment_id)
            .await
            .map_err(|e| {
                tracing::error!(
                    "calculate_payment_reason: error fetching debts for reason {}: {}",
                    payment_id,
                    e
                );
                ApiError::Internal("calculate_reason_failed".to_string())
            })?;

        if candidate_debts.is_empty() {
            let reason = format!("Abono{}", suffix);
            return self.write_payment_reason(payment_id, &reason).await;
        }

        // 5. Preferir deudas Activas; si no hay ninguna, usar todas.
        let active: Vec<&crate::models::db::Debt> = candidate_debts
            .iter()
            .filter(|d| d.s_state == "Activo")
            .collect();

        let pool: Vec<&crate::models::db::Debt> = if active.is_empty() {
            candidate_debts.iter().collect()
        } else {
            active
        };

        let joined: String = pool
            .iter()
            .map(|d| d.s_reason.as_str())
            .collect::<Vec<_>>()
            .join(", ");

        let reason = format!("{}{}", joined.to_uppercase(), suffix);
        self.write_payment_reason(payment_id, &reason).await
    }

    /// Helper interno para escribir `sReason` — evita duplicar el map_err.
    async fn write_payment_reason(
        &self,
        payment_id: ObjectId,
        reason: &str,
    ) -> Result<(), ApiError> {
        self.db
            .update_payment_reason(payment_id, reason)
            .await
            .map_err(|e| {
                tracing::error!(
                    "write_payment_reason: error writing reason for {}: {}",
                    payment_id,
                    e
                );
                ApiError::Internal("calculate_reason_failed".to_string())
            })
    }

    // -----------------------------------------------------------------------
    // T13 — process_payment (recursivo, Box::pin)
    // -----------------------------------------------------------------------

    /// Distribuye `amount` contra las deudas activas del cliente de forma recursiva.
    ///
    /// Retorna el monto restante que no pudo asignarse a ninguna deuda (excedente).
    ///
    /// Algoritmo (réplica del TS de LoopBack — ver design §4.2):
    /// 1. Si `amount <= 0` → retorna 0.
    /// 2. Determina la deuda objetivo:
    ///    - Si `opt_debt_id` es `Some`: busca por ID activa; si no existe, cae
    ///      en la deuda más antigua.
    ///    - `None`: busca la deuda activa más antigua del cliente excluyendo `excluded`.
    ///    - Sin deuda disponible: retorna `amount` (excedente).
    /// 3. Re-calcula el `pending` de la deuda (nAmount − Σ PartPayments de pagos activos).
    /// 4. Si `pending <= 0`: agrega deuda a `excluded` y recursa.
    /// 5. Inserta un `PartPayment` por `min(amount, pending)`.
    /// 6. Si queda remanente: recursa con la deuda excluida si quedó saldada.
    ///
    /// **Recursión async**: usa `Box::pin` (stable Rust, sin crates externos).
    /// La profundidad está acotada por el número de deudas activas del cliente (< 50 en práctica).
    pub fn process_payment<'a>(
        &'a self,
        client_id: ObjectId,
        payment_id: ObjectId,
        amount: f64,
        opt_debt_id: Option<ObjectId>,
        excluded: Vec<ObjectId>,
    ) -> Pin<Box<dyn Future<Output = Result<f64, ApiError>> + Send + 'a>> {
        Box::pin(async move {
            let amount = round2(amount);
            if amount <= 0.0 {
                return Ok(0.0);
            }

            // ── Determinar deuda objetivo ──────────────────────────────────
            let debt = match opt_debt_id {
                Some(id) => {
                    match self.db.find_active_debt_by_id(id).await.map_err(|e| {
                        tracing::error!("process_payment: find_active_debt_by_id {}: {}", id, e);
                        ApiError::Internal("process_payment_failed".to_string())
                    })? {
                        Some(d) => Some(d),
                        None => {
                            tracing::debug!(
                                "process_payment: debt {} not found/not active, \
                                 falling back to oldest",
                                id
                            );
                            self.db
                                .find_oldest_active_debt(client_id, &excluded)
                                .await
                                .map_err(|e| {
                                    tracing::error!(
                                        "process_payment: find_oldest_active_debt \
                                         fallback client {}: {}",
                                        client_id,
                                        e
                                    );
                                    ApiError::Internal("process_payment_failed".to_string())
                                })?
                        }
                    }
                }
                None => self
                    .db
                    .find_oldest_active_debt(client_id, &excluded)
                    .await
                    .map_err(|e| {
                        tracing::error!(
                            "process_payment: find_oldest_active_debt client {}: {}",
                            client_id,
                            e
                        );
                        ApiError::Internal("process_payment_failed".to_string())
                    })?,
            };

            let debt = match debt {
                Some(d) => d,
                None => {
                    // No hay más deudas — retorna el excedente.
                    tracing::debug!(
                        "process_payment: no more debts for client {}, leftover={}",
                        client_id,
                        amount
                    );
                    return Ok(amount);
                }
            };

            // ── Re-calcular el pending de la deuda ─────────────────────────
            // Se re-query para protección de concurrencia (otra request puede
            // haber parcializado esta deuda entre el `find_oldest` y este punto).
            let parts = self
                .db
                .find_part_payments_by_debt(debt._id)
                .await
                .map_err(|e| {
                    tracing::error!(
                        "process_payment: find_part_payments_by_debt debt={} payment={}: {}",
                        debt._id,
                        payment_id,
                        e
                    );
                    ApiError::Internal("process_payment_failed".to_string())
                })?;

            let already_applied: f64 = parts
                .iter()
                .filter(|p| p.payment_state == "Activo")
                .map(|p| p.n_amount)
                .sum();

            let pending = round2(debt.n_amount - already_applied);

            if pending <= 0.0 {
                // Deuda ya saldada — excluirla y recursar.
                let mut new_excluded = excluded;
                new_excluded.push(debt._id);
                return self
                    .process_payment(client_id, payment_id, amount, None, new_excluded)
                    .await;
            }

            // ── Insertar PartPayment ───────────────────────────────────────
            let amount_to_use = round2(amount.min(pending));

            if amount_to_use > 0.0 {
                self.db
                    .insert_part_payment(debt._id, payment_id, amount_to_use)
                    .await
                    .map_err(|e| {
                        tracing::error!(
                            "process_payment: insert_part_payment \
                             debt={} payment={}: {}",
                            debt._id,
                            payment_id,
                            e
                        );
                        ApiError::Internal("process_payment_failed".to_string())
                    })?;

                let new_remaining = round2(amount - amount_to_use);

                if new_remaining > 0.0 {
                    let mut new_excluded = excluded;
                    if round2(pending - amount_to_use) <= 0.0 {
                        new_excluded.push(debt._id);
                    }
                    return self
                        .process_payment(client_id, payment_id, new_remaining, None, new_excluded)
                        .await;
                }
            }

            Ok(0.0)
        })
    }

    // -----------------------------------------------------------------------
    // T14 — create_payment
    // -----------------------------------------------------------------------

    /// Crea un pago completo:
    /// 1. Valida campos obligatorios.
    /// 2. Inserta el doc `Payment` con `sState = "Activo"`.
    /// 3. Distribuye el monto contra deudas (`process_payment`).
    /// 4. Recomputa el balance del cliente (`update_balance`).
    /// 5. Calcula y escribe la razón del pago (`calculate_payment_reason`).
    ///
    /// `id_creator` DEBE venir del JWT — el caller es responsable de extraerlo.
    /// Esta función NO es transaccional (réplica del comportamiento legacy).
    pub async fn create_payment(
        &self,
        payment_data: PaymentInput,
        opt_id_debt: Option<ObjectId>,
    ) -> Result<PaymentCreateResult, ApiError> {
        // ── 1. Validaciones ───────────────────────────────────────────────
        if payment_data.n_amount <= 0.0 {
            return Err(ApiError::Domain {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                code: "invalid_amount".to_string(),
                field: Some("nAmount".to_string()),
                message: "El monto debe ser mayor a cero".to_string(),
                details: None,
            });
        }

        if payment_data.id_creator.is_empty() {
            return Err(ApiError::Domain {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                code: "missing_field".to_string(),
                field: Some("idCreator".to_string()),
                message: "El campo idCreator es requerido".to_string(),
                details: None,
            });
        }

        // ── 2. Redondear ──────────────────────────────────────────────────
        let n_amount = round2(payment_data.n_amount);
        let n_bs = round2(payment_data.n_bs);

        // ── 3. Construir doc Payment ──────────────────────────────────────
        let creation_date = payment_data
            .d_creation
            .unwrap_or_else(|| BsonDateTime::now().to_string());

        let mut doc = mongodb::bson::doc! {
            "idClient":   payment_data.id_client,
            "sReference": payment_data.s_reference.as_str(),
            "nBs":        n_bs,
            "nAmount":    n_amount,
            "bUSD":       payment_data.b_usd,
            "bCash":      payment_data.b_cash,
            "sState":     "Activo",
            "dCreation":  creation_date.as_str(),
            "idCreator":  payment_data.id_creator.as_str(),
        };

        if let Some(id) = payment_data.id_payment_method {
            doc.insert("idPaymentMethod", id);
        }
        if let Some(id) = payment_data.id_payment_report {
            doc.insert("idPaymentReport", id);
        }
        if let Some(comment) = &payment_data.s_commentary {
            doc.insert("sCommentary", comment.as_str());
        }

        // ── 4. Insertar Payment ───────────────────────────────────────────
        let payment_oid = self.db.insert_payment(doc).await.map_err(|e| {
            tracing::error!("create_payment: insert_payment failed: {}", e);
            ApiError::Internal("create_payment_failed".to_string())
        })?;

        tracing::debug!("create_payment: inserted payment_id={}", payment_oid);

        // ── 5. Distribuir monto contra deudas ─────────────────────────────
        let leftover = self
            .process_payment(
                payment_data.id_client,
                payment_oid,
                n_amount,
                opt_id_debt,
                vec![],
            )
            .await
            .map_err(|e| {
                tracing::error!(
                    "create_payment: process_payment failed for payment {}: {:?}",
                    payment_oid,
                    e
                );
                e
            })?;

        if leftover > 0.0 {
            tracing::debug!(
                "create_payment: payment {} leftover={} (no more debts)",
                payment_oid,
                leftover
            );
        }

        // ── 6. Recomputar balance ─────────────────────────────────────────
        self.update_balance(payment_data.id_client).await?;

        // ── 7. Calcular razón del pago ────────────────────────────────────
        self.calculate_payment_reason(payment_oid).await?;

        Ok(PaymentCreateResult {
            payment_id: payment_oid.to_hex(),
        })
    }
}
