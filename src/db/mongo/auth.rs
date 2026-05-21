use async_trait::async_trait;
use chrono::Duration;
use mongodb::bson::doc;
use mongodb::bson::oid::ObjectId;

use super::MongoDB; // Acceso al struct padre
use crate::auth::claims::VerificationCode;
use crate::db::AuthRepository;
use crate::utils::timezone::VenezuelaDateTime;

#[async_trait]
impl AuthRepository for MongoDB {
    async fn store_verification_code(&self, phone: &str, code: &u32) -> mongodb::error::Result<()> {
        let now = VenezuelaDateTime::now();
        let expires = now.add_duration(Duration::minutes(60));

        let verification = VerificationCode {
            _id: None,
            phone: phone.to_string(),
            code: *code,
            created_at: now.utc(),
            expires_at: expires.utc(),
        };

        self.verification_codes().insert_one(verification).await?;
        Ok(())
    }

    async fn find_verification_code(&self, phone: &str, code: &u32) -> Option<VerificationCode> {
        let filter = doc! { "phone": phone, "code": code };
        self.verification_codes()
            .find_one(filter)
            .await
            .ok()
            .flatten()
    }

    async fn delete_verification_code(&self, id: &ObjectId) -> Result<u64, mongodb::error::Error> {
        let filter = doc! { "_id": id };
        let result = self.verification_codes().delete_one(filter).await?;
        Ok(result.deleted_count)
    }
}
