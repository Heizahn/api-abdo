pub mod mongo;

use crate::models::payment::{Bank, PaymentReport};
use crate::{
    auth::claims::VerificationCode,
    db::mongo::{PhoneSummary, ResultGroupedByDate},
    domain::customer::{Customer, CustomerView},
    models::db::{Client, Debt, PartPayment, Payment},
    models::payment::{ClientOwner, PaymentMethod, UserPaymentInfo},
};
use mongodb::bson::oid::ObjectId;
use mongodb::error::Error as MongoError;
use mongodb::results::InsertOneResult;

// ============================================
// 1. AuthRepository: Login, Verificación
// ============================================
#[async_trait::async_trait]
pub trait AuthRepository {
    async fn store_verification_code(&self, phone: &str, code: &u32) -> mongodb::error::Result<()>;

    /// Busca un código de verificación por teléfono y código.
    async fn find_verification_code(&self, phone: &str, code: &u32) -> Option<VerificationCode>;

    /// Elimina un código de verificación por su ID de MongoDB.
    async fn delete_verification_code(&self, id: &ObjectId) -> Result<u64, MongoError>;
}

// ============================================
// 2. ProfileRepository: Clientes
// ============================================
#[async_trait::async_trait]
pub trait ProfileRepository {
    async fn find_customer_by_phone(&self, phone: &str) -> Option<Customer>;
    async fn find_customer_by_id(&self, id: &str) -> Option<CustomerView>;
    async fn summary_by_phone(&self, phone: &str) -> Option<PhoneSummary>;

    // Métodos para futuros endpoints de perfil
    // async fn find_client_by_user_id(&self, user_id: &str) -> Result<Option<Client>, String>;
    async fn find_clients_by_phone(&self, s_phone: &str) -> Result<Vec<Client>, String>;
}

// ============================================
// 3. SalesRepository: Ventas, Pagos, Deudas
// ============================================
#[async_trait::async_trait]
pub trait SalesRepository {
    // Balance y Moneda
    async fn get_user_balance_usd(&self, id: String) -> Result<f64, MongoError>;
    async fn get_latest_exchange_rate(&self) -> Result<f64, MongoError>;

    // Deudas
    // async fn find_debts_by_client_ids(&self, client_ids: &[ObjectId]) -> Result<Vec<Debt>, String>;
    async fn find_active_debts_by_client_ids(
        &self,
        client_ids: &[ObjectId],
    ) -> Result<Vec<Debt>, String>;

    // Pagos
    async fn find_part_payments_by_debt_ids(
        &self,
        debt_ids: &[ObjectId],
    ) -> Result<Vec<PartPayment>, String>;
    async fn find_payments_by_ids(&self, payment_ids: &[ObjectId]) -> Result<Vec<Payment>, String>;
    async fn get_last_payments_by_id(
        &self,
        id: String,
    ) -> Result<Vec<ResultGroupedByDate>, MongoError>;

    async fn find_debt_by_id(&self, id: &str) -> Result<Option<crate::models::db::Debt>, String>;
    async fn find_client_owner_by_id(
        &self,
        client_id: &ObjectId,
    ) -> Result<Option<ClientOwner>, String>;

    // CAMBIOS:
    async fn find_user_payment_info_by_id(
        &self,
        user_id: &str,
    ) -> Result<Option<UserPaymentInfo>, String>;
    async fn find_payment_method_by_id(
        &self,
        id: &ObjectId,
    ) -> Result<Option<PaymentMethod>, String>;
    async fn create_payment_report(
        &self,
        report: PaymentReport,
    ) -> Result<InsertOneResult, MongoError>;

    async fn find_bank_list(&self) -> Result<Vec<Bank>, String>;
}

// ============================================
// TRAIT MAESTRO
// ============================================
pub trait Db:
    AuthRepository + ProfileRepository + SalesRepository + Clone + Send + Sync + 'static
{
}
