use crate::utils::get_bson_amount::get_bson_amount;
use crate::utils::timezone::{utils as tz_utils, VenezuelaDateTime};
use async_trait::async_trait;
use futures::stream::{StreamExt, TryStreamExt};
use mongodb::bson::{doc, oid::ObjectId, DateTime, Document};
use mongodb::results::InsertOneResult;
use mongodb::{error::Error as MongoError, Collection};

use super::MongoDB;
use crate::db::SalesRepository;
use crate::models::db::{
    Debt, LatestPayment, PartPayment, PartPaymentWithPaymentState, Payment, PaymentForMatch,
    PaymentReportFull, PaymentReportListItem,
};
use crate::models::payment::{
    Bank, ClientOwner, PaymentMethod, PaymentReport, ReferenceMatchInfo, UserPaymentInfo,
};

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
                s_reason: doc.get_str("sReason").ok().map(|s| s.to_string()),
                id_client: doc.get_object_id("idClient").ok(),
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
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(Ok(doc)) = cursor.next().await {
            return Ok(get_bson_amount(&doc, "total"));
        }
        Ok(0.0)
    }

    async fn get_latest_payments(
        &self,
        limit: u32,
        owner_id: Option<&str>,
    ) -> Result<Vec<LatestPayment>, String> {
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
            pipeline.push(
                doc! { "$unwind": { "path": "$client", "preserveNullAndEmptyArrays": true } },
            );
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
            pipeline.push(
                doc! { "$unwind": { "path": "$client", "preserveNullAndEmptyArrays": true } },
            );
        }

        // Lookup creator name (UUID string _id en Users)
        pipeline.push(doc! { "$lookup": {
            "from": "Users",
            "localField": "idCreator",
            "foreignField": "_id",
            "as": "creator",
            "pipeline": [{ "$project": { "_id": 0, "sName": 1 } }]
        }});
        pipeline
            .push(doc! { "$unwind": { "path": "$creator", "preserveNullAndEmptyArrays": true } });
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
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;
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
        issuing_bank_id: Option<ObjectId>,
    ) -> Result<Option<ReferenceMatchInfo>, String> {
        let ref_or = build_ref_filter(s_reference);

        // ── 1. Payments – mismo cliente ────────────────────────────────────
        // (sin filtro de banco — Payments no tiene idIssuingBank, tech debt documentado)
        let payments_col = self.db.collection::<Document>("Payments");
        let filter = doc! { "idClient": id_client, "$or": ref_or.clone() };
        if let Some(doc) = payments_col
            .find_one(filter)
            .await
            .map_err(|e| e.to_string())?
        {
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
        // (sin filtro de banco — tech debt igual que pass 1)
        let filter = doc! { "idClient": { "$ne": id_client }, "$or": ref_or.clone() };
        if let Some(doc) = payments_col
            .find_one(filter)
            .await
            .map_err(|e| e.to_string())?
        {
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
        // Corre siempre sin filtro de banco: mismo cliente + misma referencia = DUPLICATE,
        // independientemente del banco (regla explícita del usuario).
        let reports_col = self.db.collection::<Document>("PaymentReports");
        let filter = doc! { "idClient": id_client, "$or": ref_or.clone() };
        if let Some(doc) = reports_col
            .find_one(filter)
            .await
            .map_err(|e| e.to_string())?
        {
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

        // ── 4. PaymentReports – otro cliente (banco-scoped) ────────────────
        // Solo corre cuando `issuing_bank_id` es Some. Cuando es None se salta
        // completamente (cross-client + sin banco → ACCEPT, per tabla de verdad).
        match issuing_bank_id {
            Some(bank_oid) => {
                let filter = doc! {
                    "idClient": { "$ne": id_client },
                    "$or": ref_or.clone(),
                    "idIssuingBank": bank_oid
                };
                if let Some(doc) = reports_col
                    .find_one(filter)
                    .await
                    .map_err(|e| e.to_string())?
                {
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
            }
            None => {
                // issuing_bank_id = None → skip pass 4 entirely.
                // Cross-client + banco desconocido → ACCEPT (regla explícita).
            }
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

    // ── realtime-pending-badges: T03 ─────────────────────────────────────────

    async fn count_pending_reports(&self) -> Result<u64, String> {
        let collection = self.db.collection::<Document>("PaymentReports");
        collection
            .count_documents(doc! { "sState": "Pendiente" })
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_payment_reports(&self) -> Result<Vec<PaymentReportListItem>, String> {
        // now - 2 months boundary
        let now_ms = chrono::Utc::now().timestamp_millis();
        let two_months_ms = 2 * 30 * 24 * 60 * 60 * 1000_i64;
        let cutoff_ms = now_ms - two_months_ms;
        let cutoff_iso = {
            let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(cutoff_ms)
                .unwrap_or_default();
            dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
        };

        // Filter: Pendiente OR dPaymentDate (stored as string) >= cutoff
        // We use $expr + $toDate to convert the string field for comparison.
        let pipeline = vec![
            doc! {
                "$match": {
                    "$or": [
                        { "sState": "Pendiente" },
                        {
                            "$expr": {
                                "$gte": [
                                    { "$toDate": "$dPaymentDate" },
                                    { "$toDate": &cutoff_iso }
                                ]
                            }
                        }
                    ]
                }
            },
            // Lookup client name
            doc! {
                "$lookup": {
                    "from": "Clients",
                    "localField": "idClient",
                    "foreignField": "_id",
                    "as": "client",
                    "pipeline": [{ "$project": { "_id": 0, "sName": 1 } }]
                }
            },
            doc! { "$unwind": { "path": "$client", "preserveNullAndEmptyArrays": true } },
            // Lookup editor name (Users._id is a UUID string, idEditor is also a string)
            doc! {
                "$lookup": {
                    "from": "Users",
                    "localField": "idEditor",
                    "foreignField": "_id",
                    "as": "editor",
                    "pipeline": [{ "$project": { "_id": 0, "sName": 1 } }]
                }
            },
            doc! { "$unwind": { "path": "$editor", "preserveNullAndEmptyArrays": true } },
            // Sort: Pendiente first, then by dPaymentDate desc (convert string to date for sort)
            doc! {
                "$addFields": {
                    "_sort_pending": {
                        "$cond": { "if": { "$eq": ["$sState", "Pendiente"] }, "then": 0, "else": 1 }
                    },
                    "_sort_date": { "$toDate": "$dPaymentDate" }
                }
            },
            doc! { "$sort": { "_sort_pending": 1, "_sort_date": -1 } },
        ];

        let collection = self.db.collection::<Document>("PaymentReports");
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;
        let mut items = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let id = doc
                .get_object_id("_id")
                .map(|o| o.to_hex())
                .unwrap_or_default();

            let id_client = doc.get_object_id("idClient").ok().map(|o| o.to_hex());
            let id_payment_method = doc
                .get_object_id("idPaymentMethod")
                .ok()
                .map(|o| o.to_hex());
            let id_debt = doc.get_object_id("idDebt").ok().map(|o| o.to_hex());
            let id_issuing_bank = doc.get_object_id("idIssuingBank").ok().map(|o| o.to_hex());
            let id_editor = doc.get_str("idEditor").ok().map(|s| s.to_string());
            let id_creator = doc.get_str("idCreator").ok().map(|s| s.to_string());
            let id_payment = doc.get_object_id("idPayment").ok().map(|o| o.to_hex());

            let payment_date = doc.get_str("dPaymentDate").unwrap_or_default().to_string();
            let created_at = doc
                .get_datetime("dCreation")
                .ok()
                .map(|dt| VenezuelaDateTime::from(*dt).datetime_string_venezuela())
                .unwrap_or_default();

            let client_name = doc
                .get_document("client")
                .ok()
                .and_then(|d| d.get_str("sName").ok())
                .map(|s| s.to_string());
            let editor_name = doc
                .get_document("editor")
                .ok()
                .and_then(|d| d.get_str("sName").ok())
                .map(|s| s.to_string());

            items.push(PaymentReportListItem {
                id,
                id_client,
                id_payment_method,
                id_debt,
                reference: doc.get_str("sReference").unwrap_or_default().to_string(),
                payment_date,
                amount_bs: get_bson_amount(&doc, "nBs"),
                bank_origin: doc.get_str("sBank").unwrap_or_default().to_string(),
                phone_number: doc.get_str("sPhone").unwrap_or_default().to_string(),
                image_url: doc.get_str("sImageUrl").unwrap_or_default().to_string(),
                amount_usd: get_bson_amount(&doc, "nAmountUSD"),
                exchange_rate: get_bson_amount(&doc, "nExchangeRate"),
                state: doc.get_str("sState").unwrap_or_default().to_string(),
                rejection_reason: doc.get_str("sRejectionReason").ok().map(|s| s.to_string()),
                id_creator,
                id_editor,
                id_payment,
                id_issuing_bank,
                created_at,
                client_name,
                editor_name,
            });
        }

        Ok(items)
    }

    async fn find_report_by_id(&self, id: ObjectId) -> Result<Option<PaymentReportFull>, String> {
        let collection = self.db.collection::<Document>("PaymentReports");
        let result = collection
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;

        match result {
            None => Ok(None),
            Some(doc) => {
                let report = PaymentReportFull {
                    _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                    id_client: doc.get_object_id("idClient").ok(),
                    id_payment_method: doc.get_object_id("idPaymentMethod").ok(),
                    id_debt: doc.get_object_id("idDebt").ok(),
                    reference: doc.get_str("sReference").unwrap_or_default().to_string(),
                    payment_date: doc.get_str("dPaymentDate").unwrap_or_default().to_string(),
                    amount_bs: get_bson_amount(&doc, "nBs"),
                    bank_origin: doc.get_str("sBank").unwrap_or_default().to_string(),
                    phone_number: doc.get_str("sPhone").unwrap_or_default().to_string(),
                    image_url: doc.get_str("sImageUrl").unwrap_or_default().to_string(),
                    amount_usd: get_bson_amount(&doc, "nAmountUSD"),
                    exchange_rate: get_bson_amount(&doc, "nExchangeRate"),
                    state: doc.get_str("sState").unwrap_or_default().to_string(),
                    rejection_reason: doc.get_str("sRejectionReason").ok().map(|s| s.to_string()),
                    id_creator: doc.get_str("idCreator").ok().map(|s| s.to_string()),
                    id_editor: doc.get_str("idEditor").ok().map(|s| s.to_string()),
                    id_payment: doc.get_object_id("idPayment").ok(),
                    id_issuing_bank: doc.get_object_id("idIssuingBank").ok(),
                    created_at: doc
                        .get_datetime("dCreation")
                        .ok()
                        .map(|dt| VenezuelaDateTime::from(*dt).datetime_string_venezuela())
                        .unwrap_or_default(),
                };
                Ok(Some(report))
            }
        }
    }

    async fn update_report_state(
        &self,
        id: ObjectId,
        new_state: &str,
        editor_id: &str,
        rejection_reason: Option<&str>,
    ) -> Result<(), String> {
        let collection = self.db.collection::<Document>("PaymentReports");
        let now = mongodb::bson::DateTime::now();

        let mut set_doc = doc! {
            "sState": new_state,
            "idEditor": editor_id,
            "dEdition": now,
        };

        if let Some(reason) = rejection_reason {
            set_doc.insert("sRejectionReason", reason);
        }

        collection
            .update_one(doc! { "_id": id }, doc! { "$set": set_doc })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ── realtime-pending-badges: T04 ─────────────────────────────────────────

    async fn find_payments_for_match_by_client(
        &self,
        id_client: ObjectId,
    ) -> Result<Vec<PaymentForMatch>, String> {
        let collection = self.db.collection::<Document>("Payments");
        let options = mongodb::options::FindOptions::builder()
            .projection(doc! {
                "_id": 1,
                "sReference": 1,
                "idPaymentReport": 1,
                "idPaymentMethod": 1,
            })
            .build();

        let mut cursor = collection
            .find(doc! { "idClient": id_client, "sState": "Activo" })
            .with_options(options)
            .await
            .map_err(|e| e.to_string())?;

        let mut results = Vec::new();
        while let Some(Ok(doc)) = cursor.next().await {
            results.push(PaymentForMatch {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                s_reference: doc.get_str("sReference").unwrap_or_default().to_string(),
                id_payment_report: doc.get_object_id("idPaymentReport").ok(),
                id_payment_method: doc.get_object_id("idPaymentMethod").ok(),
            });
        }
        Ok(results)
    }

    async fn update_payment_link(
        &self,
        payment_id: ObjectId,
        id_payment_report: ObjectId,
        id_payment_method: Option<ObjectId>,
    ) -> Result<(), String> {
        let collection = self.db.collection::<Document>("Payments");
        let mut set_doc = doc! { "idPaymentReport": id_payment_report };
        if let Some(pm) = id_payment_method {
            set_doc.insert("idPaymentMethod", pm);
        }
        collection
            .update_one(doc! { "_id": payment_id }, doc! { "$set": set_doc })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn insert_payment(&self, doc: Document) -> Result<ObjectId, String> {
        let collection = self.db.collection::<Document>("Payments");
        let result = collection
            .insert_one(doc)
            .await
            .map_err(|e| e.to_string())?;
        result
            .inserted_id
            .as_object_id()
            .ok_or_else(|| "inserted_id is not ObjectId".to_string())
    }

    async fn find_payment_by_id(&self, id: ObjectId) -> Result<Option<Payment>, String> {
        let collection = self.db.collection::<Document>("Payments");
        let result = collection
            .find_one(doc! { "_id": id })
            .await
            .map_err(|e| e.to_string())?;

        match result {
            None => Ok(None),
            Some(doc) => Ok(Some(Payment {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                n_amount: get_bson_amount(&doc, "nAmount"),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
                n_bs: get_bson_amount(&doc, "nBs"),
                s_reason: doc.get_str("sReason").ok().map(|s| s.to_string()),
                id_client: doc.get_object_id("idClient").ok(),
            })),
        }
    }

    async fn update_payment_reason(
        &self,
        payment_id: ObjectId,
        reason: &str,
    ) -> Result<(), String> {
        let collection = self.db.collection::<Document>("Payments");
        collection
            .update_one(
                doc! { "_id": payment_id },
                doc! { "$set": { "sReason": reason } },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ── realtime-pending-badges: T05 ─────────────────────────────────────────

    async fn find_active_debt_by_id(&self, id: ObjectId) -> Result<Option<Debt>, String> {
        let collection = self.db.collection::<Document>("Debts");
        let result = collection
            .find_one(doc! { "_id": id, "sState": "Activo" })
            .await
            .map_err(|e| e.to_string())?;

        match result {
            None => Ok(None),
            Some(doc) => Ok(Some(Debt {
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
            })),
        }
    }

    async fn find_oldest_active_debt(
        &self,
        client_id: ObjectId,
        excluded: &[ObjectId],
    ) -> Result<Option<Debt>, String> {
        // Pipeline: Debts → PartPayments (only active-payment parts) → compute pending → filter > 0
        // → sort by dCreation asc → limit 1
        // Mirrors the legacy TS pipeline from design §5.5.
        let excluded_bson: Vec<mongodb::bson::Bson> = excluded
            .iter()
            .map(|o| mongodb::bson::Bson::ObjectId(*o))
            .collect();

        let pipeline = vec![
            doc! {
                "$match": {
                    "idClient": client_id,
                    "sState": "Activo",
                    "_id": { "$nin": excluded_bson }
                }
            },
            doc! {
                "$lookup": {
                    "from": "PartPayments",
                    "let": { "debtId": "$_id" },
                    "pipeline": [
                        { "$match": { "$expr": { "$eq": ["$idDebt", "$$debtId"] } } },
                        {
                            "$lookup": {
                                "from": "Payments",
                                "localField": "idPayment",
                                "foreignField": "_id",
                                "as": "payment",
                                "pipeline": [{ "$project": { "sState": 1 } }]
                            }
                        },
                        { "$unwind": "$payment" },
                        { "$match": { "payment.sState": "Activo" } },
                        { "$project": { "nAmount": 1 } }
                    ],
                    "as": "partPayments"
                }
            },
            doc! {
                "$addFields": {
                    "pending": {
                        "$subtract": ["$nAmount", { "$sum": "$partPayments.nAmount" }]
                    }
                }
            },
            doc! { "$match": { "pending": { "$gt": 0.0 } } },
            doc! { "$sort": { "dCreation": 1 } },
            doc! { "$limit": 1 },
        ];

        let collection = self.db.collection::<Document>("Debts");
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(Ok(doc)) = cursor.next().await {
            return Ok(Some(Debt {
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
            }));
        }
        Ok(None)
    }

    async fn find_debts_for_reason(
        &self,
        debt_ids: Vec<ObjectId>,
        this_payment_id: ObjectId,
    ) -> Result<Vec<Debt>, String> {
        // Returns debts matching ($in debt_ids) AND
        // (sState=Activo OR (sState=Anulado AND idPayment=this_payment_id))
        let filter = doc! {
            "_id": { "$in": &debt_ids },
            "$or": [
                { "sState": "Activo" },
                { "sState": "Anulado", "idPayment": this_payment_id }
            ]
        };
        let collection = self.db.collection::<Document>("Debts");
        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut debts = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            debts.push(Debt {
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
            });
        }
        Ok(debts)
    }

    async fn find_active_debt_amounts_by_client(
        &self,
        client_id: ObjectId,
    ) -> Result<Vec<f64>, String> {
        let collection = self.db.collection::<Document>("Debts");
        let options = mongodb::options::FindOptions::builder()
            .projection(doc! { "_id": 0, "nAmount": 1 })
            .build();
        let mut cursor = collection
            .find(doc! { "idClient": client_id, "sState": "Activo" })
            .with_options(options)
            .await
            .map_err(|e| e.to_string())?;

        let mut amounts = Vec::new();
        while let Some(Ok(doc)) = cursor.next().await {
            amounts.push(get_bson_amount(&doc, "nAmount"));
        }
        Ok(amounts)
    }

    async fn find_active_payment_amounts_by_client(
        &self,
        client_id: ObjectId,
    ) -> Result<Vec<f64>, String> {
        let collection = self.db.collection::<Document>("Payments");
        let options = mongodb::options::FindOptions::builder()
            .projection(doc! { "_id": 0, "nAmount": 1 })
            .build();
        let mut cursor = collection
            .find(doc! { "idClient": client_id, "sState": "Activo" })
            .with_options(options)
            .await
            .map_err(|e| e.to_string())?;

        let mut amounts = Vec::new();
        while let Some(Ok(doc)) = cursor.next().await {
            amounts.push(get_bson_amount(&doc, "nAmount"));
        }
        Ok(amounts)
    }

    // ── realtime-pending-badges: T06 ─────────────────────────────────────────

    async fn insert_part_payment(
        &self,
        id_debt: ObjectId,
        id_payment: ObjectId,
        n_amount: f64,
    ) -> Result<(), String> {
        let collection = self.db.collection::<Document>("PartPayments");
        let doc = doc! {
            "_id": ObjectId::new(),
            "idDebt": id_debt,
            "idPayment": id_payment,
            "nAmount": n_amount,
        };
        collection
            .insert_one(doc)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn find_part_payments_by_payment_id(
        &self,
        payment_id: ObjectId,
    ) -> Result<Vec<PartPayment>, String> {
        let collection = self.db.collection::<Document>("PartPayments");
        let mut cursor = collection
            .find(doc! { "idPayment": payment_id })
            .await
            .map_err(|e| e.to_string())?;

        let mut results = Vec::new();
        while let Some(Ok(doc)) = cursor.next().await {
            results.push(PartPayment {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                id_debt: doc
                    .get_object_id("idDebt")
                    .unwrap_or_else(|_| ObjectId::new()),
                id_payment: doc
                    .get_object_id("idPayment")
                    .unwrap_or_else(|_| ObjectId::new()),
                n_amount: get_bson_amount(&doc, "nAmount"),
            });
        }
        Ok(results)
    }

    async fn find_part_payments_by_debt(
        &self,
        debt_id: ObjectId,
    ) -> Result<Vec<PartPaymentWithPaymentState>, String> {
        // Join PartPayments with Payments.sState for the linked payment.
        let pipeline = vec![
            doc! { "$match": { "idDebt": debt_id } },
            doc! {
                "$lookup": {
                    "from": "Payments",
                    "localField": "idPayment",
                    "foreignField": "_id",
                    "as": "payment",
                    "pipeline": [{ "$project": { "_id": 0, "sState": 1 } }]
                }
            },
            doc! { "$unwind": { "path": "$payment", "preserveNullAndEmptyArrays": true } },
            doc! {
                "$project": {
                    "_id": 1,
                    "idDebt": 1,
                    "idPayment": 1,
                    "nAmount": 1,
                    "payment_state": { "$ifNull": ["$payment.sState", ""] }
                }
            },
        ];

        let collection = self.db.collection::<Document>("PartPayments");
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;
        let mut results = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            results.push(PartPaymentWithPaymentState {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                id_debt: doc
                    .get_object_id("idDebt")
                    .unwrap_or_else(|_| ObjectId::new()),
                id_payment: doc
                    .get_object_id("idPayment")
                    .unwrap_or_else(|_| ObjectId::new()),
                n_amount: get_bson_amount(&doc, "nAmount"),
                payment_state: doc.get_str("payment_state").unwrap_or_default().to_string(),
            });
        }
        Ok(results)
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
