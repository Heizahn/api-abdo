use mongodb::bson::Document;

pub fn get_bson_amount(doc: &Document, field_name: &str) -> f64 {
    if let Ok(val) = doc.get_f64(field_name) {
        return val;
    }
    if let Ok(val) = doc.get_i32(field_name) {
        return val as f64;
    }
    if let Ok(val) = doc.get_i64(field_name) {
        return val as f64;
    }
    // Si llegamos aquí, el campo no existe o no es un número
    0.0
}
