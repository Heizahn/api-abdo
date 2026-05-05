//! División político-territorial de Venezuela.
//!
//! Datos compilados en tiempo de compilación como const array. El `LazyLock`
//! ordena estados y municipios con collation es-VE (Ñ entre N y O) la primera
//! vez que se accede. Usado por:
//! - El endpoint `GET /coverage-zones/political-divisions`.
//! - La capa de validación (`validate_state`, `validate_municipality`).

use std::collections::HashMap;
use std::sync::LazyLock;

pub struct StateEntry {
    pub state: &'static str,
    pub municipalities: &'static [&'static str],
}

/// Datos crudos. El orden aquí no importa; `DIVISIONS` los ordena al iniciar.
const RAW_DIVISIONS: &[(&str, &[&str])] = &[
    ("Amazonas", &["Alto Orinoco", "Atabapo", "Atures", "Autana", "Manapiare", "Maroa", "Río Negro"]),
    ("Anzoátegui", &["Anaco", "Aragua", "Bolívar", "Bruzual", "Cajigal", "Carvajal", "Diego Bautista Urbaneja", "Ezequiel Zamora", "Fernando de Peñalver", "Francisco del Carmen Cova", "Guanta", "Independencia", "Juan Antonio Sotillo", "Juan Manuel Cajigal", "Liberal", "McGregor", "Miranda", "Monagas", "Peñalver", "Pedro María Freites", "Píritu", "San Juan de Capistrano", "Santa Ana", "Simón Bolívar", "Simón Rodríguez", "Sir Robert Daun", "Sotillo"]),
    ("Apure", &["Achaguas", "Biruaca", "Muñoz", "Páez", "Pedro Camejo", "Rómulo Gallegos", "San Fernando"]),
    ("Aragua", &["Bolívar", "Camatagua", "Costa de Oro", "Francisco Linares Alcántara", "Girardot", "Independencia", "Libertador", "Mario Briceño Iragorry", "Ocumare de la Costa de Oro", "Ribas", "San Casimiro", "San Sebastián", "Santiago Mariño", "Santos Michelena", "Sucre", "Tovar", "Urdaneta", "Zamora"]),
    ("Barinas", &["Alberto Arvelo Torrealba", "Antonio José de Sucre", "Arismendi", "Barinas", "Bolívar", "Cruz Paredes", "Ezequiel Zamora", "Obispos", "Pedraza", "Rojas", "Sosa", "Ticoporo"]),
    ("Bolívar", &["Angostura del Orinoco", "Caroní", "Cedeño", "El Callao", "Gran Sabana", "Heres", "Padre Pedro Chien", "Piar", "Raúl Leoni", "Roscio", "Sifontes", "Sucre"]),
    ("Carabobo", &["Bejuma", "Carlos Arvelo", "Diego Ibarra", "Guacara", "Juan José Mora", "Libertador", "Los Guayos", "Miranda", "Montalbán", "Naguanagua", "Puerto Cabello", "San Diego", "San Joaquín", "Valencia"]),
    ("Cojedes", &["Anzoátegui", "Falcón", "Girardot", "Lima Blanco", "Pao de San Juan Bautista", "Ricaurte", "Rómulo Gallegos", "San Carlos", "Tinaco"]),
    ("Delta Amacuro", &["Antonio Díaz", "Casacoima", "Pedernales", "Tucupita"]),
    ("Distrito Capital", &["Libertador"]),
    ("Falcón", &["Acosta", "Bolívar", "Buchivacoa", "Cacique Manaure", "Carirubana", "Colina", "Dabajuro", "Democracia", "Falcón", "Federación", "Jacura", "Los Taques", "Mauroa", "Miranda", "Monseñor Iturriza", "Palmasola", "Petit", "Piritu", "San Francisco", "Silva", "Sucre", "Tocópero", "Unión", "Urumaco", "Zamora"]),
    ("Guárico", &["Camatagua", "Chaguaramas", "El Socorro", "Francisco de Miranda", "Julián Mellado", "Las Mercedes", "Leonardo Infante", "Monagas", "Ortiz", "Ribas", "San Gerónimo de Guayabal", "San José de Guaribe", "Santa María de Ipire", "Zaraza"]),
    ("La Guaira", &["Vargas"]),
    ("Lara", &["Andrés Eloy Blanco", "Crespo", "Iribarren", "Jiménez", "Morán", "Palavecino", "Simón Planas", "Torres", "Urdaneta"]),
    ("Mérida", &["Alberto Adriani", "Andrés Bello", "Antonio Pinto Salinas", "Aricagua", "Arzobispo Chacón", "Caracciolo Parra Olmedo", "Cardenal Quintero", "Guaraque", "Julio César Salas", "Justo Briceño", "Libertador", "Miranda", "Obispo Ramos de Lora", "Padre Noguera", "Pueblo Llano", "Rangel", "Rivas Dávila", "Santos Marquina", "Sucre", "Tovar", "Tulio Febres Cordero", "Zea"]),
    ("Miranda", &["Acevedo", "Andrés Bello", "Baruta", "Brión", "Buroz", "Carrizal", "Chacao", "Cristóbal Rojas", "El Hatillo", "Guaicaipuro", "Independencia", "Lander", "Libertador", "Los Salias", "Páez", "Paz Castillo", "Pedro Gual", "Plaza", "Simón Bolívar", "Sucre", "Urdaneta", "Zamora"]),
    ("Monagas", &["Acosta", "Aguasay", "Bolívar", "Caripe", "Cedeño", "Ezequiel Zamora", "Libertador", "Maturín", "Piar", "Punceres", "Santa Bárbara", "Sotillo", "Uracoa"]),
    ("Nueva Esparta", &["Antolín del Campo", "Arismendi", "Díaz", "García", "Gómez", "Granadillo", "Macanao", "Maneiro", "Marcano", "Mariño", "Tubores", "Villalba"]),
    ("Portuguesa", &["Araure", "Esteller", "Guanare", "Guanarito", "José Vicente de Unda", "Ospino", "Páez", "Papelón", "San Genaro de Boconoíto", "San Rafael de Onoto", "Santa Rosalía", "Sucre", "Turen"]),
    ("Sucre", &["Andrés Eloy Blanco", "Arismendi", "Benítez", "Bermúdez", "Bolívar", "Cajigal", "Cruz Salmerón Acosta", "Libertador", "Mariño", "Mejías", "Montes", "Ribero", "Sucre", "Valdez"]),
    ("Táchira", &["Andrés Bello", "Ayacucho", "Bolívar", "Cárdenas", "Córdoba", "Fernández Feo", "Francisco de Miranda", "García de Hevia", "Guásimos", "Independencia", "Jáuregui", "José María Vargas", "Junín", "Libertad", "Libertador", "Lobatera", "Michelena", "Panamericano", "Pedro María Ureña", "Rafael Urdaneta", "Samuel Darío Maldonado", "San Cristóbal", "San Judas Tadeo", "Seboruco", "Simón Rodríguez", "Torbes", "Uribante"]),
    ("Trujillo", &["Andrés Bello", "Boconó", "Candelaria", "Carache", "Escuque", "José Felipe Márquez Cañizalez", "La Ceiba", "Miranda", "Monte Carmelo", "Motatan", "Pampán", "Pampanito", "Rafael Rangel", "San Rafael de Carvajal", "Sucre", "Trujillo", "Urdaneta", "Valera"]),
    ("Yaracuy", &["Arístides Bastidas", "Bolívar", "Bruzual", "Cocorote", "Independencia", "La Trinidad", "Monge", "Nirgua", "Páez", "Peña", "San Felipe", "Sucre", "Urachiche", "Veroes"]),
    ("Zulia", &["Almirante Padilla", "Baralt", "Catatumbo", "Colón", "Francisco Javier Pulgar", "Guajira", "Jesús Enrique Lossada", "Jesús María Semprún", "La Cañada de Urdaneta", "Lagunillas", "Machiques de Perijá", "Mara", "Maracaibo", "Miranda", "Páez", "Rosario de Perijá", "San Francisco", "Santa Rita", "Simón Bolívar", "Sucre", "Valmore Rodríguez"]),
];

/// Vec de estados ordenados alfabéticamente con collation es-VE (Ñ entre N y O).
/// Municipios dentro de cada estado también están ordenados.
pub static DIVISIONS: LazyLock<Vec<StateEntry>> = LazyLock::new(|| {
    let mut entries: Vec<StateEntry> = RAW_DIVISIONS
        .iter()
        .map(|(state, munis)| {
            let mut munis_sorted: Vec<&'static str> = munis.to_vec();
            munis_sorted.sort_by(|a, b| es_ve_cmp(a, b));
            let leaked: &'static [&'static str] = Box::leak(munis_sorted.into_boxed_slice());
            StateEntry { state, municipalities: leaked }
        })
        .collect();
    entries.sort_by(|a, b| es_ve_cmp(a.state, b.state));
    entries
});

/// Mapa estado → slice de municipios. O(1) lookup para validación.
pub static STATE_INDEX: LazyLock<HashMap<&'static str, &'static [&'static str]>> =
    LazyLock::new(|| {
        DIVISIONS
            .iter()
            .map(|e| (e.state, e.municipalities))
            .collect()
    });

/// Comparador de orden es-VE: normaliza a minúsculas + strip de diacríticos,
/// con Ñ/ñ mapeado a "n~" para que quede entre N y O.
pub fn es_ve_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    normalize_for_sort(a).cmp(&normalize_for_sort(b))
}

fn normalize_for_sort(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            match c.to_lowercase().next().unwrap_or(c) {
                'ñ' => vec!['n', '~'],
                lc => match lc {
                    'á' | 'à' | 'ä' | 'â' => vec!['a'],
                    'é' | 'è' | 'ë' | 'ê' => vec!['e'],
                    'í' | 'ì' | 'ï' | 'î' => vec!['i'],
                    'ó' | 'ò' | 'ö' | 'ô' => vec!['o'],
                    'ú' | 'ù' | 'ü' | 'û' => vec!['u'],
                    other => vec![other],
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_24_states_present() {
        assert_eq!(DIVISIONS.len(), 24, "Deben estar los 24 estados de Venezuela");
    }

    #[test]
    fn test_states_sorted_ascending() {
        for window in DIVISIONS.windows(2) {
            assert!(
                es_ve_cmp(window[0].state, window[1].state) != std::cmp::Ordering::Greater,
                "Estado '{}' debería ir antes que '{}'",
                window[0].state, window[1].state
            );
        }
    }

    #[test]
    fn test_municipalities_sorted_within_state() {
        for entry in DIVISIONS.iter() {
            for window in entry.municipalities.windows(2) {
                assert!(
                    es_ve_cmp(window[0], window[1]) != std::cmp::Ordering::Greater,
                    "En {}: '{}' debería ir antes que '{}'",
                    entry.state, window[0], window[1]
                );
            }
        }
    }

    #[test]
    fn test_carabobo_has_valencia() {
        let munis = STATE_INDEX.get("Carabobo").expect("Carabobo debe existir");
        assert!(munis.contains(&"Valencia"), "Carabobo debe tener Valencia");
        assert_eq!(munis.len(), 14, "Carabobo tiene 14 municipios");
    }

    #[test]
    fn test_state_index_count() {
        assert_eq!(STATE_INDEX.len(), 24, "STATE_INDEX debe tener 24 entradas");
    }

    #[test]
    fn test_n_tilde_sorts_between_n_and_o() {
        // Ñ/ñ debe ordenar después de N y antes de O
        let n_key = normalize_for_sort("nube");
        let enie_key = normalize_for_sort("ñoño");
        let o_key = normalize_for_sort("opaco");

        assert!(
            n_key < enie_key,
            "\"nube\" debe ordenar antes que \"ñoño\" (n < n~)"
        );
        assert!(
            enie_key < o_key,
            "\"ñoño\" debe ordenar antes que \"opaco\" (n~ < o)"
        );
    }

    #[test]
    fn test_distrito_capital_exists() {
        assert!(
            STATE_INDEX.contains_key("Distrito Capital"),
            "Distrito Capital debe estar en el índice"
        );
    }

    #[test]
    fn test_first_state_is_amazonas() {
        // Después de ordenar, Amazonas debe ser el primero
        assert_eq!(
            DIVISIONS[0].state, "Amazonas",
            "El primer estado debería ser Amazonas"
        );
    }
}
