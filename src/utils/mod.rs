pub mod bcv_scraper;
pub mod get_bson_amount;
pub mod sms;
pub mod timezone;
pub mod whatsapp;

use rand::{rng, Rng};

/// Genera un código de verificación de 6 dígitos
pub fn generate_verification_code() -> u32 {
    rng().random_range(100_000..1_000_000)
}
