use serde::{Deserialize, Serialize};

// Re-export ObjectId for backward compatibility
pub use mongodb::bson::oid::ObjectId;

// --- 1. Estructuras de Datos (Modelos) ---

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub _id: ObjectId,
    pub s_phone: String, // El número de teléfono del cliente
                         // ... otros campos irrelevantes para esta lógica
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Debt {
    pub _id: ObjectId,
    pub n_amount: f64,   // Monto total de la deuda
    pub s_state: String, // Estado de la deuda (no usado en el cálculo, pero útil para la lógica)
    pub id_client: ObjectId,
    pub s_reason: String,
    // ... otros campos
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PartPayment {
    pub _id: ObjectId,
    pub id_debt: ObjectId,
    pub id_payment: ObjectId,
    pub n_amount: f64, // Monto de la parte de pago
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Payment {
    pub _id: ObjectId,
    pub n_amount: f64, // Monto del pago
    pub s_state: String, // Estado del pago: "Activo" o "Anulado"
                       // ... otros campos
}

// Estructura de respuesta para la deuda activa
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveDebtResponse {
    pub debt: Debt,
    pub active_debt_amount: f64,
}

// --- 3. Dependencias de Simulación (para que el código compile) ---

// Definiciones simuladas de las dependencias que usaste en tu código:
#[derive(Debug)]
pub struct Request;
impl Request {
    pub fn header(&self, _name: &str) -> Option<&str> {
        None
    }
}

#[derive(Debug)]
pub struct Response;
impl Response {
    pub fn json(_status: u16, _body: &str) -> Self {
        Response
    }
}
