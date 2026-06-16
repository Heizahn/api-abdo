use crate::utils::get_bson_amount::get_bson_amount;
use crate::utils::timezone::{utils as tz_utils, VenezuelaDateTime};
use async_trait::async_trait;
use futures::stream::{StreamExt, TryStreamExt};
use mongodb::bson::{doc, oid::ObjectId, Bson, DateTime, Document};
use mongodb::results::InsertOneResult;
use mongodb::{error::Error as MongoError, Collection};

use super::MongoDB;
use crate::db::SalesRepository;
use crate::models::db::{
    DailyPaymentChartPoint, Debt, LatestPayment, PartPayment, PartPaymentWithPaymentState, Payment,
    PaymentForMatch, PaymentHistoryFilters, PaymentHistoryItem, PaymentHistoryPage,
    PaymentHistoryPaymentType, PaymentHistorySortBy, PaymentHistorySortDir, PaymentReportFull,
    PaymentReportListItem,
};
use crate::models::payment::{
    Bank, ClientOwner, PaymentMethod, PaymentReport, ReferenceMatchInfo, UserPaymentInfo,
};

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn bson_date_string(doc: &Document, field: &str) -> Option<String> {
    if let Ok(dt) = doc.get_datetime(field) {
        return Some(VenezuelaDateTime::from(*dt).datetime_string_venezuela());
    }

    doc.get_str(field)
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

async fn find_client_ids_by_owner(db: &MongoDB, owner: &str) -> Result<Vec<ObjectId>, String> {
    let clients_col = db.db.collection::<Document>("Clients");
    find_object_ids(
        clients_col,
        doc! { "idOwner": owner },
        Some(doc! { "_id": 1 }),
    )
    .await
}

async fn find_ids_by_name(
    db: &MongoDB,
    collection_name: &str,
    name: &str,
) -> Result<Vec<Bson>, String> {
    let collection = db.db.collection::<Document>(collection_name);
    find_bson_ids(
        collection,
        doc! { "sName": regex_contains_filter(name) },
        Some(doc! { "_id": 1 }),
    )
    .await
}

async fn find_object_ids(
    collection: Collection<Document>,
    filter: Document,
    projection: Option<Document>,
) -> Result<Vec<ObjectId>, String> {
    let mut find = collection.find(filter);
    if let Some(projection) = projection {
        find = find.projection(projection);
    }

    let mut cursor = find.await.map_err(|e| e.to_string())?;
    let mut ids = Vec::new();
    while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
        if let Ok(id) = doc.get_object_id("_id") {
            ids.push(id);
        }
    }

    Ok(ids)
}

fn payment_history_item_from_doc(doc: Document) -> PaymentHistoryItem {
    PaymentHistoryItem {
        id: doc
            .get_object_id("_id")
            .map(|id| id.to_hex())
            .unwrap_or_default(),
        id_client: doc
            .get_object_id("idClient")
            .map(|id| id.to_hex())
            .unwrap_or_default(),
        client: doc.get_str("client_name").unwrap_or_default().to_string(),
        amount: round2(get_bson_amount(&doc, "nAmount")),
        amount_bs: round2(get_bson_amount(&doc, "nBs")),
        commentary: doc.get_str("sCommentary").ok().map(str::to_string),
        state: doc.get_str("sState").unwrap_or_default().to_string(),
        creator: doc
            .get_str("creator_name")
            .ok()
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        editor: doc
            .get_str("editor_name")
            .ok()
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        reason: doc
            .get_str("sReason")
            .ok()
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        created_at: bson_date_string(&doc, "dCreation"),
        updated_at: bson_date_string(&doc, "dEdition"),
        is_usd: doc.get_bool("bUSD").unwrap_or(false),
        is_cash: doc.get_bool("bCash").unwrap_or(false),
        reference: doc
            .get_str("sReference")
            .ok()
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
    }
}

fn payment_history_pipeline(match_doc: Document, skip: Option<u64>, limit: i64) -> Vec<Document> {
    let mut pipeline = vec![
        doc! { "$match": match_doc },
        payment_history_sort_date_stage(),
    ];

    pipeline.push(doc! { "$sort": { "_sortDate": -1, "_id": -1 } });

    if let Some(skip) = skip {
        pipeline.push(doc! { "$skip": skip as i64 });
    }
    pipeline.push(doc! { "$limit": limit });

    push_payment_history_lookups(&mut pipeline);
    push_payment_history_name_fields(&mut pipeline);
    push_payment_history_project(&mut pipeline);

    pipeline
}

fn payment_history_complete_pipeline(
    match_doc: Document,
    filters: &PaymentHistoryFilters,
    skip: Option<u64>,
    limit: Option<i64>,
) -> Vec<Document> {
    let mut pipeline = vec![
        doc! { "$match": match_doc },
        payment_history_sort_date_stage(),
    ];

    if let Some(date_match) = payment_history_date_match(filters) {
        pipeline.push(doc! { "$match": date_match });
    }

    let joins_before_pagination = payment_history_needs_joins_before_pagination(filters);
    if joins_before_pagination {
        push_payment_history_lookups(&mut pipeline);
        push_payment_history_name_fields(&mut pipeline);
    }

    push_payment_history_sort(&mut pipeline, filters);
    if let Some(skip) = skip {
        pipeline.push(doc! { "$skip": skip as i64 });
    }
    if let Some(limit) = limit {
        pipeline.push(doc! { "$limit": limit });
    }

    if !joins_before_pagination {
        push_payment_history_lookups(&mut pipeline);
        push_payment_history_name_fields(&mut pipeline);
    }

    push_payment_history_project(&mut pipeline);
    pipeline
}

fn payment_history_sort_date_stage() -> Document {
    doc! { "$addFields": {
        "_sortDate": {
            "$convert": {
                "input": "$dCreation",
                "to": "date",
                "onError": DateTime::from_millis(0),
                "onNull": DateTime::from_millis(0)
            }
        }
    }}
}

fn push_payment_history_lookups(pipeline: &mut Vec<Document>) {
    pipeline.push(doc! { "$lookup": {
        "from": "Clients",
        "localField": "idClient",
        "foreignField": "_id",
        "as": "client",
        "pipeline": [{ "$project": { "_id": 0, "sName": 1 } }]
    }});
    pipeline.push(doc! { "$unwind": { "path": "$client", "preserveNullAndEmptyArrays": true } });

    pipeline.push(doc! { "$lookup": {
        "from": "Users",
        "localField": "idCreator",
        "foreignField": "_id",
        "as": "creator",
        "pipeline": [{ "$project": { "_id": 0, "sName": 1 } }]
    }});
    pipeline.push(doc! { "$unwind": { "path": "$creator", "preserveNullAndEmptyArrays": true } });

    pipeline.push(doc! { "$lookup": {
        "from": "Users",
        "localField": "idEditor",
        "foreignField": "_id",
        "as": "editor",
        "pipeline": [{ "$project": { "_id": 0, "sName": 1 } }]
    }});
    pipeline.push(doc! { "$unwind": { "path": "$editor", "preserveNullAndEmptyArrays": true } });
}

fn push_payment_history_name_fields(pipeline: &mut Vec<Document>) {
    pipeline.push(doc! { "$addFields": {
        "client_name": { "$ifNull": ["$client.sName", ""] },
        "creator_name": { "$ifNull": ["$creator.sName", ""] },
        "editor_name": { "$ifNull": ["$editor.sName", ""] },
    }});
}

fn push_payment_history_project(pipeline: &mut Vec<Document>) {
    pipeline.push(doc! { "$project": {
        "_id": 1,
        "idClient": 1,
        "nAmount": 1,
        "nBs": 1,
        "sCommentary": 1,
        "sState": 1,
        "sReason": 1,
        "dCreation": 1,
        "dEdition": 1,
        "bUSD": 1,
        "bCash": 1,
        "sReference": 1,
        "client_name": 1,
        "creator_name": 1,
        "editor_name": 1,
    }});
}

fn payment_history_needs_joins_before_pagination(filters: &PaymentHistoryFilters) -> bool {
    matches!(
        filters.sort_by,
        PaymentHistorySortBy::Client | PaymentHistorySortBy::Creator | PaymentHistorySortBy::Editor
    )
}

fn payment_history_date_match(filters: &PaymentHistoryFilters) -> Option<Document> {
    let mut range = Document::new();
    if let Some(from) = filters.created_from {
        range.insert("$gte", DateTime::from_millis(from.timestamp_millis()));
    }
    if let Some(to) = filters.created_to {
        range.insert("$lte", DateTime::from_millis(to.timestamp_millis()));
    }

    if range.is_empty() {
        None
    } else {
        Some(doc! { "_sortDate": range })
    }
}

fn push_payment_history_sort(pipeline: &mut Vec<Document>, filters: &PaymentHistoryFilters) {
    let sort_field = match filters.sort_by {
        PaymentHistorySortBy::CreatedAt => "_sortDate",
        PaymentHistorySortBy::Client => "client_name",
        PaymentHistorySortBy::Reason => "sReason",
        PaymentHistorySortBy::State => "sState",
        PaymentHistorySortBy::Creator => "creator_name",
        PaymentHistorySortBy::Editor => "editor_name",
        PaymentHistorySortBy::Amount => "nAmount",
        PaymentHistorySortBy::AmountBs => "nBs",
        PaymentHistorySortBy::Reference => "sReference",
    };
    let direction = match filters.sort_dir {
        PaymentHistorySortDir::Asc => 1,
        PaymentHistorySortDir::Desc => -1,
    };

    let mut sort_doc = Document::new();
    sort_doc.insert(sort_field, direction);
    sort_doc.insert("_id", direction);
    pipeline.push(doc! { "$sort": sort_doc });
}

fn regex_contains_filter(value: &str) -> Document {
    doc! { "$regex": regex::escape(value), "$options": "i" }
}

fn field_regex_match(field: &str, value: &str) -> Document {
    let mut match_doc = Document::new();
    match_doc.insert(field, regex_contains_filter(value));
    match_doc
}

async fn find_bson_ids(
    collection: Collection<Document>,
    filter: Document,
    projection: Option<Document>,
) -> Result<Vec<Bson>, String> {
    let mut find = collection.find(filter);
    if let Some(projection) = projection {
        find = find.projection(projection);
    }

    let mut cursor = find.await.map_err(|e| e.to_string())?;
    let mut ids = Vec::new();
    while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
        if let Some(id) = doc.get("_id") {
            ids.push(id.clone());
        }
    }

    Ok(ids)
}

fn id_in_condition(field: &str, ids: Vec<Bson>) -> Document {
    let mut in_doc = Document::new();
    in_doc.insert("$in", ids);

    let mut condition = Document::new();
    condition.insert(field, in_doc);
    condition
}

fn append_and_condition(match_doc: &mut Document, condition: Document) {
    if condition.is_empty() {
        return;
    }

    let mut conditions = match match_doc.remove("$and") {
        Some(Bson::Array(items)) => items,
        _ => Vec::new(),
    };
    conditions.push(Bson::Document(condition));
    match_doc.insert("$and", conditions);
}

fn append_or_condition(match_doc: &mut Document, conditions: Vec<Document>) {
    if conditions.is_empty() {
        return;
    }

    append_and_condition(
        match_doc,
        doc! { "$or": conditions.into_iter().map(Bson::Document).collect::<Vec<_>>() },
    );
}

fn insert_number_range(match_doc: &mut Document, field: &str, min: Option<f64>, max: Option<f64>) {
    let mut range = Document::new();
    if let Some(min) = min {
        range.insert("$gte", min);
    }
    if let Some(max) = max {
        range.insert("$lte", max);
    }

    if !range.is_empty() {
        match_doc.insert(field, range);
    }
}

fn apply_payment_history_filters(match_doc: &mut Document, filters: &PaymentHistoryFilters) {
    if let Some(reference) = &filters.reference {
        match_doc.insert("sReference", regex_contains_filter(reference));
    }
    if let Some(reason) = &filters.reason {
        match_doc.insert("sReason", regex_contains_filter(reason));
    }
    if let Some(commentary) = &filters.commentary {
        match_doc.insert("sCommentary", regex_contains_filter(commentary));
    }
    if let Some(state) = &filters.state {
        match_doc.insert("sState", state);
    }

    match filters.payment_type {
        Some(PaymentHistoryPaymentType::Cash) => {
            match_doc.insert("bCash", true);
        }
        Some(PaymentHistoryPaymentType::Usd) => {
            match_doc.insert("bCash", false);
            match_doc.insert("bUSD", true);
        }
        Some(PaymentHistoryPaymentType::Mobile) => {
            match_doc.insert("bCash", false);
            match_doc.insert("bUSD", false);
        }
        None => {}
    }

    insert_number_range(match_doc, "nAmount", filters.amount_min, filters.amount_max);
    insert_number_range(
        match_doc,
        "nBs",
        filters.amount_bs_min,
        filters.amount_bs_max,
    );
}

async fn apply_payment_history_name_filters(
    db: &MongoDB,
    match_doc: &mut Document,
    filters: &PaymentHistoryFilters,
) -> Result<bool, String> {
    if let Some(client) = &filters.client {
        let client_ids = find_ids_by_name(db, "Clients", client).await?;
        if client_ids.is_empty() {
            return Ok(false);
        }
        append_and_condition(match_doc, id_in_condition("idClient", client_ids));
    }

    if let Some(creator) = &filters.creator {
        let creator_ids = find_ids_by_name(db, "Users", creator).await?;
        if creator_ids.is_empty() {
            return Ok(false);
        }
        append_and_condition(match_doc, id_in_condition("idCreator", creator_ids));
    }

    if let Some(editor) = &filters.editor {
        let editor_ids = find_ids_by_name(db, "Users", editor).await?;
        if editor_ids.is_empty() {
            return Ok(false);
        }
        append_and_condition(match_doc, id_in_condition("idEditor", editor_ids));
    }

    if let Some(search) = &filters.search {
        let mut identity_conditions = Vec::new();

        let client_ids = find_ids_by_name(db, "Clients", search).await?;
        if !client_ids.is_empty() {
            identity_conditions.push(id_in_condition("idClient", client_ids));
        }

        let user_ids = find_ids_by_name(db, "Users", search).await?;
        if !user_ids.is_empty() {
            identity_conditions.push(id_in_condition("idCreator", user_ids.clone()));
            identity_conditions.push(id_in_condition("idEditor", user_ids));
        }

        if identity_conditions.is_empty() {
            append_or_condition(
                match_doc,
                vec![
                    field_regex_match("sReference", search),
                    field_regex_match("sReason", search),
                    field_regex_match("sCommentary", search),
                    field_regex_match("sState", search),
                ],
            );
        } else {
            append_or_condition(match_doc, identity_conditions);
        }
    }

    Ok(true)
}

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

    async fn list_payments_simple(
        &self,
        owner_id: Option<&str>,
        reference: Option<&str>,
    ) -> Result<Vec<PaymentHistoryItem>, String> {
        let mut match_doc = doc! {};

        if let Some(owner) = owner_id {
            let client_ids = find_client_ids_by_owner(self, owner).await?;
            if client_ids.is_empty() {
                return Ok(Vec::new());
            }
            match_doc.insert("idClient", doc! { "$in": client_ids });
        }

        if let Some(reference) = reference.map(str::trim).filter(|s| !s.is_empty()) {
            match_doc.insert("sReference", reference);
        }

        let pipeline = payment_history_pipeline(match_doc, None, 500);
        let collection = self.db.collection::<Document>("Payments");
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let mut payments = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
            payments.push(payment_history_item_from_doc(doc));
        }

        Ok(payments)
    }

    async fn list_payments_complete(
        &self,
        owner_id: Option<&str>,
        filters: PaymentHistoryFilters,
    ) -> Result<PaymentHistoryPage, String> {
        let page = filters.page.max(1);
        let per_page = filters.per_page.clamp(1, 500);
        let mut match_doc = doc! {};

        if let Some(owner) = owner_id {
            let client_ids = find_client_ids_by_owner(self, owner).await?;
            if client_ids.is_empty() {
                return Ok(PaymentHistoryPage {
                    items: Vec::new(),
                    page,
                    per_page,
                    has_next_page: false,
                });
            }
            match_doc.insert("idClient", doc! { "$in": client_ids });
        }

        apply_payment_history_filters(&mut match_doc, &filters);
        if !apply_payment_history_name_filters(self, &mut match_doc, &filters).await? {
            return Ok(PaymentHistoryPage {
                items: Vec::new(),
                page,
                per_page,
                has_next_page: false,
            });
        }

        let date_range_unpaginated = filters.created_from.is_some() || filters.created_to.is_some();
        let (skip, limit) = if date_range_unpaginated {
            (None, None)
        } else {
            (
                Some(((page - 1) as u64) * (per_page as u64)),
                Some(per_page as i64 + 1),
            )
        };
        let pipeline = payment_history_complete_pipeline(match_doc, &filters, skip, limit);
        let collection = self.db.collection::<Document>("Payments");
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let mut payments = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
            payments.push(payment_history_item_from_doc(doc));
        }

        let has_next_page = !date_range_unpaginated && payments.len() > per_page as usize;
        if has_next_page {
            payments.truncate(per_page as usize);
        }

        Ok(PaymentHistoryPage {
            items: payments,
            page,
            per_page,
            has_next_page,
        })
    }

    async fn get_daily_payments_chart(
        &self,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
        owner_id: Option<&str>,
    ) -> Result<Vec<DailyPaymentChartPoint>, String> {
        let mut match_doc = doc! {
            "sState": "Activo",
            "dCreation": {
                "$gte": mongodb::bson::DateTime::from_millis(start.timestamp_millis()),
                "$lte": mongodb::bson::DateTime::from_millis(end.timestamp_millis())
            }
        };

        if let Some(owner) = owner_id {
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

            match_doc.insert("idClient", doc! { "$in": client_ids });
        }

        let pipeline = vec![
            doc! { "$match": match_doc },
            doc! {
                "$group": {
                    "_id": {
                        "$dateToString": {
                            "format": "%Y-%m-%d",
                            "date": "$dCreation",
                            "timezone": "America/Caracas"
                        }
                    },
                    "amount_usd": { "$sum": "$nAmount" },
                    "amount_bs": { "$sum": "$nBs" },
                }
            },
            doc! { "$sort": { "_id": 1 } },
        ];

        let collection = self.db.collection::<Document>("Payments");
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        let mut points: Vec<DailyPaymentChartPoint> = Vec::new();
        while let Some(Ok(doc)) = cursor.next().await {
            points.push(DailyPaymentChartPoint {
                date: doc.get_str("_id").unwrap_or_default().to_string(),
                amount_usd: get_bson_amount(&doc, "amount_usd"),
                amount_bs: get_bson_amount(&doc, "amount_bs"),
            });
        }

        Ok(points)
    }

    async fn get_monthly_closing_summary(
        &self,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
        owner_id: Option<&str>,
    ) -> Result<(f64, f64, f64), String> {
        let mut match_doc = doc! {
            "sState": "Activo",
            "dCreation": {
                "$gte": mongodb::bson::DateTime::from_millis(start.timestamp_millis()),
                "$lte": mongodb::bson::DateTime::from_millis(end.timestamp_millis())
            }
        };

        if let Some(owner) = owner_id {
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
                return Ok((0.0, 0.0, 0.0));
            }

            match_doc.insert("idClient", doc! { "$in": client_ids });
        }

        let pipeline = vec![
            doc! { "$match": match_doc },
            doc! {
                "$group": {
                    "_id": null,
                    "total_collected_usd": { "$sum": { "$ifNull": ["$nAmount", 0] } },
                    "total_paid_bs": { "$sum": { "$ifNull": ["$nBs", 0] } },
                    "total_paid_usd": {
                        "$sum": {
                            "$cond": [
                                { "$eq": [ { "$ifNull": ["$bUSD", false] }, true ] },
                                { "$ifNull": ["$nAmount", 0] },
                                0
                            ]
                        }
                    }
                }
            },
        ];

        let collection = self.db.collection::<Document>("Payments");
        let mut cursor = collection
            .aggregate(pipeline)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(Ok(doc)) = cursor.next().await {
            return Ok((
                get_bson_amount(&doc, "total_collected_usd"),
                get_bson_amount(&doc, "total_paid_usd"),
                get_bson_amount(&doc, "total_paid_bs"),
            ));
        }

        Ok((0.0, 0.0, 0.0))
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
            let created_at = doc.get_str("dCreation").unwrap_or_default().to_string();

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
                    created_at: doc.get_str("dCreation").unwrap_or_default().to_string(),
                };
                Ok(Some(report))
            }
        }
    }

    async fn acquire_report_approval_lock(
        &self,
        id: ObjectId,
        lock_token: &str,
        stale_after_ms: i64,
    ) -> Result<Option<PaymentReportFull>, String> {
        use mongodb::options::{FindOneAndUpdateOptions, ReturnDocument};

        let collection = self.db.collection::<Document>("PaymentReports");
        let now = DateTime::now();
        let stale_before = DateTime::from_millis(now.timestamp_millis() - stale_after_ms);

        // Solo lockeamos reportes no verificados y con lock ausente o stale.
        let filter = doc! {
            "_id": id,
            "sState": { "$ne": "Verificado" },
            "$or": [
                { "approval_lock": { "$exists": false } },
                { "approval_lock.at": { "$lt": stale_before } }
            ]
        };

        let update = doc! {
            "$set": {
                "approval_lock": {
                    "token": lock_token,
                    "at": now
                }
            }
        };

        let opts = FindOneAndUpdateOptions::builder()
            .return_document(ReturnDocument::After)
            .build();

        let result = collection
            .find_one_and_update(filter, update)
            .with_options(opts)
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
                    created_at: doc.get_str("dCreation").unwrap_or_default().to_string(),
                };
                Ok(Some(report))
            }
        }
    }

    async fn release_report_approval_lock(
        &self,
        id: ObjectId,
        lock_token: &str,
    ) -> Result<(), String> {
        let collection = self.db.collection::<Document>("PaymentReports");
        collection
            .update_one(
                doc! { "_id": id, "approval_lock.token": lock_token },
                doc! { "$unset": { "approval_lock": "" } },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn finalize_report_approval(
        &self,
        id: ObjectId,
        lock_token: &str,
        editor_id: &str,
        approved_at: DateTime,
    ) -> Result<bool, String> {
        let collection = self.db.collection::<Document>("PaymentReports");
        let res = collection
            .update_one(
                doc! {
                    "_id": id,
                    "approval_lock.token": lock_token,
                    "sState": { "$ne": "Verificado" }
                },
                doc! {
                    "$set": {
                        "sState": "Verificado",
                        "idEditor": editor_id,
                        "dEdition": approved_at
                    },
                    "$unset": {
                        "approval_lock": "",
                        "sRejectionReason": ""
                    }
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.modified_count > 0)
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
