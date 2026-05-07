//! Seed lazy de datos de negocio (`AiPlans`, `AiCoverageZones`).
//!
//! Se corre al arrancar el back. Si la colección está vacía, inserta el
//! catálogo inicial — los datos snapshot 2026-04 de `abdo77.com.ve/planes/`
//! y `/cobertura/`. Si después el SUPERADMIN borra todo desde el front y
//! reinicia, vuelven a insertarse: la IA sin datos comerciales es peor
//! UX que arrastrar el catálogo viejo. Si querés un opt-out, deshabilitá
//! desde el front (toggle `is_active`) en vez de borrar.

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
        benefits: &[
            "Internet ilimitado",
            "Router Wi-Fi incluido",
            "IPv6 público",
        ],
        display_order: 10,
    },
    SeedPlan {
        name: "Conexión Avanzada",
        mbps: 100,
        devices_recommendation: "6 a 8 dispositivos",
        benefits: &[
            "Internet ilimitado",
            "Router Wi-Fi incluido",
            "IPv6 público",
        ],
        display_order: 20,
    },
    SeedPlan {
        name: "Conexión Élite 120",
        mbps: 120,
        devices_recommendation: "Más de 10 dispositivos",
        benefits: &[
            "Internet ilimitado",
            "Router Wi-Fi incluido",
            "IPv6 público",
        ],
        display_order: 30,
    },
    SeedPlan {
        name: "Conexión Élite 250",
        mbps: 250,
        devices_recommendation: "Más de 10 dispositivos",
        benefits: &[
            "Internet ilimitado",
            "Router Wi-Fi incluido",
            "IPv6 público",
        ],
        display_order: 40,
    },
    SeedPlan {
        name: "Conexión Élite 500",
        mbps: 500,
        devices_recommendation: "Más de 10 dispositivos",
        benefits: &[
            "Internet ilimitado",
            "Router Wi-Fi incluido",
            "IPv6 público",
        ],
        display_order: 50,
    },
    SeedPlan {
        name: "Conexión Élite 1000",
        mbps: 1000,
        devices_recommendation: "Más de 10 dispositivos",
        benefits: &[
            "Internet ilimitado",
            "Router Wi-Fi incluido",
            "IPv6 público",
        ],
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
    SeedZone {
        display_name: "Carlos Arvelo",
        state: "Carabobo",
        municipality: "Carlos Arvelo",
    },
    SeedZone {
        display_name: "Guacara",
        state: "Carabobo",
        municipality: "Guacara",
    },
    SeedZone {
        display_name: "Los Guayos",
        state: "Carabobo",
        municipality: "Los Guayos",
    },
    SeedZone {
        display_name: "Valencia",
        state: "Carabobo",
        municipality: "Valencia",
    },
    SeedZone {
        display_name: "San Diego",
        state: "Carabobo",
        municipality: "San Diego",
    },
    SeedZone {
        display_name: "Libertador",
        state: "Carabobo",
        municipality: "Libertador",
    },
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
            tracing::warn!(
                "[ai_agent.seed] insert zone {} falló: {}",
                z.display_name,
                e
            );
        }
    }
    state.redis.invalidate_ai_coverage_cache_v2().await;
    tracing::info!("[ai_agent.seed] {} zonas insertadas", SEED_ZONES.len());
    Ok(())
}
