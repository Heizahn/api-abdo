//! Server-side guardrails para tool calls del AI Agent + bloque [turn_state]
//! del prompt. Pure helpers — sin I/O. Toda la data viene precomputada por
//! `dispatch.rs` desde la `recent` slice del turno.

use super::runner::{ConvRole, ConvTurn};
use super::tools::normalize_zone;
use crate::models::whatsapp::WaMessage;

/// Mapping intent group → trigger substrings. Substrings ya normalizados
/// (lowercase, sin tildes). Claves en español alineadas con los prompts
/// del proyecto. Modificar con cuidado: cada cambio impacta el HUD que
/// lee el LLM en CADA turno.
const INTENT_KEYWORDS: &[(&str, &[&str])] = &[
    ("internet", &["internet", "conexion", "wifi", "red"]),
    (
        "contratar",
        &[
            "contratar",
            "contrato",
            "instalar",
            "instalacion",
            "nuevo servicio",
            "instalan",
        ],
    ),
    ("precio", &["precio", "costo", "cuanto", "vale"]),
    (
        "cobertura",
        &["cobertura", "llegan", "llega", "cubren", "zona"],
    ),
    ("factura", &["factura", "facturacion"]),
    (
        "pago",
        &[
            "pago",
            "pagar",
            "pague",
            "comprobante",
            "deposito",
            "transferencia",
            "transferi",
            "abono",
            "referencia",
        ],
    ),
    ("saldo", &["saldo", "debo", "deuda", "mora"]),
    ("planes", &["plan", "planes", "mbps", "megas", "velocidad"]),
    (
        "soporte",
        &[
            "soporte",
            "no anda",
            "no funciona",
            "no tengo internet",
            "sin internet",
            "lento",
            "se cayo",
            "no me anda",
            "no carga",
            "falla",
            "averia",
            "problema",
        ],
    ),
    (
        "humano",
        &[
            "humano",
            "persona",
            "asesor",
            "operador",
            "hablar con alguien",
            "agente",
            "supervisor",
        ],
    ),
    (
        "plan_change",
        &[
            "cambiar de plan",
            "subir plan",
            "bajar plan",
            "upgrade",
            "downgrade",
        ],
    ),
    (
        "account",
        &[
            "actualizar",
            "cambiar datos",
            "mi correo",
            "mi telefono",
            "mi direccion",
        ],
    ),
    ("cancel", &["cancelar", "dar de baja", "retirar"]),
];

/// Devuelve los bodies normalizados (lowercase + sin tildes + trim) de los
/// mensajes inbound del cliente. Ignora mensajes sin body o vacíos.
///
/// El nombre habla de "zones" pero en realidad son **bodies completos**: la
/// extracción de zonas reales pasa en `validate_zone_mentioned`, que hace
/// matching por tokens significativos sobre este buffer. Conservamos el
/// nombre por compatibilidad con `ToolContext.customer_explicit_zones`.
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

/// Stopwords que se descartan al tokenizar la zona reclamada por la IA. Si
/// no las filtráramos, palabras como "municipio" o "estado" siempre estarían
/// presentes en el buffer del cliente y matchearían cualquier alucinación.
const ZONE_STOPWORDS: &[&str] = &[
    "municipio",
    "parroquia",
    "sector",
    "urbanizacion",
    "urb",
    "estado",
    "ciudad",
    "pueblo",
    "calle",
    "avenida",
    "zona",
    "area",
    "region",
];

/// Largo mínimo de un token para considerarse significativo. 4 descarta
/// conectores comunes ("en", "por", "del", "los", etc.) sin esfuerzo.
const MIN_SIGNIFICANT_TOKEN_LEN: usize = 4;

/// Tokeniza un string ya normalizado por palabras alfanuméricas, descartando
/// tokens cortos y stopwords geográficos.
fn significant_tokens(normalized: &str) -> Vec<String> {
    normalized
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .filter(|t| t.chars().count() >= MIN_SIGNIFICANT_TOKEN_LEN)
        .filter(|t| !ZONE_STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// true si la zona reclamada por la IA fue mencionada por el cliente. Hace
/// matching por tokens significativos: tokeniza la zona reclamada (descartando
/// stopwords como "municipio", "estado") y verifica que **al menos un token**
/// aparezca en el buffer normalizado de mensajes del cliente.
///
/// Diseñado para tolerar el caso real:
///   cliente: "estoy ubicado en pedernales municipio carlos arvelo"
///   AI claim: "Loro Pedernales, municipio Carlos Arvelo"
///   → tokens claim filtrados: [loro, pedernales, carlos, arvelo]
///   → buffer cliente contiene "pedernales" → ✅ pasa
///
/// Si el claim no tiene ningún token significativo (todo stopwords o texto
/// muy corto), retorna `false` por seguridad — la IA no debería estar
/// llamando `check_coverage` con argumentos así.
pub fn validate_zone_mentioned(claimed_zone: &str, customer_zones: &[String]) -> bool {
    let n_claimed = normalize_zone(claimed_zone);
    if n_claimed.is_empty() {
        return false;
    }
    let claim_tokens = significant_tokens(&n_claimed);
    if claim_tokens.is_empty() {
        return false;
    }
    let buffer: String = customer_zones
        .iter()
        .map(|s| normalize_zone(s))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if buffer.is_empty() {
        return false;
    }
    claim_tokens.iter().any(|t| buffer.contains(t.as_str()))
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

/// Igual que `extract_customer_explicit_intents` pero acepta `&[&WaMessage]`
/// para evitar clonar la ráfaga en el caller.
pub fn extract_customer_explicit_intents_refs(messages: &[&WaMessage]) -> Vec<String> {
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
/// es turn_number 1 y no hay zones, intents ni media — evitar inyectar HUD vacío.
pub fn build_turn_state(
    history: &[ConvTurn],
    customer_zones: &[String],
    customer_intents: &[String],
    customer_media_ids: &[String],
) -> Option<String> {
    let turn_number = history.iter().filter(|t| t.role == ConvRole::User).count() + 1;
    if turn_number == 1
        && customer_zones.is_empty()
        && customer_intents.is_empty()
        && customer_media_ids.is_empty()
    {
        return None;
    }
    let mut lines = vec![format!("turn_number: {}", turn_number)];
    if !customer_zones.is_empty() {
        lines.push(format!(
            "customer_explicit_zones: {}",
            customer_zones.join(", ")
        ));
    }
    if !customer_intents.is_empty() {
        lines.push(format!(
            "customer_explicit_intents: {}",
            customer_intents.join(", ")
        ));
    }
    // Lista cerrada de media_ids legítimos (de los mensajes inbound del cliente).
    // El validador de `report_payment` rechaza cualquier ID fuera de esta lista,
    // así que el LLM debe copiar uno de acá literal — no inventar `...` ni
    // `image_0` ni concatenaciones.
    if !customer_media_ids.is_empty() {
        lines.push(format!(
            "available_media_ids: {}",
            customer_media_ids.join(", ")
        ));
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn customer_buffer(messages: &[&str]) -> Vec<String> {
        messages.iter().map(|s| normalize_zone(s)).collect()
    }

    #[test]
    fn passes_when_claim_token_appears_in_long_customer_phrase() {
        // Caso real prod: cliente dice frase larga, AI reclama zona compuesta.
        let zones = customer_buffer(&[
            "hola buenas",
            "estoy ubicado en pedernales municipio carlos arvelo",
            "creo que tambien se llama Loro Pedernales",
        ]);
        assert!(validate_zone_mentioned(
            "Loro Pedernales, municipio Carlos Arvelo",
            &zones
        ));
    }

    #[test]
    fn passes_when_customer_says_zone_alone_and_ai_claims_same() {
        let zones = customer_buffer(&["vivo en Naguanagua"]);
        assert!(validate_zone_mentioned("Naguanagua", &zones));
    }

    #[test]
    fn passes_when_ai_claims_subset_of_customer_message() {
        let zones = customer_buffer(&["estoy en Valencia, Estado Carabobo"]);
        assert!(validate_zone_mentioned("Valencia", &zones));
        assert!(validate_zone_mentioned("Carabobo", &zones));
    }

    #[test]
    fn blocks_when_ai_hallucinates_zone() {
        let zones = customer_buffer(&["hola, quiero info del internet", "soy nuevo cliente"]);
        assert!(!validate_zone_mentioned("Naguanagua", &zones));
        assert!(!validate_zone_mentioned(
            "Valencia, Estado Carabobo",
            &zones
        ));
    }

    #[test]
    fn blocks_when_only_stopwords_match() {
        // Si el AI manda "Municipio Carabobo" y el cliente solo dijo
        // "estoy en mi municipio", municipio matchea pero es stopword
        // → debe bloquear (carabobo no aparece en buffer).
        let zones = customer_buffer(&["estoy en mi municipio"]);
        assert!(!validate_zone_mentioned("Municipio Carabobo", &zones));
    }

    #[test]
    fn blocks_empty_claim() {
        let zones = customer_buffer(&["vivo en Caracas"]);
        assert!(!validate_zone_mentioned("", &zones));
        assert!(!validate_zone_mentioned("   ", &zones));
    }

    #[test]
    fn blocks_when_customer_buffer_empty() {
        assert!(!validate_zone_mentioned("Caracas", &[]));
        assert!(!validate_zone_mentioned("Caracas", &["".to_string()]));
    }

    #[test]
    fn blocks_when_claim_has_no_significant_tokens() {
        let zones = customer_buffer(&["vivo en algun lugar"]);
        // Solo tiene tokens cortos / stopwords.
        assert!(!validate_zone_mentioned("la el", &zones));
        assert!(!validate_zone_mentioned("zona", &zones));
    }

    #[test]
    fn matches_case_and_accent_insensitive() {
        let zones = customer_buffer(&["estoy en Maracaibo"]);
        assert!(validate_zone_mentioned("MARACAIBO", &zones));
        assert!(validate_zone_mentioned("Maracaíbo", &zones));
    }

    #[test]
    fn significant_tokens_filters_correctly() {
        let n = normalize_zone("Loro Pedernales, municipio Carlos Arvelo");
        let tokens = significant_tokens(&n);
        assert!(tokens.contains(&"loro".to_string()));
        assert!(tokens.contains(&"pedernales".to_string()));
        assert!(tokens.contains(&"carlos".to_string()));
        assert!(tokens.contains(&"arvelo".to_string()));
        assert!(!tokens.contains(&"municipio".to_string()));
    }
}
