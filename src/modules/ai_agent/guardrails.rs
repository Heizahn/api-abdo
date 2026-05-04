//! Server-side guardrails para tool calls del AI Agent + bloque [turn_state]
//! del prompt. Pure helpers — sin I/O. Toda la data viene precomputada por
//! `dispatch.rs` desde la `recent` slice del turno.

use crate::models::whatsapp::WaMessage;
use super::runner::{ConvRole, ConvTurn};
use super::tools::normalize_zone;

/// Mapping intent group → trigger substrings. Substrings ya normalizados
/// (lowercase, sin tildes). Claves en español alineadas con los prompts
/// del proyecto. Modificar con cuidado: cada cambio impacta el HUD que
/// lee Gemini en CADA turno.
const INTENT_KEYWORDS: &[(&str, &[&str])] = &[
    ("internet",    &["internet", "conexion", "wifi", "red"]),
    ("contratar",   &["contratar", "contrato", "instalar", "instalacion", "nuevo servicio", "instalan"]),
    ("precio",      &["precio", "costo", "cuanto", "vale"]),
    ("cobertura",   &["cobertura", "llegan", "llega", "cubren", "zona"]),
    ("factura",     &["factura", "facturacion"]),
    ("pago",        &["pago", "pagar", "pague", "comprobante", "deposito", "transferencia", "transferi", "abono", "referencia"]),
    ("saldo",       &["saldo", "debo", "deuda", "mora"]),
    ("planes",      &["plan", "planes", "mbps", "megas", "velocidad"]),
    ("soporte",     &["soporte", "no anda", "no funciona", "no tengo internet", "sin internet",
                      "lento", "se cayo", "no me anda", "no carga", "falla", "averia", "problema"]),
    ("humano",      &["humano", "persona", "asesor", "operador", "hablar con alguien", "agente", "supervisor"]),
    ("plan_change", &["cambiar de plan", "subir plan", "bajar plan", "upgrade", "downgrade"]),
    ("account",     &["actualizar", "cambiar datos", "mi correo", "mi telefono", "mi direccion"]),
    ("cancel",      &["cancelar", "dar de baja", "retirar"]),
];

/// Devuelve los bodies normalizados (lowercase + sin tildes + trim) de los
/// mensajes inbound del cliente. Ignora mensajes sin body o vacíos. Cada
/// elemento es el mensaje completo — el matching es substring bidireccional
/// contra la `zone` que mande Gemini (ver `validate_zone_mentioned`).
pub fn extract_customer_explicit_zones(messages: &[WaMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.direction == "in")
        .filter_map(|m| m.body.as_deref())
        .map(|s| normalize_zone(s))
        .filter(|s| !s.is_empty())
        .collect()
}

/// media_ids únicos (en orden de aparición) de los mensajes inbound del
/// cliente con archivo adjunto. La unicidad es defensiva: Meta no debería
/// duplicar pero el dedupe local protege contra retries del webhook.
pub fn extract_recent_media_ids(messages: &[WaMessage]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for m in messages.iter().filter(|m| m.direction == "in") {
        if let Some(mid) = m.media_id.as_deref() {
            let mid = mid.trim();
            if !mid.is_empty() && !seen.iter().any(|s| s == mid) {
                seen.push(mid.to_string());
            }
        }
    }
    seen
}

/// Bidirectional substring match con `normalize_zone`. true si la zona
/// reclamada por la IA está mencionada (literal o como parte de un texto
/// más largo) por el cliente, o viceversa.
pub fn validate_zone_mentioned(claimed_zone: &str, customer_zones: &[String]) -> bool {
    let n_claimed = normalize_zone(claimed_zone);
    if n_claimed.is_empty() {
        return false;
    }
    customer_zones.iter().any(|raw| {
        // raw ya viene normalizado desde extract_customer_explicit_zones,
        // pero re-normalizamos por defensiva (función puede ser llamada
        // con datos crudos en otro contexto).
        let n_cust = normalize_zone(raw);
        if n_cust.is_empty() {
            return false;
        }
        n_claimed.contains(&n_cust) || n_cust.contains(&n_claimed)
    })
}

/// Scanea bodies de mensajes inbound y devuelve los GROUP KEYS detectados
/// (sin duplicados, en orden estable de declaración del table). Ver
/// `INTENT_KEYWORDS`.
pub fn extract_customer_explicit_intents(messages: &[WaMessage]) -> Vec<String> {
    // Unimos todos los bodies inbound en un buffer normalizado para barrer
    // INTENT_KEYWORDS una sola vez (n_groups × n_triggers) en lugar de
    // n_messages × n_groups × n_triggers.
    let buffer: String = messages
        .iter()
        .filter(|m| m.direction == "in")
        .filter_map(|m| m.body.as_deref())
        .map(|s| normalize_zone(s))
        .collect::<Vec<_>>()
        .join(" ");
    if buffer.is_empty() {
        return Vec::new();
    }

    let mut hits: Vec<String> = Vec::new();
    for (group, triggers) in INTENT_KEYWORDS {
        if triggers.iter().any(|t| buffer.contains(t)) {
            hits.push((*group).to_string());
        }
    }
    hits
}

/// Construye el bloque `[turn_state]` body (sin la cabecera `[turn_state]`,
/// que la pega `runner::build_system_instruction`). Devuelve `None` cuando
/// es turn_number 1 y no hay zones ni intents — evitar inyectar HUD vacío.
pub fn build_turn_state(
    history: &[ConvTurn],
    customer_zones: &[String],
    customer_intents: &[String],
) -> Option<String> {
    let turn_number = history.iter().filter(|t| t.role == ConvRole::User).count() + 1;
    if turn_number == 1 && customer_zones.is_empty() && customer_intents.is_empty() {
        return None;
    }
    let mut lines = vec![format!("turn_number: {}", turn_number)];
    if !customer_zones.is_empty() {
        lines.push(format!("customer_explicit_zones: {}", customer_zones.join(", ")));
    }
    if !customer_intents.is_empty() {
        lines.push(format!("customer_explicit_intents: {}", customer_intents.join(", ")));
    }
    Some(lines.join("\n"))
}
