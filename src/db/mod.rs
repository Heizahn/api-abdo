pub mod mongo;
use crate::models::db::{ActiveClientBalance, LatestVersion, OnuForUpdateIp, OnuIdentity, OnuIpUpdate, Tax};

use crate::models::payment::{Bank, PaymentReport};
use crate::models::users::{User, UserCredentials}; // Import
use crate::services::zte_parse_update::OnuDetected;
use crate::{
    auth::claims::VerificationCode,
    db::mongo::ResultGroupedByDate,
    domain::customer::{Customer, CustomerView},
    models::db::{Client, Debt, PartPayment, Payment},
    models::payment::{ClientOwner, PaymentMethod, UserPaymentInfo},
};
use mongodb::bson::oid::ObjectId;
use mongodb::bson::Document;
use mongodb::error::Error as MongoError;
use mongodb::results::InsertOneResult;
use crate::error::ApiError;

// ============================================
// 1. AuthRepository: Login, Verificación
// ============================================
#[async_trait::async_trait]
pub trait AuthRepository {
    async fn store_verification_code(&self, phone: &str, code: &u32) -> mongodb::error::Result<()>;
    async fn find_verification_code(&self, phone: &str, code: &u32) -> Option<VerificationCode>;
    async fn delete_verification_code(&self, id: &ObjectId) -> Result<u64, MongoError>;
}

// ============================================
// 6. UserRepository: Auth Users (Admin/Staff)
// ============================================
#[async_trait::async_trait]
pub trait UserRepository {
    async fn find_user_by_email(&self, email: &str) -> Result<Option<User>, String>;
    async fn find_user_credentials_by_user_id(
        &self,
        user_id: &str,
    ) -> Result<Option<UserCredentials>, String>;
    async fn find_user_by_id(&self, id: &str) -> Result<Option<User>, String>;
    async fn create_user(&self, user: User) -> Result<(), String>;
    async fn create_user_credentials(&self, creds: UserCredentials) -> Result<(), String>;
    async fn find_providers(&self) -> Result<Vec<User>, String>;
}

// ============================================
// 2. ProfileRepository: Clientes
// ============================================
#[async_trait::async_trait]
pub trait ProfileRepository {
    async fn find_customer_by_phone(&self, phone: &str) -> Option<Customer>;
    async fn find_customer_by_id(&self, id: &str) -> Option<CustomerView>;

    async fn find_clients_by_phone(&self, s_phone: &str) -> Result<Vec<Client>, String>;
    async fn find_client_by_id(&self, id: &str) -> Result<Client, String>;
    async fn find_tax_by_id(&self, id: Option<ObjectId>) -> Result<Option<Tax>, String>;

    async fn get_clients_by_phone_group(&self, phone: String) -> Result<Vec<Document>, MongoError>;
    async fn get_last_payments_by_id_client(
        &self,
        id: String,
    ) -> Result<Vec<ResultGroupedByDate>, MongoError>;

    async fn get_phone(&self, id: &str) -> Result<String, String>;

    async fn find_active_clients_for_closing(&self) -> Result<Vec<ActiveClientBalance>, String>;
}

// ============================================
// 3. SalesRepository: Ventas, Pagos, Deudas
// ============================================
#[async_trait::async_trait]
pub trait SalesRepository {
    async fn get_latest_exchange_rate(&self) -> Result<f64, MongoError>;

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

    async fn sum_active_payments_in_range(
        &self,
        client_ids: &[mongodb::bson::oid::ObjectId],
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> Result<f64, String>;

    async fn find_pending_reports_by_debt_ids(
        &self,
        debt_ids: &[ObjectId],
    ) -> Result<Vec<PaymentReport>, String>;
}

// ============================================
// 4. UtilsRepository: Utils
// ============================================
#[async_trait::async_trait]
pub trait UtilsRepository {
    async fn find_latest_version(&self) -> Result<Option<LatestVersion>, String>;

    async fn exists_rate_for_date(
        &self,
        date_start: chrono::DateTime<chrono::Utc>,
        date_end: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, String>;
    async fn save_exchange_rate(
        &self,
        rate: f64,
        date: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), mongodb::error::Error>;

    async fn find_client_olt_position(&self, client_id: &str) -> Result<(String, String), ApiError>;
}

// ============================================
// 5. OnuRepository: Onu
// ============================================
#[async_trait::async_trait]
pub trait OnuRepository {
    // ZTE / Devices
    async fn get_device_serial_numbers(&self) -> Result<Vec<OnuIdentity>, String>;
    async fn save_onu_from_zte(&self, onu: OnuDetected, id_editor: &str) -> Result<(), String>;

    // IP Update
    async fn get_onus_for_update_ip(&self) -> Result<Vec<OnuForUpdateIp>, String>;
    async fn update_onu_ip(&self, onu: OnuIpUpdate, id_editor: &str) -> Result<(), String>;
}

// ============================================
// TRAIT MAESTRO
// ============================================
pub trait Db:
    AuthRepository
    + UserRepository
    + ProfileRepository
    + SalesRepository
    + OnuRepository
    + UtilsRepository
    + Clone
    + Send
    + Sync
    + 'static
{
}
