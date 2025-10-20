use super::Db;
use crate::{
    auth::claims::VerificationCode,
    domain::customer::{Customer, CustomerView},
};
use chrono::{Duration, Utc};
use futures::stream::StreamExt;
use mongodb::{
    Client, Collection, Database,
    bson::{Document, doc},
};
use std::sync::Arc;

#[derive(Clone)]
pub struct MongoDB {
    #[allow(dead_code)]
    client: Arc<Client>,
    db: Database,
}

pub struct PhoneSummary {
    pub primary_name: String, // nombre del primero
    pub phone: String,
    pub total_balance: f64, // suma de nBalance
    pub count: i64,         // cuántos clientes comparten ese phone
}

impl MongoDB {
    pub async fn new(uri: &str, db_name: &str) -> Self {
        let client = Client::with_uri_str(uri)
            .await
            .expect("Error conectando a MongoDB");
        let db = client.database(db_name);
        Self {
            client: Arc::new(client),
            db,
        }
    }

    fn customers(&self) -> Collection<Document> {
        self.db.collection::<Document>("Clients")
    }

    fn verification_codes(&self) -> Collection<VerificationCode> {
        self.db.collection::<VerificationCode>("verification_codes")
    }
}

#[async_trait::async_trait]
impl Db for MongoDB {
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
        let obj_id = mongodb::bson::oid::ObjectId::parse_str(id).ok()?;
        let filter = doc! { "_id": obj_id };
        let result = self.customers().find_one(filter).await.ok()??;

        Some(CustomerView {
            full_name: result.get_str("sName").unwrap_or_default().to_string(),
            phone: result.get_str("sPhone").unwrap_or_default().to_string(),
            balance: result.get_f64("nBalance").unwrap_or(0.0),
        })
    }

    async fn summary_by_phone(&self, phone: &str) -> Option<PhoneSummary> {
        let pipeline = vec![
            doc! { "$match": { "sPhone": phone } },
            // Ordenamos para que el "primero" sea estable
            doc! { "$sort": { "_id": 1 } },
            doc! { "$group": {
                "_id": "$sPhone",
                "firstName": { "$first": "$sName" },
                "phone":     { "$first": "$sPhone" },
                "totalBalance": { "$sum": { "$ifNull": ["$nBalance", 0.0] } },
                "count": { "$sum": 1 }
            }},
        ];

        let mut cursor = self.customers().aggregate(pipeline).await.ok()?;
        let Some(Ok(doc)) = cursor.next().await else {
            return None;
        };

        Some(PhoneSummary {
            primary_name: doc.get_str("firstName").unwrap_or_default().to_string(),
            phone: doc.get_str("phone").unwrap_or_default().to_string(),
            total_balance: doc.get_f64("totalBalance").unwrap_or(0.0),
            count: doc.get_i64("count").unwrap_or(0),
        })
    }

    async fn store_verification_code(&self, phone: &str, code: &u32) -> mongodb::error::Result<()> {
        let now = Utc::now();
        let verification = VerificationCode {
            phone: phone.to_string(),
            code: code.to_string(),
            created_at: now,
            expires_at: now + Duration::minutes(3),
        };

        self.verification_codes().insert_one(verification).await?;
        Ok(())
    }
}
