pub mod mongo;
use crate::domain::customer::{Customer, CustomerView};

#[async_trait::async_trait]
pub trait Db: Clone + Send + Sync + 'static {
    async fn find_customer_by_phone(&self, phone: &str) -> Option<Customer>;
    async fn find_customer_by_id(&self, id: &str) -> Option<CustomerView>;
    async fn summary_by_phone(&self, phone: &str) -> Option<mongo::PhoneSummary>;
}
