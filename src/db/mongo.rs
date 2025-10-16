use mongodb::{
    Client, Collection, Database,
    bson::{Document, doc},
};
use std::sync::Arc;

use super::Db;
use crate::crypto::jwt::JwtService;
use crate::domain::customer::Customer;

#[derive(Clone)]
pub struct MongoDB {
    client: Arc<Client>,
    db: Database,
}

#[derive(Debug, Clone)]
pub struct RefreshRecord {
    pub jti: String,
    pub sub: String,
    pub fam: String,
    pub exp: i64,
    pub revoked: bool,
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

    fn refresh_tokens(&self) -> Collection<Document> {
        self.db.collection::<Document>("refresh_tokens") // 👈 nueva colección
    }

    // --- Métodos RT (opcionales pero útiles) ---
    pub async fn save_refresh(&self, rec: RefreshRecord) {
        let _ = self
            .refresh_tokens()
            .insert_one(doc! {
                "jti": rec.jti,
                "sub": rec.sub,
                "fam": rec.fam,
                "exp": rec.exp,
                "revoked": rec.revoked,
            })
            .await;
    }

    pub async fn revoke_refresh(&self, jti: &str) {
        let _ = self
            .refresh_tokens()
            .update_one(doc! {"jti": jti}, doc! {"$set": {"revoked": true}})
            .await;
    }

    pub async fn is_refresh_valid(&self, jti: &str) -> bool {
        if let Ok(Some(doc)) = self.refresh_tokens().find_one(doc! {"jti": jti}).await {
            let revoked = doc.get_bool("revoked").unwrap_or(true);
            let exp = doc.get_i64("exp").unwrap_or(0);
            return !revoked && exp > JwtService::now();
        }
        false
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
}
