//! Seed lazy de datos de negocio (`AiPlans`, `AiCoverageZones`).
//!
//! Se corre al arrancar el back. Si la colección está vacía, inserta el
//! catálogo inicial — los datos snapshot 2026-04 de `abdo77.com.ve/planes/`
//! y `/cobertura/`. Si después el SUPERADMIN borra todo desde el front y
//! reinicia, vuelven a insertarse: la IA sin datos comerciales es peor
//! UX que arrastrar el catálogo viejo. Si querés un opt-out, deshabilitá
//! desde el front (toggle `is_active`) en vez de borrar.
//!
//! ## System prompt para Carla (agente `purpose: ventas`)
//!
//! El agente Carla vive en MongoDB (creado vía API). Para actualizar su
//! system_prompt, usar el endpoint `PATCH /v1/auth-user/whatsapp/ai-agent/agents/:id`
//! con el cuerpo `{ "system_prompt": CARLA_SYSTEM_PROMPT }`.
//!
//! Ver también: `scripts/migrations/2026-05-06-carla-system-prompt.js`.

/// System prompt de referencia para el agente de Ventas (Carla).
/// Soporta el flujo completo de cotización con precios, instalaciones y promociones.
///
/// Este string es una referencia canónica — el valor live está en `AiAgents.system_prompt`.
/// Para actualizar Carla en producción: `PATCH /ai-agent/agents/:id` con este valor.
pub const CARLA_SYSTEM_PROMPT_V2: &str = r#"Sos Carla, asistente virtual de ventas de ABDO77. Tu misión es ayudar a clientes potenciales a elegir un plan de internet y guiarlos hasta cerrar la contratación con un asesor humano.

## FLUJO DE COTIZACIÓN COMPLETO

### Paso 1 — Cobertura
Siempre verificar cobertura PRIMERO. Si el cliente no mencionó zona: preguntale "¿De qué zona o municipio nos escribís?"
Usá `check_coverage` solo cuando el cliente DIJO explícitamente la zona.

La respuesta de `check_coverage` incluye `available_types`:
- Si tiene 1 tipo (ej: ["fibra"]): usá ese tipo directamente, no preguntes.
- Si tiene 2 tipos (["fibra", "antena"]): preguntale "En tu zona tenemos fibra y antena. ¿Cuál preferís?"

### Paso 2 — Planes
Llamá `list_plans` para obtener el catálogo con precios en USD.
Presentá las opciones de forma clara con velocidad, dispositivos y precio.

### Paso 3 — Cotización en Bs
Cuando el cliente elija un plan o pregunte el precio en Bs:
1. Llamá `calculate_amount_bs` con el `price_usd` del plan.
2. Presentá: precio USD + tasa BCV + IVA + total en Bs.

### Paso 4 — Instalación
Cuando el cliente pregunte por el costo de instalación:
1. Llamá `get_installation_info` con el tipo de conexión confirmado.
2. Presentá: costo base USD + qué incluye.
3. Sobre metro extra: "Incluye [X]mt de cable. Si necesitás más, el metro extra cuesta $[Y] (≈Bs Z). El asesor confirma los metros exactos al visitar."
4. NO intentes calcular metros — ese dato solo lo puede medir el asesor en sitio.

### Paso 5 — Promociones
Llamá `get_active_promotions` después de `list_plans` o `get_installation_info`.
Si hay promos vigentes, mencionálas: "Además tenemos una promo activa: [descripción]."
Si no hay promos: no comentes nada.

### Paso 6 — Cierre
Cuando el cliente quiera contratar o pida coordinar la instalación, llamá `request_human` para pasarlo a un asesor.
Razón: "Cliente listo para contratar — zona [X], plan [Y], tipo [Z]."

## EJEMPLOS

E1 — Sin zona mencionada:
Cliente: "Quiero contratar internet"
Carla: "¡Genial! ¿De qué zona o municipio nos escribís para verificar cobertura?"

E2 — Cotización básica:
Cliente: "¿Cuánto sale el plan de 100 Mbps en Bs?"
→ `list_plans` → tomar price_usd del plan → `calculate_amount_bs` → responder con desglose.

E3 — Zona con un solo tipo:
`check_coverage` devuelve `available_types: ["fibra"]`
→ Usar fibra directamente. No preguntar.

E4 — Zona con dos tipos:
`check_coverage` devuelve `available_types: ["fibra", "antena"]`
→ "En tu zona tenemos fibra y antena. ¿Cuál preferís?"

E5 — Instalación:
Cliente: "¿Cuánto cuesta la instalación?"
→ `get_installation_info(connection_type: "[tipo confirmado]")` → presentar desglose.

E6 — Con promo activa:
Después de cotizar: `get_active_promotions` devuelve promo.
→ "Además, tenemos una promo activa: [nombre]. [descripción]. [condiciones]. ¡Aplicaría a tu contratación!"

E7 — Cotización completa (con todo):
1. Zona → cobertura → tipo (si único, no preguntar)
2. `list_plans` → cliente elige
3. `calculate_amount_bs` → presentar precio en Bs
4. `get_installation_info` → presentar costo instalación
5. `get_active_promotions` → mencionar si hay
6. Ofrecé cerrar con asesor → `request_human`

E8 — Cierre con request_human:
Cliente: "Sí, quiero contratar"
→ `request_human(reason: "Cliente listo para contratar — zona Valencia, plan Conexión Avanzada 100Mbps, fibra")`

## REGLAS ESTRICTAS

- NUNCA inventés precios, tasas ni disponibilidad. Siempre usá las tools.
- NUNCA calculés metros de cable extra. Eso lo confirma el asesor en sitio.
- NUNCA derivés a humano sin tener al menos: zona verificada + plan elegido.
- Si el cliente tiene problemas técnicos o de facturación: derivá a otro agente, no es tu scope.
- Siempre respondé en español venezolano, cálido y directo."#;

use std::sync::Arc;

use mongodb::bson::DateTime as BsonDateTime;

use crate::{
    db::AiAgentRepository,
    models::ai_agent::{AiCoverageZone, AiPlan},
    state::AppState,
};

struct SeedPlan {
    name: &'static str,
    mbps: u32,
    devices_recommendation: &'static str,
    benefits: &'static [&'static str],
    display_order: i32,
}

const SEED_PLANS: &[SeedPlan] = &[
    SeedPlan {
        name: "Conexión Esencial",
        mbps: 80,
        devices_recommendation: "1 a 3 dispositivos",
        benefits: &["Internet ilimitado", "Router Wi-Fi incluido", "IPv6 público"],
        display_order: 10,
    },
    SeedPlan {
        name: "Conexión Avanzada",
        mbps: 100,
        devices_recommendation: "6 a 8 dispositivos",
        benefits: &["Internet ilimitado", "Router Wi-Fi incluido", "IPv6 público"],
        display_order: 20,
    },
    SeedPlan {
        name: "Conexión Élite 120",
        mbps: 120,
        devices_recommendation: "Más de 10 dispositivos",
        benefits: &["Internet ilimitado", "Router Wi-Fi incluido", "IPv6 público"],
        display_order: 30,
    },
    SeedPlan {
        name: "Conexión Élite 250",
        mbps: 250,
        devices_recommendation: "Más de 10 dispositivos",
        benefits: &["Internet ilimitado", "Router Wi-Fi incluido", "IPv6 público"],
        display_order: 40,
    },
    SeedPlan {
        name: "Conexión Élite 500",
        mbps: 500,
        devices_recommendation: "Más de 10 dispositivos",
        benefits: &["Internet ilimitado", "Router Wi-Fi incluido", "IPv6 público"],
        display_order: 50,
    },
    SeedPlan {
        name: "Conexión Élite 1000",
        mbps: 1000,
        devices_recommendation: "Más de 10 dispositivos",
        benefits: &["Internet ilimitado", "Router Wi-Fi incluido", "IPv6 público"],
        display_order: 60,
    },
];

/// Estructura de zona para el seed inicial (esquema jerárquico nuevo).
struct SeedZone {
    display_name: &'static str,
    state: &'static str,
    municipality: &'static str,
}

/// 6 zonas de Carabobo — seed inicial. Todas activas, sin revisión pendiente.
const SEED_ZONES: &[SeedZone] = &[
    SeedZone { display_name: "Carlos Arvelo", state: "Carabobo", municipality: "Carlos Arvelo" },
    SeedZone { display_name: "Guacara",       state: "Carabobo", municipality: "Guacara"       },
    SeedZone { display_name: "Los Guayos",    state: "Carabobo", municipality: "Los Guayos"    },
    SeedZone { display_name: "Valencia",      state: "Carabobo", municipality: "Valencia"      },
    SeedZone { display_name: "San Diego",     state: "Carabobo", municipality: "San Diego"     },
    SeedZone { display_name: "Libertador",    state: "Carabobo", municipality: "Libertador"    },
];

pub async fn run(state: Arc<AppState>) {
    if let Err(e) = seed_plans(&state).await {
        tracing::warn!("[ai_agent.seed] plans falló: {}", e);
    }
    if let Err(e) = seed_zones(&state).await {
        tracing::warn!("[ai_agent.seed] coverage zones falló: {}", e);
    }
}

async fn seed_plans(state: &Arc<AppState>) -> Result<(), String> {
    if !state.db.ai_plans_is_empty().await? {
        return Ok(());
    }
    let now = BsonDateTime::now();
    for p in SEED_PLANS {
        let plan = AiPlan {
            id: None,
            name: p.name.to_string(),
            mbps: p.mbps,
            devices_recommendation: p.devices_recommendation.to_string(),
            benefits: p.benefits.iter().map(|b| b.to_string()).collect(),
            active: true,
            display_order: p.display_order,
            price_usd: 0.0,
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = state.db.create_ai_plan(plan).await {
            tracing::warn!("[ai_agent.seed] insert plan {} falló: {}", p.name, e);
        }
    }
    state.redis.invalidate_ai_plans_cache().await;
    tracing::info!("[ai_agent.seed] {} planes insertados", SEED_PLANS.len());
    Ok(())
}

async fn seed_zones(state: &Arc<AppState>) -> Result<(), String> {
    if !state.db.ai_coverage_zones_is_empty().await? {
        return Ok(());
    }
    let now = BsonDateTime::now();
    for z in SEED_ZONES {
        let zone = AiCoverageZone {
            id: None,
            display_name: z.display_name.to_string(),
            state: z.state.to_string(),
            municipality: z.municipality.to_string(),
            parish: None,
            sector: None,
            aliases: vec![],
            connection_types: vec![crate::models::ai_agent::ConnectionType::Fibra],
            is_active: true,
            needs_review: false,
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = state.db.create_ai_coverage_zone(zone).await {
            tracing::warn!("[ai_agent.seed] insert zone {} falló: {}", z.display_name, e);
        }
    }
    state.redis.invalidate_ai_coverage_cache_v2().await;
    tracing::info!("[ai_agent.seed] {} zonas insertadas", SEED_ZONES.len());
    Ok(())
}
