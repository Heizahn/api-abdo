pub mod mongo;
use crate::{
    auth::claims::VerificationCode,
    db::mongo::ResultGroupedByDate,
    domain::customer::{Customer, CustomerView},
};
use mongodb::bson::oid::ObjectId;
use mongodb::error::Error as MongoError;

#[async_trait::async_trait]
pub trait Db: Clone + Send + Sync + 'static {
    async fn find_customer_by_phone(&self, phone: &str) -> Option<Customer>;
    async fn find_customer_by_id(&self, id: &str) -> Option<CustomerView>;
    async fn summary_by_phone(&self, phone: &str) -> Option<mongo::PhoneSummary>;
    async fn store_verification_code(&self, phone: &str, code: &u32) -> mongodb::error::Result<()>;

    /// Busca un código de verificación por teléfono y código.
    async fn find_verification_code(&self, phone: &str, code: &u32) -> Option<VerificationCode>;

    /// Elimina un código de verificación por su ID de MongoDB.
    async fn delete_verification_code(&self, id: &ObjectId) -> Result<u64, MongoError>;

    //Traer el balance en USD
    async fn get_user_balance_usd(&self, id: String) -> Result<f64, MongoError>;

    async fn get_latest_exchange_rate(&self) -> Result<f64, MongoError>;

    async fn get_last_payments_by_id(
        &self,
        id: String,
    ) -> Result<Vec<ResultGroupedByDate>, MongoError>;
}
