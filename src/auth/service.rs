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
}
