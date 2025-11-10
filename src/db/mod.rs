pub mod mongo;
use crate::{
    auth::claims::VerificationCode,
    db::mongo::ResultGroupedByDate,
    domain::customer::{Customer, CustomerView},
    models::db::{Client, Debt, PartPayment, Payment},
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

    // The following methods are implemented for future endpoints
    // that will display debt information and payment details.

    // 1. Obtener el cliente por ID de usuario (user_id)
    #[allow(dead_code)]
    async fn find_client_by_user_id(&self, user_id: &str) -> Result<Option<Client>, String>;

    // 2. Obtener todos los clientes con un sPhone específico
    #[allow(dead_code)]
    async fn find_clients_by_phone(&self, s_phone: &str) -> Result<Vec<Client>, String>;

    // 3. Obtener todas las deudas para una lista de IDs de cliente
    #[allow(dead_code)]
    async fn find_debts_by_client_ids(&self, client_ids: &[ObjectId]) -> Result<Vec<Debt>, String>;

    // 4. Obtener todas las partes de pago para una lista de IDs de deuda
    #[allow(dead_code)]
    async fn find_part_payments_by_debt_ids(
        &self,
        debt_ids: &[ObjectId],
    ) -> Result<Vec<PartPayment>, String>;

    // 5. Obtener los pagos por una lista de IDs de pago
    #[allow(dead_code)]
    async fn find_payments_by_ids(&self, payment_ids: &[ObjectId]) -> Result<Vec<Payment>, String>;

    async fn find_active_debts_by_client_ids(
        &self,
        client_ids: &[ObjectId],
    ) -> Result<Vec<Debt>, String>;
}
