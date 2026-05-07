/// Extrae la referencia numérica canónica de un input de texto libre.
///
/// Algoritmo: escanea `input` en un solo paso y acumula corridas contiguas
/// de dígitos ASCII (`0–9`). Retorna la más larga. En empate de longitud
/// se queda con la **última** ocurrencia (en texto venezolano el número de
/// referencia suele aparecer al final, después del teléfono / monto / fecha).
///
/// Retorna `None` si no se encontró ningún dígito o si el input está vacío.
pub fn extract_canonical_reference(input: &str) -> Option<String> {
    let mut best_start: usize = 0;
    let mut best_len: usize = 0;
    let mut cur_start: usize = 0;
    let mut cur_len: usize = 0;
    let mut in_run = false;

    for (i, ch) in input.char_indices() {
        if ch.is_ascii_digit() {
            if in_run {
                cur_len += 1;
            } else {
                cur_start = i;
                cur_len = 1;
                in_run = true;
            }
        } else {
            if in_run {
                // end of run — check if it beats or ties the best
                if cur_len >= best_len {
                    best_start = cur_start;
                    best_len = cur_len;
                }
                in_run = false;
            }
        }
    }

    // Handle run that extends to end of string
    if in_run && cur_len >= best_len {
        best_start = cur_start;
        best_len = cur_len;
    }

    if best_len == 0 {
        None
    } else {
        Some(input[best_start..best_start + best_len].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_numeric_idempotent() {
        assert_eq!(
            extract_canonical_reference("12345678"),
            Some("12345678".to_string())
        );
    }

    #[test]
    fn mixed_text_extracts_suffix() {
        assert_eq!(
            extract_canonical_reference("ref 5678"),
            Some("5678".to_string())
        );
    }

    #[test]
    fn whitespace_ignored() {
        assert_eq!(
            extract_canonical_reference("  123  "),
            Some("123".to_string())
        );
    }

    #[test]
    fn no_digits_returns_none() {
        assert_eq!(extract_canonical_reference("ABCDEF"), None);
    }

    #[test]
    fn empty_returns_none() {
        assert_eq!(extract_canonical_reference(""), None);
    }

    /// Intencional false positive: teléfono (11 dígitos) gana sobre ref (4 dígitos).
    /// Mitigado por check_reference con scope de banco — documentado como trade-off.
    #[test]
    fn phone_beats_reference_longest_wins() {
        assert_eq!(
            extract_canonical_reference("Pago Móvil enviado a 04141234567 por Bs 100,00 ref 5678"),
            Some("04141234567".to_string())
        );
    }

    #[test]
    fn tie_keeps_last() {
        // "1234" y "5678" ambos tienen 4 dígitos — se queda con el último
        assert_eq!(
            extract_canonical_reference("ref 1234 op 5678"),
            Some("5678".to_string())
        );
    }

    #[test]
    fn longest_wins_among_many() {
        // "123" (3), "456789" (6), "12" (2) — gana el de 6
        assert_eq!(
            extract_canonical_reference("123 456789 12"),
            Some("456789".to_string())
        );
    }

    #[test]
    fn run_at_end_of_string_counted() {
        // Asegura que la corrida al final del string se considera correctamente
        assert_eq!(
            extract_canonical_reference("abc 99999"),
            Some("99999".to_string())
        );
    }

    #[test]
    fn leading_zeros_preserved_12_digit() {
        // BDV reference with leading zeros — must be preserved verbatim (no parseInt stripping)
        assert_eq!(
            extract_canonical_reference("005336391541"),
            Some("005336391541".to_string())
        );
    }

    #[test]
    fn leading_zero_short_ref() {
        // 4-digit short reference with a leading zero
        assert_eq!(
            extract_canonical_reference("0114"),
            Some("0114".to_string())
        );
    }
}
