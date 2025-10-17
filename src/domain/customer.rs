use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Customer {
    pub id: String,
    pub full_name: String,
    pub phone: String,
}

pub struct CustomerView {
    pub full_name: String,
    pub phone: String,
    pub balance: f64,
}
