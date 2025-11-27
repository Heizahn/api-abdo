use super::MongoDB;
use crate::db::mongo::ResultGroupedByDate;
use crate::db::ProfileRepository;
use crate::domain::customer::{Customer, CustomerView};
use crate::models::db::{Client, Tax};
use async_trait::async_trait;
use futures::stream::StreamExt;
use futures::TryStreamExt;
use mongodb::bson::{self, Document};
use mongodb::bson::{doc, oid::ObjectId};
use mongodb::error::Error as MongoError;
use mongodb::Collection;

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
                };
                Ok(client)
            }
            Ok(None) => Ok(Client {
                _id: ObjectId::new(),
                s_phone: String::new(),
                id_tax: None,
            }),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn find_tax_by_id(&self, tax_id: &ObjectId) -> Result<Option<Tax>, String> {
        let db_bcv = self.client.database("BCV");
        let collection: Collection<Tax> = db_bcv.collection("IVA");

        let mut tax_doc = None;

        let filter = doc! { "_id": tax_id };

        if let Ok(found) = collection.find_one(filter).await {
            tax_doc = found;
        }

        if tax_doc.is_none() {
            let filter = doc! { "sTarget": "DEFAULT" };

            if let Ok(found) = collection.find_one(filter).await {
                tax_doc = found;
            } else {
                return Ok(None);
            }
        }

        if tax_doc.is_none() {
            return Ok(None);
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

    async fn get_last_payments_by_id(
        &self,
        id: String,
    ) -> Result<Vec<ResultGroupedByDate>, MongoError> {
        let obj_id = ObjectId::parse_str(&id)
            .map_err(|e| MongoError::custom(format!("Invalid ObjectId format: {}", e)))?;

        // Nuevo pipeline para obtener Payments Y PaymentReports del cliente.
        // Usaremos $unionWith para combinar ambas colecciones.
        let pipeline = vec![
            doc! { "$match": { "_id": obj_id } },
            doc! { "$lookup": {
                "from": "Payments",
                "let": { "client_id": "$_id" },
                "pipeline": [
                    doc! { "$match": { "$expr": { "$eq": ["$idClient", "$$client_id"] } }},
                    doc! { "$project": {
                        "_id": 1, "sReason": 1, "nBs": 1, "sState": 1, "dCreation": 1,
                        // Marcar tipo de pago
                        "type": "Payment"
                    }},
                ],
                "as": "payments"
            }},
            doc! { "$lookup": {
                "from": "PaymentReports",
                "let": { "client_id": "$_id" },
                "pipeline": [
                    doc! { "$match": { "$expr": { "$eq": ["$idClient", "$$client_id"] } }},
                    doc! { "$project": {
                        "_id": 1, "sConcept": "$sConcept", "nAmount": "$nAmount", "sState": 1, "dCreation": 1,
                        // Marcar tipo de pago como Reporte
                        "type": "Report",
                        // Renombrar campos para uniformar con Payments.
                        "sReason": "$sConcept",
                        "nBs": "$nAmount"
                    }},
                ],
                "as": "reports"
            }},
            doc! { "$project": {
                "all_transactions": { "$concatArrays": ["$payments", "$reports"] }
            }},
            doc! { "$unwind": "$all_transactions" },
            doc! { "$replaceRoot": { "newRoot": "$all_transactions" } },
            doc! { "$project": {
                "_id": 1,
                "reason": "$sReason", // Ahora incluye sReason de Payments y sConcept de Reports
                "balance_bs": "$nBs", // Ahora incluye nBs de Payments y nAmount de Reports
                "status": "$sState",
                "full_date": "$dCreation",
                "type": "$type", // Incluir el tipo para distinguirlos
                "date_group_key": { "$dateToString": { "format": "%Y-%m-%d", "date": "$dCreation", "timezone": "America/Caracas" } }
            }},
            doc! { "$sort": { "full_date": -1 } },
            doc! { "$limit": 20 }, // Aumentar el límite total si se combinan dos colecciones.
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

    async fn get_phone(&self, id: &str) -> Result<String, String> {
        let obj_id = ObjectId::parse_str(id).map_err(|e| e.to_string())?;
        let filter = doc! { "_id": obj_id };
        let result = self.customers().find_one(filter).await.ok().flatten();

        match result {
            Some(doc) => Ok(doc.get_str("sPhone").unwrap_or_default().to_string()),
            None => Err("Cliente no encontrado".to_string()),
        }
    }
}
