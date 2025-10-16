pub mod mongo;
use crate::domain::customer::Customer;

#[async_trait::async_trait]
pub trait Db: Clone + Send + Sync + 'static {
    async fn find_customer_by_phone(&self, phone: &str) -> Option<Customer>;
}
