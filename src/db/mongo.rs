use mongodb::{
    Client, Collection, Database,
    bson::{Document, doc},
};
use std::sync::Arc;

use super::Db;
use crate::domain::customer::Customer;

#[derive(Clone)]
pub struct MongoDB {
    #[allow(dead_code)]
    client: Arc<Client>,
    db: Database,
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
}

#[async_trait::async_trait]
impl Db for MongoDB {
    async fn find_customer_by_phone(&self, phone: &str) -> Option<Customer> {
        let filter = doc! { "sPhone": phone };
        let result = self.customers().find_one(filter).await.ok()??;

        Some(Customer {
            id: result.get_object_id("_id").ok().clone(),
            full_name: result.get_str("sName").unwrap_or_default().to_string(),
            phone: result.get_str("sPhone").unwrap_or_default().to_string(),
        })
    }
}
