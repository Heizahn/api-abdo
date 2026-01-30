use async_trait::async_trait;
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

    async fn find_user_credentials_by_user_id(&self, user_id: &str) -> Result<Option<UserCredentials>, String> {
        let collection: Collection<UserCredentials> = self.db.collection("UserCredentials");
        let filter = doc! { "userId": user_id };
        
        match collection.find_one(filter).await {
            Ok(res) => Ok(res),
            Err(e) => {
                tracing::error!("❌ Error finding credentials for user_id {}: {:?}", user_id, e);
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
}
