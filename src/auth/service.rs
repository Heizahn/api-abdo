use crate::db::Db;
use crate::domain::customer::Customer;

pub struct AuthService;

impl AuthService {
    pub async fn lookup_by_phone<D: Db>(db: &D, phone: &str) -> Option<Customer> {
        db.find_customer_by_phone(phone).await
    }
}
