// Migration: AI Agent Sales Phase 2
// 2026-05-06
//
// Backfill campos nuevos en colecciones existentes para que el seed lazy
// y los handlers PATCH no fallen al leer docs legacy sin estos campos.
//
// Correr con:
//   mongosh <MONGO_URI> --file scripts/migrations/2026-05-06-ai-sales-phase2.js

// 1. Backfill connection_types = ["fibra"] en zonas que no lo tengan.
//    El SUPERADMIN puede actualizarlo desde el front cuando corresponda.
var zonesResult = db.AiCoverageZones.updateMany(
    { connection_types: { $exists: false } },
    { $set: { connection_types: ["fibra"] } }
);
print("AiCoverageZones backfilled:", zonesResult.modifiedCount, "docs");

// 2. Backfill price_usd = 0 en planes existentes.
//    El SUPERADMIN debe actualizar los precios reales desde la UI.
var plansResult = db.AiPlans.updateMany(
    { price_usd: { $exists: false } },
    { $set: { price_usd: 0 } }
);
print("AiPlans backfilled:", plansResult.modifiedCount, "docs");

print("Migration 2026-05-06-ai-sales-phase2 completada.");
