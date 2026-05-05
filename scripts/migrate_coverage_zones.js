// scripts/migrate_coverage_zones.js
// Migra AiCoverageZones del esquema legacy {name, region, active} al
// esquema jerárquico {display_name, state, municipality, parish, sector,
// aliases, is_active, needs_review}.
//
// Uso: mongosh "<MONGO_URI>" scripts/migrate_coverage_zones.js
//
// El script es IDEMPOTENTE: detecta docs ya migrados por la presencia
// de los campos `display_name` o `needs_review` y los salta sin modificar.
// Re-ejecutar reporta migrated: 0.

print("Migrando AiCoverageZones (legacy → jerárquico)…");

// Lista canónica de estados — espejo de src/data/ve_political_divisions.rs.
// Actualizar acá si se agrega/renombra un estado en el Rust.
const CANONICAL_STATES = [
    "Amazonas", "Anzoátegui", "Apure", "Aragua", "Barinas", "Bolívar",
    "Carabobo", "Cojedes", "Delta Amacuro", "Distrito Capital", "Falcón",
    "Guárico", "La Guaira", "Lara", "Mérida", "Miranda", "Monagas",
    "Nueva Esparta", "Portuguesa", "Sucre", "Táchira", "Trujillo",
    "Yaracuy", "Zulia",
];

function stripDiacritics(s) {
    return (s || "")
        .normalize("NFD")
        .replace(/\p{M}/gu, "")
        .toLowerCase()
        .trim();
}

// Índice normalizado → nombre canónico para lookup O(1).
const STATE_INDEX = {};
CANONICAL_STATES.forEach(s => {
    STATE_INDEX[stripDiacritics(s)] = s;
});

// Intenta mapear una región del doc legacy al nombre canónico del estado.
// Soporta prefijos "Estado", "Edo.", "Edo " (con y sin punto).
function bestEffortState(region) {
    if (!region) return "";
    const key = stripDiacritics(region);
    if (STATE_INDEX[key]) return STATE_INDEX[key];
    // Intentar sin prefijos comunes
    const stripped = key.replace(/^(estado\s+|edo\.?\s*)/, "").trim();
    if (STATE_INDEX[stripped]) return STATE_INDEX[stripped];
    return "";
}

let migrated = 0;
let skipped = 0;
let errors = 0;

db.AiCoverageZones.find({}).forEach(doc => {
    // Idempotencia: si ya tiene display_name o needs_review → ya fue migrado.
    if (doc.display_name !== undefined || doc.needs_review !== undefined) {
        skipped++;
        return;
    }

    const newState = bestEffortState(doc.region || "");

    try {
        db.AiCoverageZones.updateOne(
            { _id: doc._id },
            {
                $set: {
                    display_name: doc.name || "",
                    state: newState,
                    municipality: "",
                    parish: null,
                    sector: null,
                    aliases: [],
                    is_active: false,          // forzar inactivo hasta revisión
                    needs_review: true,        // señal para el SUPERADMIN
                    updated_at: new Date(),
                },
                $unset: {
                    name: "",
                    region: "",
                    active: "",
                },
            }
        );
        migrated++;
    } catch (e) {
        print(`ERROR en doc ${doc._id}: ${e.message}`);
        errors++;
    }
});

print(`Done — migrated: ${migrated}, skipped (ya migrados): ${skipped}, errors: ${errors}`);

if (migrated > 0) {
    print("");
    print("SIGUIENTE PASO: revisá cada zona desde el front (UI de Coverage Zones).");
    print("Completá 'state' y 'municipality', luego activá con is_active=true.");
    print("La tool check_coverage NO matcheará zonas inactivas hasta que las actives.");
}
