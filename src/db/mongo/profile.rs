use async_trait::async_trait;
use futures::stream::StreamExt;
use mongodb::bson::{doc, oid::ObjectId};
use mongodb::Collection;

use super::{MongoDB, PhoneSummary};
use crate::db::ProfileRepository;
use crate::domain::customer::{Customer, CustomerView};
use crate::models::db::{Client, Tax};

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

    async fn summary_by_phone(&self, phone: &str) -> Option<PhoneSummary> {
        let pipeline = vec![
            doc! { "$match": { "sPhone": phone } },
            doc! { "$sort": { "_id": 1 } },
            doc! { "$group": {
                "_id": "$sPhone",
                "firstName": { "$first": "$sName" },
                "phone":     { "$first": "$sPhone" },
            }},
        ];

        let mut cursor = self.customers().aggregate(pipeline).await.ok()?;
        let Some(Ok(doc)) = cursor.next().await else {
            return None;
        };

        Some(PhoneSummary {
            primary_name: doc.get_str("firstName").unwrap_or_default().to_string(),
            phone: doc.get_str("phone").unwrap_or_default().to_string(),
        })
    }

    // async fn find_client_by_user_id(&self, user_id: &str) -> Result<Option<Client>, String> {
    //     let obj_id = ObjectId::parse_str(user_id).map_err(|e| e.to_string())?;
    //     let filter = doc! { "_id": obj_id };
    //
    //     match self.customers().find_one(filter).await {
    //         Ok(Some(doc)) => {
    //             let client = Client {
    //                 _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
    //                 s_phone: doc.get_str("sPhone").unwrap_or_default().to_string(),
    //             };
    //             Ok(Some(client))
    //         }
    //         Ok(None) => Ok(None),
    //         Err(e) => Err(e.to_string()),
    //     }
    // }

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

    async fn find_tax_by_id(&self, id: &ObjectId) -> Result<Option<Tax>, String> {
        let db_bcv = self.client.database("BCV");
        let collection: Collection<Tax> = db_bcv.collection("IVA");
        let filter = doc! { "_id": id };

        collection.find_one(filter).await.map_err(|e| e.to_string())
    }
}
