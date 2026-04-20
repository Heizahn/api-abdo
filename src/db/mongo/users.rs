use async_trait::async_trait;
use futures::stream::TryStreamExt;
use mongodb::bson::doc;
use mongodb::Collection;

use super::MongoDB;
use crate::db::UserRepository;
use crate::models::users::{User, UserCredentials};

#[async_trait]
impl UserRepository for MongoDB {
    async fn find_user_by_email(&self, email: &str) -> Result<Option<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        let filter = doc! { "email": email };

        match collection.find_one(filter).await {
            Ok(res) => Ok(res),
            Err(e) => {
                tracing::error!("❌ Error finding user by email {}: {:?}", email, e);
                Err(e.to_string())
            }
        }
    }

    async fn find_user_credentials_by_user_id(
        &self,
        user_id: &str,
    ) -> Result<Option<UserCredentials>, String> {
        let collection: Collection<UserCredentials> = self.db.collection("UserCredentials");
        let filter = doc! { "userId": user_id };

        match collection.find_one(filter).await {
            Ok(res) => Ok(res),
            Err(e) => {
                tracing::error!(
                    "❌ Error finding credentials for user_id {}: {:?}",
                    user_id,
                    e
                );
                Err(e.to_string())
            }
        }
    }

    async fn find_user_by_id(&self, id: &str) -> Result<Option<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        // LB4 uses string IDs (UUIDs)
        let filter = doc! { "_id": id };

        collection
            .find_one(filter)
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_user(&self, user: User) -> Result<(), String> {
        let collection: Collection<User> = self.db.collection("Users");
        collection
            .insert_one(user)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn create_user_credentials(&self, creds: UserCredentials) -> Result<(), String> {
        let collection: Collection<UserCredentials> = self.db.collection("UserCredentials");
        collection
            .insert_one(creds)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn find_agents(&self) -> Result<Vec<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        // nRole >= 0 AND nRole < 3 (excluye providers con nRole == 3.0)
        let filter = doc! { "nRole": { "$gte": 0.0, "$lt": 3.0 }, "visible": true };

        collection
            .find(filter)
            .sort(doc! { "sName": 1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_providers(&self) -> Result<Vec<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        let filter = doc! { "nRole": 3.0 };

        let mut cursor = collection
            .find(filter)
            .sort(doc! { "nTag": 1})
            .await
            .map_err(|e| e.to_string())?;

        let mut users = Vec::new();
        while let Some(user) = cursor.try_next().await.map_err(|e| e.to_string())? {
            users.push(user);
        }
        Ok(users)
    }
}
