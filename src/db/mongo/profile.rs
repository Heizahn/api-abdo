use super::MongoDB;
use crate::db::mongo::ResultGroupedByDate;
use crate::db::ProfileRepository;
use crate::domain::customer::{Customer, CustomerView};
use crate::models::db::{ActiveClientBalance, Client, ClientListItem, SolvencyCounts, Tax};
use crate::utils::get_bson_amount::get_bson_amount;
use async_trait::async_trait;
use futures::stream::StreamExt;
use futures::TryStreamExt;
use mongodb::bson::{self, Document};
use mongodb::bson::{doc, oid::ObjectId};
use mongodb::error::Error as MongoError;
use mongodb::Collection;
use std::collections::HashMap;

#[async_trait]
impl ProfileRepository for MongoDB {
    async fn find_customer_by_phone(&self, phone: &str) -> Option<Customer> {
        let filter = doc! { "sPhone": phone };
        let result = self.customers().find_one(filter).await.ok()??;

        Some(Customer {
            id: result.get_object_id("_id").ok()?.to_string(),
            full_name: result.get_str("sName").unwrap_or_default().to_string(),
            phone: result.get_str("sPhone").unwrap_or_default().to_string(),
        })
    }

    async fn find_customer_by_id(&self, id: &str) -> Option<CustomerView> {
        let obj_id = ObjectId::parse_str(id).ok()?;
        let filter = doc! { "_id": obj_id };
        let result = self.customers().find_one(filter).await.ok()??;

        Some(CustomerView {
            full_name: result.get_str("sName").unwrap_or_default().to_string(),
            phone: result.get_str("sPhone").unwrap_or_default().to_string(),
        })
    }

    async fn find_clients_by_phone(&self, s_phone: &str) -> Result<Vec<Client>, String> {
        let filter = doc! { "sPhone": s_phone };
        let mut cursor = self
            .customers()
            .find(filter)
            .await
            .map_err(|e| e.to_string())?;
        let mut clients = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let client = Client {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                s_phone: doc.get_str("sPhone").unwrap_or_default().to_string(),
                id_tax: None,
                n_balance: 0.0,
                s_state: String::new(),
            };
            clients.push(client);
        }
        Ok(clients)
    }

    async fn find_client_by_id(&self, id: &str) -> Result<Client, String> {
        let obj_id = ObjectId::parse_str(id).map_err(|e| e.to_string())?;
        let filter = doc! { "_id": obj_id };

        match self.customers().find_one(filter).await {
            Ok(Some(doc)) => {
                let client = Client {
                    _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                    s_phone: doc.get_str("sPhone").unwrap_or_default().to_string(),
                    id_tax: doc.get_object_id("idTax").ok(),
                    n_balance: 0.0,
                    s_state: String::new(),
                };
                Ok(client)
            }
            Ok(None) => Ok(Client {
                _id: ObjectId::new(),
                s_phone: String::new(),
                id_tax: None,
                n_balance: 0.0,
                s_state: String::new(),
            }),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn find_tax_by_id(&self, tax_id: Option<ObjectId>) -> Result<Option<Tax>, String> {
        let db_bcv = self.client.database("BCV");
        let collection: Collection<Tax> = db_bcv.collection("IVA");

        let mut tax_doc = None;

        if let Some(id) = tax_id {
            let filter = doc! { "_id": id };
            if let Ok(found) = collection.find_one(filter).await {
                tax_doc = found;
            }
        }

        if tax_doc.is_none() {
            let filter = doc! { "sTarget": "DEFAULT" };

            if let Ok(found) = collection.find_one(filter).await {
                tax_doc = found;
            } else {
                return Ok(None);
            }
        }

        Ok(tax_doc)
    }

    async fn get_clients_by_phone_group(&self, id: String) -> Result<Vec<Document>, MongoError> {
        let collection: Collection<Document> = self.db.collection("Clients");
        let obj_id = ObjectId::parse_str(id)
            .map_err(|e| MongoError::custom(format!("Invalid ObjectId format: {}", e)))?;

        let pipeline = vec![
            // 1. Encontrar el documento del usuario autenticado para obtener su sPhone
            doc! { "$match": { "_id": obj_id } },
            doc! { "$project": { "_id": 0, "sPhone": 1 } },
            // 2. Usar $lookup para encontrar todos los clientes con ese sPhone
            doc! { "$lookup": {
                "from": "Clients",
                "localField": "sPhone",
                "foreignField": "sPhone",
                "as": "client_group"
            }},
            // 3. Desanidar el array de clientes
            doc! { "$unwind": "$client_group" },
            // 4. Reemplazar la raíz para que cada documento sea un cliente del grupo
            doc! { "$replaceRoot": { "newRoot": "$client_group" } },
            // 5. Opcional: Proyectar solo los campos necesarios (ID y nombre)
            doc! { "$project": {
                "_id": 1,
                "sName": 1,
                "sPhone": 1,
                "idTax": 1,
                "nBalance": 1 // Si el balance es un campo directo del cliente, tómalo.
            }},
        ];

        let mut cursor = collection.aggregate(pipeline).await?;
        let mut clients = Vec::new();
        while let Some(doc) = cursor.try_next().await? {
            clients.push(doc);
        }

        Ok(clients)
    }

    async fn get_last_payments_by_id_client(
        &self,
        id: String,
    ) -> Result<Vec<ResultGroupedByDate>, MongoError> {
        let obj_id = ObjectId::parse_str(&id)
            .map_err(|e| MongoError::custom(format!("Invalid ObjectId format: {}", e)))?;

        // Nuevo pipeline para obtener Payments Y PaymentReports del cliente.
        // Usaremos $unionWith para combinar ambas colecciones.
        let pipeline = vec![
            doc! { "$match": { "_id": obj_id } },
            // 1. Lookup de Payments (Filtrando solo Activos)
            doc! { "$lookup": {
                "from": "Payments",
                "let": { "client_id": "$_id" },
                "pipeline": [
                    doc! { "$match": {
                        "$expr": { "$eq": ["$idClient", "$$client_id"] },
                        "sState": "Activo" 
                    }},
                    doc! { "$project": {
                        "_id": 1, "sReason": 1, "nBs": 1, "sState": 1, "dCreation": 1,
                        "type": "Payment"
                    }},
                ],
                "as": "payments"
            }},
            // 2. Lookup de PaymentReports (Filtrando solo Activos)
            doc! { "$lookup": {
                "from": "PaymentReports",
                "let": { "client_id": "$_id" },
                "pipeline": [
                    doc! { "$match": {
                        "$expr": { "$eq": ["$idClient", "$$client_id"] },
                        "sState": "Pendiente" 
                    }},
                    doc! { "$lookup": {
                        "from": "Debts",
                        "localField": "idDebt",
                        "foreignField": "_id",
                        "as": "debt"
                    }},
                    doc! { "$unwind": {
                        "path": "$debt",
                        "preserveNullAndEmptyArrays": true
                    }},
                    doc! { "$project": {
                        "_id": 1,
                        "sReason": { "$ifNull": ["$debt.sReason", "$sReason"] },
                        "nBs": 1, "sState": 1, "dCreation": 1,
                        "type": "Report",
                    }},
                ],
                "as": "reports"
            }},
            // El resto del pipeline se mantiene igual
            doc! { "$project": {
                "all_transactions": { "$concatArrays": ["$payments", "$reports"] }
            }},
            doc! { "$unwind": "$all_transactions" },
            doc! { "$replaceRoot": { "newRoot": "$all_transactions" } },
            doc! { "$project": {
                "_id": 1,
                "reason": { "$ifNull": ["$sReason", "Abono"] },
                "balance_bs": "$nBs",
                "status": "$sState",
                "full_date": { "$toDate": "$dCreation" },
                "type": "$type",
                "date_group_key": { "$dateToString": { "format": "%Y-%m-%d", "date": { "$toDate": "$dCreation" }, "timezone": "America/Caracas" } }
            }},
            doc! { "$sort": { "full_date": -1 } },
            doc! { "$limit": 7 },
            doc! { "$group": {
                "_id": "$date_group_key",
                "payments": { "$push": {
                    "_id": "$_id", "reason": "$reason", "balance_bs": "$balance_bs",
                    "status": "$status", "full_date": "$full_date", "type": "$type"
                }}
            }},
            doc! { "$sort": { "_id": -1 } },
        ];

        let client_collection = self.db.collection::<Document>("Clients");
        let mut cursor = client_collection.aggregate(pipeline).await?;
        let mut results: Vec<ResultGroupedByDate> = Vec::new();

        while let Some(doc) = cursor.try_next().await? {
            let item: ResultGroupedByDate = bson::from_document(doc).map_err(|e| {
                MongoError::from(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    e.to_string(),
                ))
            })?;
            results.push(item);
        }
        Ok(results)
    }

    async fn get_solvency_counts(&self, owner_id: Option<&str>) -> Result<SolvencyCounts, String> {
        let mut match_doc = doc! { "sState": { "$in": ["Activo", "Suspendido"] } };
        if let Some(owner) = owner_id {
            match_doc.insert("idOwner", owner);
        }
        let pipeline = vec![
            doc! { "$match": match_doc },
            doc! {
                "$group": {
                    "_id": null,
                    "solventes": { "$sum": {
                        "$cond": [{ "$and": [
                            { "$eq": ["$sState", "Activo"] },
                            { "$gte": ["$nBalance", 0.0] }
                        ]}, 1, 0]
                    }},
                    "morosos": { "$sum": {
                        "$cond": [{ "$and": [
                            { "$eq": ["$sState", "Activo"] },
                            { "$lt": ["$nBalance", 0.0] }
                        ]}, 1, 0]
                    }},
                    "suspendidos": { "$sum": {
                        "$cond": [{ "$eq": ["$sState", "Suspendido"] }, 1, 0]
                    }},
                }
            },
        ];

        let collection: Collection<Document> = self.db.collection("Clients");
        let mut cursor = collection.aggregate(pipeline).await.map_err(|e| e.to_string())?;

        if let Some(Ok(doc)) = cursor.next().await {
            return Ok(SolvencyCounts {
                solventes: doc.get_i32("solventes").unwrap_or(0) as u32,
                morosos: doc.get_i32("morosos").unwrap_or(0) as u32,
                suspendidos: doc.get_i32("suspendidos").unwrap_or(0) as u32,
            });
        }

        Ok(SolvencyCounts { solventes: 0, morosos: 0, suspendidos: 0 })
    }

    async fn find_active_clients_for_closing(&self, owner_id: Option<&str>) -> Result<Vec<ActiveClientBalance>, String> {
        let mut filter = doc! { "sState": "Activo" };
        if let Some(owner) = owner_id {
            filter.insert("idOwner", owner);
        }
        let collection: Collection<Document> = self.db.collection("Clients");

        let mut cursor = collection
            .find(filter)
            .await
            .map_err(|e| e.to_string())?;

        let mut clients = Vec::new();
        while let Some(Ok(doc)) = cursor.next().await {
            if let Ok(id) = doc.get_object_id("_id") {
                clients.push(ActiveClientBalance {
                    id,
                    n_balance: get_bson_amount(&doc, "nBalance"),
                });
            }
        }
        Ok(clients)
    }

    async fn get_phone(&self, id: &str) -> Result<String, String> {
        let obj_id = ObjectId::parse_str(id).map_err(|e| e.to_string())?;
        let filter = doc! { "_id": obj_id };
        let result = self.customers().find_one(filter).await.ok().flatten();

        match result {
            Some(doc) => Ok(doc.get_str("sPhone").unwrap_or_default().to_string()),
            None => Err("Cliente no encontrado".to_string()),
        }
    }

    async fn get_all_clients(&self, owner_id: Option<&str>) -> Result<Vec<ClientListItem>, String> {
        let db = self.db.clone();

        let mut client_filter = doc! {
            "sState": { "$in": ["Activo", "Suspendido", "Retirado"] }
        };
        if let Some(owner) = owner_id {
            client_filter.insert("idOwner", owner);
        }

        let client_projection = doc! {
            "_id": 1, "sName": 1, "sDni": 1, "sRif": 1,
            "sState": 1, "nBalance": 1, "idSector": 1, "idSubscription": 1
        };

        // 3 queries en paralelo: clients + sectors + plans
        let clients_fut = {
            let db = db.clone();
            let filter = client_filter.clone();
            let proj = client_projection.clone();
            async move {
                db.collection::<Document>("Clients")
                    .find(filter)
                    .projection(proj)
                    .sort(doc! { "sName": 1 })
                    .await
                    .map_err(|e| e.to_string())
            }
        };

        let sectors_fut = {
            let db = db.clone();
            async move {
                db.collection::<Document>("Sectors")
                    .find(doc! {})
                    .projection(doc! { "_id": 1, "sName": 1 })
                    .await
                    .map_err(|e| e.to_string())
            }
        };

        let plans_fut = {
            let db = db.clone();
            async move {
                db.collection::<Document>("Plans")
                    .find(doc! {})
                    .projection(doc! { "_id": 1, "sName": 1, "nAmount": 1 })
                    .await
                    .map_err(|e| e.to_string())
            }
        };

        let (clients_res, sectors_res, plans_res) =
            tokio::join!(clients_fut, sectors_fut, plans_fut);

        let mut clients_cursor = clients_res?;
        let mut sectors_cursor = sectors_res?;
        let mut plans_cursor = plans_res?;

        // Sector HashMap: ObjectId hex -> sector_name
        let mut sectors: HashMap<String, String> = HashMap::new();
        while let Some(Ok(doc)) = sectors_cursor.next().await {
            if let Ok(id) = doc.get_object_id("_id") {
                if let Ok(name) = doc.get_str("sName") {
                    sectors.insert(id.to_hex(), name.to_string());
                }
            }
        }

        // Plans HashMap: ObjectId hex -> (plan_name, plan_price)
        let mut plans: HashMap<String, (String, f64)> = HashMap::new();
        while let Some(Ok(doc)) = plans_cursor.next().await {
            if let Ok(id) = doc.get_object_id("_id") {
                let name = doc.get_str("sName").unwrap_or_default().to_string();
                let price = get_bson_amount(&doc, "nAmount");
                plans.insert(id.to_hex(), (name, price));
            }
        }

        // Construir lista de clientes haciendo join en memoria
        let mut clients = Vec::with_capacity(512);
        while let Some(Ok(doc)) = clients_cursor.next().await {
            let id = doc
                .get_object_id("_id")
                .map(|o| o.to_hex())
                .unwrap_or_default();

            let name = doc.get_str("sName").unwrap_or_default().to_string();
            let balance = get_bson_amount(&doc, "nBalance");
            let s_state = doc.get_str("sState").unwrap_or_default();

            let status = match s_state {
                "Activo" if balance < 0.0 => "Moroso".to_string(),
                "Activo" => "Solvente".to_string(),
                other => other.to_string(),
            };

            let dni = doc
                .get_str("sDni")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| doc.get_str("sRif").ok().filter(|s| !s.is_empty()))
                .map(|s| s.to_string());

            let sector_name = doc
                .get_object_id("idSector")
                .ok()
                .and_then(|id| sectors.get(&id.to_hex()))
                .cloned();

            let plan_entry = doc
                .get_object_id("idSubscription")
                .ok()
                .and_then(|id| plans.get(&id.to_hex()));

            let plan_name = plan_entry.map(|(name, _)| name.clone());
            let plan_price = plan_entry
                .map(|(_, price)| *price)
                .filter(|&v| v > 0.0);

            clients.push(ClientListItem {
                id,
                name,
                dni,
                status,
                balance,
                sector_name,
                plan_name,
                plan_price,
            });
        }

        Ok(clients)
    }
}
