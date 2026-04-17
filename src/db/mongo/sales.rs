use crate::utils::get_bson_amount::get_bson_amount;
use crate::utils::timezone::{utils as tz_utils, VenezuelaDateTime};
use async_trait::async_trait;
use futures::stream::{StreamExt, TryStreamExt};
use mongodb::bson::{doc, oid::ObjectId, DateTime, Document};
use mongodb::results::InsertOneResult;
use mongodb::{error::Error as MongoError, Collection};

use super::MongoDB;
use crate::db::SalesRepository;
use crate::models::db::{Debt, LatestPayment, PartPayment, Payment};
use crate::models::payment::{Bank, ClientOwner, PaymentMethod, PaymentReport, ReferenceMatchInfo, UserPaymentInfo};

#[async_trait]
impl SalesRepository for MongoDB {
    async fn get_latest_exchange_rate(&self) -> Result<f64, MongoError> {
        let db_bcv = self.client.database("BCV");
        let collection: Collection<Document> = db_bcv.collection("BCVRates");

        // 1. Obtener el inicio y fin del día usando la librería de timezone
        // Esto devuelve VenezuelaDateTime con el UTC correcto: 04:00 AM / 03:59:59 AM del día siguiente
        let start_of_day = tz_utils::start_of_today_venezuela();
        let end_of_day = tz_utils::end_of_today_venezuela();

        // 2. Convertir a BSON para la query de Mongo
        // (Usamos el impl From<VenezuelaDateTime> for BsonDateTime)
        let start_of_day_bson = mongodb::bson::DateTime::from(start_of_day.clone());
        let end_of_day_bson = mongodb::bson::DateTime::from(end_of_day.clone());

        // Log para depurar y ver que las fechas son correctas
        tracing::info!(
            "🔎 Buscando tasas desde: {} hasta: {} (Hora Vzla)",
            start_of_day.datetime_string_venezuela(),
            end_of_day.datetime_string_venezuela()
        );

        let filter = doc! {
            "timestamp": {
                "$gte": start_of_day_bson,
                "$lte": end_of_day_bson
            }
        };

        let options = mongodb::options::FindOptions::builder()
            .sort(doc! { "timestamp": -1 })
            .limit(1)
            .build();

        let mut cursor = collection.find(filter).with_options(options).await?;
        let doc = cursor.try_next().await?;

        match doc {
            Some(d) => {
                // Usamos tu helper para sacar el float de forma segura
                let rate = get_bson_amount(&d, "value");

                if rate <= 0.0 {
                    return Err(MongoError::custom("Invalid or zero exchange rate value"));
                }

                // Log informativo con la fecha formateada
                if let Ok(ts) = d.get_datetime("timestamp") {
                    // Convertimos el timestamp de BSON a tu struct VenezuelaDateTime
                    let vz_time = VenezuelaDateTime::from(*ts);

                    tracing::info!(
                        "💱 Tasa BCV: {} @ {} (hora Venezuela)",
                        rate,
                        vz_time.datetime_string_venezuela()
                    );
                } else {
                    tracing::info!("💱 Tasa BCV encontrada: {}", rate);
                }
                Ok(rate)
            }
            None => {
                // Usamos start_of_day para mostrar desde qué hora buscamos
                tracing::warn!(
                    "⚠️ No se encontró tasa BCV para hoy (Buscando desde {})",
                    start_of_day.datetime_string_venezuela()
                );
                Err(MongoError::custom(
                    "No exchange rate found for today in BCV collection",
                ))
            }
        }
    }
    async fn find_part_payments_by_debt_ids(
        &self,
        debt_ids: &[ObjectId],
    ) -> Result<Vec<PartPayment>, String> {
        let filter = doc! { "idDebt": { "$in": debt_ids } };
        let collection = self.db.collection::<Document>("PartPayments");
        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut part_payments = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let pp = PartPayment {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                id_debt: doc
                    .get_object_id("idDebt")
                    .unwrap_or_else(|_| ObjectId::new()),
                id_payment: doc
                    .get_object_id("idPayment")
                    .unwrap_or_else(|_| ObjectId::new()),
                n_amount: get_bson_amount(&doc, "nAmount"),
            };
            part_payments.push(pp);
        }
        Ok(part_payments)
    }

    async fn find_payments_by_ids(&self, payment_ids: &[ObjectId]) -> Result<Vec<Payment>, String> {
        let filter = doc! { "_id": { "$in": payment_ids } };
        let collection = self.db.collection::<Document>("Payments");
        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut payments = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let payment = Payment {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                n_amount: get_bson_amount(&doc, "nAmount"),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
                n_bs: get_bson_amount(&doc, "nBs"),
            };
            payments.push(payment);
        }
        Ok(payments)
    }

    async fn find_active_debts_by_client_ids(
        &self,
        client_ids: &[ObjectId],
    ) -> Result<Vec<Debt>, String> {
        let filter = doc! {
            "idClient": { "$in": client_ids },
            "sState": "Activo"
        };
        let sort_debts = doc! { "dCreation": 1 };
        let collection = self.db.collection::<Document>("Debts");
        let mut cursor = collection
            .find(filter)
            .sort(sort_debts)
            .await
            .map_err(|e| e.to_string())?;
        let mut debts = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let debt = Debt {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                n_amount: get_bson_amount(&doc, "nAmount"),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
                id_client: doc
                    .get_object_id("idClient")
                    .unwrap_or_else(|_| ObjectId::new()),
                s_reason: doc.get_str("sReason").unwrap_or_default().to_string(),
                d_creation: doc
                    .get_datetime("dCreation")
                    .ok()
                    .cloned()
                    .unwrap_or_else(|| DateTime::from_millis(0)),
            };
            debts.push(debt);
        }
        Ok(debts)
    }

    //[PAYMENT]
    async fn find_debt_by_id(&self, id: &str) -> Result<Option<Debt>, String> {
        // 1. Cambiamos <Debt> por <Document> para leer sin errores de tipo
        let collection = self.db.collection::<Document>("Debts");
        let obj_id = ObjectId::parse_str(id).map_err(|e| e.to_string())?;

        // 2. Buscamos el documento crudo
        let result = collection
            .find_one(doc! { "_id": obj_id })
            .await
            .map_err(|e| e.to_string())?;

        // 3. Mapeamos manualmente usando el helper
        match result {
            Some(doc) => {
                let debt = Debt {
                    _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                    // AQUI LA PROTECCION:
                    n_amount: get_bson_amount(&doc, "nAmount"),
                    s_state: doc.get_str("sState").unwrap_or_default().to_string(),
                    id_client: doc
                        .get_object_id("idClient")
                        .unwrap_or_else(|_| ObjectId::new()),
                    s_reason: doc.get_str("sReason").unwrap_or_default().to_string(),
                    d_creation: doc
                        .get_datetime("dCreation")
                        .ok()
                        .cloned()
                        .unwrap_or_else(|| DateTime::from_millis(0)),
                };
                Ok(Some(debt))
            }
            None => Ok(None),
        }
    }
    async fn find_client_owner_by_id(
        &self,
        client_id: &ObjectId,
    ) -> Result<Option<ClientOwner>, String> {
        let collection = self.db.collection::<ClientOwner>("Clients");
        let options = mongodb::options::FindOneOptions::builder()
            .projection(doc! { "idOwner": 1 })
            .build();
        collection
            .find_one(doc! { "_id": client_id })
            .with_options(options)
            .await
            .map_err(|e| e.to_string())
    }

    // CAMBIO: Buscar User y traer idPaymentMethod
    async fn find_user_payment_info_by_id(
        &self,
        user_id: &str,
    ) -> Result<Option<UserPaymentInfo>, String> {
        let collection = self.db.collection::<UserPaymentInfo>("Users");
        let filter = doc! { "_id": user_id};
        let options = mongodb::options::FindOneOptions::builder()
            .projection(doc! { "idPaymentMethod": 1 }) // Solo traemos el ID del método
            .build();

        collection
            .find_one(filter)
            .with_options(options)
            .await
            .map_err(|e| e.to_string())
    }

    // CAMBIO: Buscar PaymentMethod por _id
    async fn find_payment_method_by_id(
        &self,
        id: &ObjectId,
    ) -> Result<Option<PaymentMethod>, String> {
        let collection = self.db.collection::<PaymentMethod>("PaymentMethods");
        let filter = doc! {
            "_id": id,
            "bActive": true // Mantenemos el filtro de activo por seguridad
        };

        collection.find_one(filter).await.map_err(|e| e.to_string())
    }

    async fn create_payment_report(
        &self,
        report: PaymentReport,
    ) -> Result<InsertOneResult, MongoError> {
        // Accede a la colección tipada con el struct
        let collection = self.db.collection::<PaymentReport>("PaymentReports");

        // Inserta el documento
        collection.insert_one(report).await
    }

    async fn sum_active_payments_in_range(
        &self,
        client_ids: &[ObjectId],
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> Result<f64, String> {
        let start_bson = mongodb::bson::DateTime::from_millis(start.timestamp_millis());
        let end_bson = mongodb::bson::DateTime::from_millis(end.timestamp_millis());

        let pipeline = vec![
            doc! {
                "$match": {
                    "idClient": { "$in": client_ids },
                    "sState": "Activo",
                    "dCreation": { "$gte": start_bson, "$lte": end_bson },
                }
            },
            doc! {
                "$group": {
                    "_id": null,
                    "total": { "$sum": "$nAmount" },
                }
            },
        ];

        let collection = self.db.collection::<Document>("Payments");
        let mut cursor = collection.aggregate(pipeline).await.map_err(|e| e.to_string())?;

        if let Some(Ok(doc)) = cursor.next().await {
            return Ok(get_bson_amount(&doc, "total"));
        }
        Ok(0.0)
    }

    async fn get_latest_payments(&self, limit: u32, owner_id: Option<&str>) -> Result<Vec<LatestPayment>, String> {
        let mut pipeline: Vec<Document> = Vec::new();

        if let Some(owner) = owner_id {
            // Paso 1: obtener los _id de los clientes del owner (usa índice en idOwner)
            let clients_col = self.db.collection::<Document>("Clients");
            let mut cursor = clients_col
                .find(doc! { "idOwner": owner })
                .projection(doc! { "_id": 1 })
                .await
                .map_err(|e| e.to_string())?;

            let mut client_ids: Vec<ObjectId> = Vec::new();
            while let Some(Ok(doc)) = cursor.next().await {
                if let Ok(id) = doc.get_object_id("_id") {
                    client_ids.push(id);
                }
            }

            if client_ids.is_empty() {
                return Ok(vec![]);
            }

            // Paso 2: filtrar pagos por esos IDs (usa índice en idClient), luego sort+limit
            pipeline.push(doc! { "$match": { "idClient": { "$in": &client_ids } } });
            pipeline.push(doc! { "$sort": { "dCreation": -1 } });
            pipeline.push(doc! { "$limit": limit as i64 });
            pipeline.push(doc! { "$lookup": {
                "from": "Clients",
                "localField": "idClient",
                "foreignField": "_id",
                "as": "client",
                "pipeline": [{ "$project": { "_id": 0, "sName": 1 } }]
            }});
            pipeline.push(doc! { "$unwind": { "path": "$client", "preserveNullAndEmptyArrays": true } });
        } else {
            pipeline.push(doc! { "$sort": { "dCreation": -1 } });
            pipeline.push(doc! { "$limit": limit as i64 });
            pipeline.push(doc! { "$lookup": {
                "from": "Clients",
                "localField": "idClient",
                "foreignField": "_id",
                "as": "client",
                "pipeline": [{ "$project": { "_id": 0, "sName": 1 } }]
            }});
            pipeline.push(doc! { "$unwind": { "path": "$client", "preserveNullAndEmptyArrays": true } });
        }

        // Lookup creator name (UUID string _id en Users)
        pipeline.push(doc! { "$lookup": {
            "from": "Users",
            "localField": "idCreator",
            "foreignField": "_id",
            "as": "creator",
            "pipeline": [{ "$project": { "_id": 0, "sName": 1 } }]
        }});
        pipeline.push(doc! { "$unwind": { "path": "$creator", "preserveNullAndEmptyArrays": true } });
        pipeline.push(doc! { "$project": {
            "_id": 1,
            "dCreation": 1,
            "sReason": 1,
            "sState": 1,
            "nAmount": 1,
            "nBs": 1,
            "client_name": { "$ifNull": ["$client.sName", ""] },
            "creator_name": { "$ifNull": ["$creator.sName", ""] },
        }});

        let collection = self.db.collection::<Document>("Payments");
        let mut cursor = collection.aggregate(pipeline).await.map_err(|e| e.to_string())?;
        let mut payments = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let id = doc
                .get_object_id("_id")
                .map(|o| o.to_hex())
                .unwrap_or_default();

            let created_at = doc
                .get_datetime("dCreation")
                .ok()
                .map(|dt| VenezuelaDateTime::from(*dt).datetime_string_venezuela())
                .unwrap_or_default();

            payments.push(LatestPayment {
                id,
                created_at,
                reason: doc.get_str("sReason").unwrap_or_default().to_string(),
                state: doc.get_str("sState").unwrap_or_default().to_string(),
                amount: get_bson_amount(&doc, "nAmount"),
                amount_bs: get_bson_amount(&doc, "nBs"),
                client_name: doc.get_str("client_name").unwrap_or_default().to_string(),
                creator_name: doc.get_str("creator_name").unwrap_or_default().to_string(),
            });
        }

        Ok(payments)
    }

    async fn find_bank_list(&self) -> Result<Vec<Bank>, String> {
        let collection = self.db.collection::<Bank>("ListBanks");

        let mut cursor = collection.find(doc! {}).await.map_err(|e| e.to_string())?;
        let mut banks = Vec::new();

        while let Some(Ok(bank)) = cursor.next().await {
            banks.push(bank);
        }

        Ok(banks)
    }

    async fn check_reference(
        &self,
        id_client: &ObjectId,
        s_reference: &str,
    ) -> Result<Option<ReferenceMatchInfo>, String> {
        let ref_or = build_ref_filter(s_reference);

        // ── 1. Payments – mismo cliente ────────────────────────────────────
        let payments_col = self.db.collection::<Document>("Payments");
        let filter = doc! { "idClient": id_client, "$or": ref_or.clone() };
        if let Some(doc) = payments_col.find_one(filter).await.map_err(|e| e.to_string())? {
            return Ok(Some(ReferenceMatchInfo {
                source: "payments".to_string(),
                is_same_client: true,
                s_name: None,
                s_reference: doc.get_str("sReference").unwrap_or_default().to_string(),
                n_amount: get_bson_amount(&doc, "nAmount"),
                n_bs: get_bson_amount(&doc, "nBs"),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
            }));
        }

        // ── 2. Payments – otro cliente ─────────────────────────────────────
        let filter = doc! { "idClient": { "$ne": id_client }, "$or": ref_or.clone() };
        if let Some(doc) = payments_col.find_one(filter).await.map_err(|e| e.to_string())? {
            let s_name = match doc.get_object_id("idClient") {
                Ok(cid) => fetch_client_name(&self.db, &cid).await?,
                Err(_) => None,
            };
            return Ok(Some(ReferenceMatchInfo {
                source: "payments".to_string(),
                is_same_client: false,
                s_name,
                s_reference: doc.get_str("sReference").unwrap_or_default().to_string(),
                n_amount: get_bson_amount(&doc, "nAmount"),
                n_bs: get_bson_amount(&doc, "nBs"),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
            }));
        }

        // ── 3. PaymentReports – mismo cliente ──────────────────────────────
        let reports_col = self.db.collection::<Document>("PaymentReports");
        let filter = doc! { "idClient": id_client, "$or": ref_or.clone() };
        if let Some(doc) = reports_col.find_one(filter).await.map_err(|e| e.to_string())? {
            return Ok(Some(ReferenceMatchInfo {
                source: "payment_reports".to_string(),
                is_same_client: true,
                s_name: None,
                s_reference: doc.get_str("sReference").unwrap_or_default().to_string(),
                n_amount: get_bson_amount(&doc, "nAmountUSD"),
                n_bs: get_bson_amount(&doc, "nBs"),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
            }));
        }

        // ── 4. PaymentReports – otro cliente ───────────────────────────────
        let filter = doc! { "idClient": { "$ne": id_client }, "$or": ref_or.clone() };
        if let Some(doc) = reports_col.find_one(filter).await.map_err(|e| e.to_string())? {
            let s_name = match doc.get_object_id("idClient") {
                Ok(cid) => fetch_client_name(&self.db, &cid).await?,
                Err(_) => None,
            };
            return Ok(Some(ReferenceMatchInfo {
                source: "payment_reports".to_string(),
                is_same_client: false,
                s_name,
                s_reference: doc.get_str("sReference").unwrap_or_default().to_string(),
                n_amount: get_bson_amount(&doc, "nAmountUSD"),
                n_bs: get_bson_amount(&doc, "nBs"),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
            }));
        }

        Ok(None)
    }

    async fn find_pending_reports_by_debt_ids(
        &self,
        debt_ids: &[ObjectId],
    ) -> Result<Vec<PaymentReport>, String> {
        let collection = self.db.collection::<PaymentReport>("PaymentReports");

        let filter = doc! {
            "idDebt": { "$in": debt_ids },
            "sState": "Pendiente"
        };

        let cursor = collection.find(filter).await.map_err(|e| e.to_string());
        let mut reports = Vec::new();

        let mut cursor = cursor?;
        while let Some(Ok(report)) = cursor.next().await {
            reports.push(report);
        }

        Ok(reports)
    }

    async fn find_rejected_reports_by_debt_id(
        &self,
        debt_id: &ObjectId,
    ) -> Result<Vec<PaymentReport>, String> {
        let collection = self.db.collection::<PaymentReport>("PaymentReports");

        let filter = doc! {
            "idDebt": debt_id,
            "sState": "Rechazado"
        };

        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut reports = Vec::new();

        while let Some(Ok(report)) = cursor.next().await {
            reports.push(report);
        }

        Ok(reports)
    }

    async fn find_rejected_reports_by_client_id(
        &self,
        client_id: &ObjectId,
    ) -> Result<Vec<PaymentReport>, String> {
        let collection = self.db.collection::<PaymentReport>("PaymentReports");

        let filter = doc! {
            "idClient": client_id,
            "sState": "Rechazado"
        };

        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut reports = Vec::new();

        while let Some(Ok(report)) = cursor.next().await {
            reports.push(report);
        }

        Ok(reports)
    }
}

// ============================================================
// Helpers para check_reference
// ============================================================

/// Genera un filtro $or que detecta coincidencia de sufijo bidireccional:
///   - El campo almacenado termina con `s_reference` (campo más largo)
///   - `s_reference` termina con el campo almacenado (ref enviada más larga)
fn build_ref_filter(s_reference: &str) -> Vec<Document> {
    // Sufijos del valor enviado → el campo almacenado puede ser cualquiera de ellos
    let suffix_bson: Vec<mongodb::bson::Bson> = generate_suffixes(s_reference)
        .into_iter()
        .map(mongodb::bson::Bson::String)
        .collect();

    // Regex para el caso inverso: el campo almacenado es más largo y termina con s_reference
    let escaped = regex_escape(s_reference);

    vec![
        doc! { "sReference": { "$in": suffix_bson } },
        doc! { "sReference": { "$regex": format!("{}$", escaped) } },
    ]
}

/// Genera todos los sufijos no vacíos de `s` (incluyendo `s` mismo)
fn generate_suffixes(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    (0..len)
        .map(|i| chars[i..].iter().collect::<String>())
        .collect()
}

/// Escapa caracteres especiales de expresiones regulares
fn regex_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                format!("\\{}", c)
            }
            other => other.to_string(),
        })
        .collect()
}

/// Obtiene el `sName` de un cliente por su ObjectId
async fn fetch_client_name(
    db: &mongodb::Database,
    client_id: &ObjectId,
) -> Result<Option<String>, String> {
    let col = db.collection::<Document>("Clients");
    let options = mongodb::options::FindOneOptions::builder()
        .projection(doc! { "sName": 1 })
        .build();
    let result = col
        .find_one(doc! { "_id": client_id })
        .with_options(options)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.and_then(|d| d.get_str("sName").ok().map(|s| s.to_string())))
}
