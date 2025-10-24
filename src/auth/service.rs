use mongodb::bson::oid::ObjectId;

use crate::auth::claims::VerificationCode;
use crate::db::Db;
use crate::domain::customer::{Customer, CustomerView};

pub struct AuthService;

impl AuthService {
    pub async fn lookup_by_phone<D: Db>(db: &D, phone: &str) -> Option<Customer> {
        db.find_customer_by_phone(phone).await
    }

    pub async fn lookup_by_id<D: Db>(db: &D, id: &str) -> Option<CustomerView> {
        db.find_customer_by_id(id).await
    }

    pub async fn lookup_verification_code<D: Db>(
        db: &D,
        phone: &str,
        code: &u32,
    ) -> Option<VerificationCode> {
        // Usando VerificationCode
        // Llama al método del trait
        db.find_verification_code(phone, code).await
    }

    // (Opcional pero recomendado)
    pub async fn delete_verification_code<D: Db>(db: &D, id: &ObjectId) {
        // Llama al método del trait
        // Ignoramos el resultado (como en tu ejemplo)
        let _ = db.delete_verification_code(id).await;
    }
}
