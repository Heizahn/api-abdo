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

// 3. Normalizar AiAgents.model.provider a "openrouter" en docs legacy
//    (residuo de la era multi-provider con Gemini). Hoy el campo es
//    informativo y nunca se lee en runtime — esta limpieza es cosmética
//    para evitar que la UI muestre "gemini" en agentes viejos.
var providerResult = db.AiAgents.updateMany(
    { "model.provider": { $ne: "openrouter" } },
    { $set: { "model.provider": "openrouter" } }
);
print("AiAgents.model.provider normalized:", providerResult.modifiedCount, "docs");

// 4. Limpiar AiAgents.model.endpoint_override y AiAgents.model.api_key_encrypted
//    en docs legacy. Ambos campos son ignorados en runtime tras la migración
//    a OpenRouter + AiConfig global, pero quedan como bytes muertos en DB.
var endpointResult = db.AiAgents.updateMany(
    { "model.endpoint_override": { $exists: true } },
    { $unset: { "model.endpoint_override": "" } }
);
print("AiAgents.model.endpoint_override unset:", endpointResult.modifiedCount, "docs");

var apiKeyResult = db.AiAgents.updateMany(
    { "model.api_key_encrypted": { $exists: true, $ne: "" } },
    { $set: { "model.api_key_encrypted": "" } }
);
print("AiAgents.model.api_key_encrypted cleared:", apiKeyResult.modifiedCount, "docs");

print("Migration 2026-05-06-ai-sales-phase2 completada.");
