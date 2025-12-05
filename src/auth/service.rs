use mongodb::bson::oid::ObjectId;

use crate::auth::claims::VerificationCode;
use crate::db::Db;
use crate::domain::customer::{Customer, CustomerView};
use crate::utils::timezone::VenezuelaDateTime;

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

    pub fn is_code_expired(verification_code: &VerificationCode) -> bool {
        let now = VenezuelaDateTime::now();
        let expires = VenezuelaDateTime::from_utc(verification_code.expires_at);

        now.is_after(&expires)
    }

    #[allow(dead_code)]
    pub fn get_code_time_info(verification: &VerificationCode) -> CodeTimeInfo {
        let created = VenezuelaDateTime::from_utc(verification.created_at);
        let expires = VenezuelaDateTime::from_utc(verification.expires_at);
        let now = VenezuelaDateTime::now();

        CodeTimeInfo {
            created_at_vz: created.datetime_string_venezuela(),
            expires_at_vz: expires.datetime_string_venezuela(),
            now_vz: now.datetime_string_venezuela(),
            is_expired: now.is_after(&expires),
            created_at_utc: verification.created_at,
            expires_at_utc: verification.expires_at,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct CodeTimeInfo {
    pub created_at_vz: String,
    pub expires_at_vz: String,
    pub now_vz: String,
    pub is_expired: bool,
    pub created_at_utc: chrono::DateTime<chrono::Utc>,
    pub expires_at_utc: chrono::DateTime<chrono::Utc>,
}