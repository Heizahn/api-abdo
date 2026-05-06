//! Datos de negocio editables desde el front:
//! - Planes (`AiPlans`) que la tool `list_plans` devuelve a la IA.
//! - Zonas de cobertura (`AiCoverageZones`) que la tool `check_coverage` matchea.
//!
//! También expone:
//! - Discovery (`GET /tools`) con metadata de todas las tools soportadas.
//! - División política canónica (`GET /ai/zones/political-divisions`).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use mongodb::bson::{oid::ObjectId, DateTime as BsonDateTime};
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::{
    data::ve_political_divisions::STATE_INDEX,
    db::{AiAgentRepository, AiInstallationRepository, AiPromotionRepository},
    error::ApiError,
    models::{
        ai_agent::{
            AiBusinessDataDeleteResponse, AiCoverageZone, AiCoverageZoneItem,
            AiCoverageZoneResponse, AiCoverageZonesListResponse,
            AiInstallationConfig, AiInstallationConfigItem, AiInstallationConfigResponse,
            AiInstallationConfigsListResponse, UpdateAiInstallationConfigRequest,
            AiPlan, AiPlanItem, AiPlanResponse, AiPlansListResponse,
            AiPromotion, AiPromotionItem, AiPromotionResponse, AiPromotionsListResponse,
            CreateAiCoverageZoneRequest, CreateAiPlanRequest,
            CreateAiPromotionRequest, UpdateAiPromotionRequest,
            ConnectionType, PoliticalDivisionItem, PoliticalDivisionsResponse,
            UpdateAiCoverageZoneRequest, UpdateAiPlanRequest,
        },
        users::User,
    },
    state::AppState,
};

use super::tools::normalize_zone;

const SUPERADMIN_ROLE: f32 = 0.0;

const PLAN_NAME_MAX: usize = 100;
const PLAN_DEVICES_MAX: usize = 200;
const PLAN_BENEFIT_MAX: usize = 200;
const PLAN_BENEFITS_MAX_COUNT: usize = 12;

const ZONE_DISPLAY_NAME_MAX: usize = 100;
const ZONE_PARISH_MAX: usize = 100;
const ZONE_SECTOR_MAX: usize = 100;
const ZONE_ALIAS_MAX_LEN: usize = 100;
const ZONE_ALIASES_MAX_COUNT: usize = 5;

fn require_superadmin(u: &User) -> Result<(), ApiError> {
    if u.role != SUPERADMIN_ROLE {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

fn parse_oid(s: &str, field: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(s).map_err(|_| ApiError::ValidationError {
        code: "invalid_id".into(),
        field: field.into(),
        message: format!("'{}' no es un ObjectId válido", field),
    })
}

fn validate_required(v: &str, field: &str) -> Result<(), ApiError> {
    if v.trim().is_empty() {
        return Err(ApiError::ValidationError {
            code: "missing_field".into(),
            field: field.into(),
            message: format!("'{}' es requerido", field),
        });
    }
    Ok(())
}

fn validate_max_len(v: &str, field: &str, max: usize) -> Result<(), ApiError> {
    if v.chars().count() > max {
        return Err(ApiError::ValidationError {
            code: "field_too_long".into(),
            field: field.into(),
            message: format!("'{}' supera {} caracteres", field, max),
        });
    }
    Ok(())
}

fn iso(d: BsonDateTime) -> String {
    d.try_to_rfc3339_string().unwrap_or_default()
}

fn plan_not_found() -> ApiError {
    ApiError::domain_simple(StatusCode::NOT_FOUND, "ai_plan_not_found", "Plan no encontrado")
}

fn zone_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "ai_coverage_zone_not_found",
        "Zona de cobertura no encontrada",
    )
}

fn plan_to_item(p: AiPlan) -> AiPlanItem {
    AiPlanItem {
        id: p.id.map(|o| o.to_hex()).unwrap_or_default(),
        name: p.name,
        mbps: p.mbps,
        devices_recommendation: p.devices_recommendation,
        benefits: p.benefits,
        active: p.active,
        display_order: p.display_order,
        price_usd: p.price_usd,
        created_at: iso(p.created_at),
        updated_at: iso(p.updated_at),
    }
}

fn zone_to_item(z: AiCoverageZone) -> AiCoverageZoneItem {
    AiCoverageZoneItem {
        id: z.id.map(|o| o.to_hex()).unwrap_or_default(),
        display_name: z.display_name,
        state: z.state,
        municipality: z.municipality,
        parish: z.parish,
        sector: z.sector,
        aliases: z.aliases,
        connection_types: z.connection_types,
        is_active: z.is_active,
        needs_review: z.needs_review,
        created_at: iso(z.created_at),
        updated_at: iso(z.updated_at),
    }
}

fn validate_plan_input(
    name: &str,
    devices: &str,
    benefits: &[String],
) -> Result<(), ApiError> {
    validate_required(name, "name")?;
    validate_max_len(name, "name", PLAN_NAME_MAX)?;
    validate_required(devices, "devices_recommendation")?;
    validate_max_len(devices, "devices_recommendation", PLAN_DEVICES_MAX)?;
    if benefits.len() > PLAN_BENEFITS_MAX_COUNT {
        return Err(ApiError::ValidationError {
            code: "too_many_benefits".into(),
            field: "benefits".into(),
            message: format!("Máximo {} beneficios por plan", PLAN_BENEFITS_MAX_COUNT),
        });
    }
    for b in benefits {
        validate_required(b, "benefits[]")?;
        validate_max_len(b, "benefits[]", PLAN_BENEFIT_MAX)?;
    }
    Ok(())
}

// ─── Validaciones de zona ────────────────────────────────────────────────────

/// Valida que el estado exista en la lista canónica. Devuelve `&str` del input
/// trimmeado para evitar allocaciones en el path feliz.
fn validate_state(s: &str) -> Result<&str, ApiError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ApiError::ValidationError {
            code: "missing_field".into(),
            field: "state".into(),
            message: "'state' es requerido".into(),
        });
    }
    if STATE_INDEX.contains_key(trimmed) {
        Ok(trimmed)
    } else {
        Err(ApiError::ValidationError {
            code: "invalid_state".into(),
            field: "state".into(),
            message: format!("'{}' no es un estado válido de Venezuela", trimmed),
        })
    }
}

/// Valida que el municipio pertenezca al estado indicado (ya validado).
fn validate_municipality<'a>(state: &str, m: &'a str) -> Result<&'a str, ApiError> {
    let trimmed = m.trim();
    if trimmed.is_empty() {
        return Err(ApiError::ValidationError {
            code: "missing_field".into(),
            field: "municipality".into(),
            message: "'municipality' es requerido".into(),
        });
    }
    let munis = STATE_INDEX.get(state).ok_or_else(|| ApiError::ValidationError {
        code: "invalid_state".into(),
        field: "state".into(),
        message: format!("'{}' no es un estado válido de Venezuela", state),
    })?;
    if munis.iter().any(|x| *x == trimmed) {
        Ok(trimmed)
    } else {
        Err(ApiError::ValidationError {
            code: "invalid_municipality".into(),
            field: "municipality".into(),
            message: format!("'{}' no es un municipio del estado '{}'", trimmed, state),
        })
    }
}

/// Normaliza, deduplica y valida los aliases. Máx 5 items, cada uno ≤ 100 chars.
fn normalize_aliases(input: Vec<String>) -> Result<Vec<String>, ApiError> {
    if input.len() > ZONE_ALIASES_MAX_COUNT {
        return Err(ApiError::ValidationError {
            code: "too_many_aliases".into(),
            field: "aliases".into(),
            message: format!("Máximo {} aliases por zona", ZONE_ALIASES_MAX_COUNT),
        });
    }
    let mut out: Vec<String> = Vec::with_capacity(input.len());
    let mut seen = std::collections::HashSet::new();
    for raw in input {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().count() > ZONE_ALIAS_MAX_LEN {
            return Err(ApiError::ValidationError {
                code: "field_too_long".into(),
                field: "aliases".into(),
                message: format!("Cada alias debe tener máximo {} caracteres", ZONE_ALIAS_MAX_LEN),
            });
        }
        let key = normalize_zone(trimmed);
        if seen.insert(key) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

// ============================================
// CRUD Plans
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/plans",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Lista de planes (incluye inactivos)", body = AiPlansListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere SUPERADMIN"),
    )
)]
pub async fn list_plans_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
) -> Result<Json<AiPlansListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let plans = state
        .db
        .list_ai_plans(false)
        .await
        .map_err(ApiError::DatabaseError)?;
    Ok(Json(AiPlansListResponse {
        ok: true,
        data: plans.into_iter().map(plan_to_item).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/plans",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    request_body = CreateAiPlanRequest,
    responses(
        (status = 201, description = "Plan creado", body = AiPlanResponse),
        (status = 422, description = "Validación"),
    )
)]
pub async fn create_plan_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<CreateAiPlanRequest>,
) -> Result<(StatusCode, Json<AiPlanResponse>), ApiError> {
    require_superadmin(&current_user)?;
    let name = body.name.trim().to_string();
    let devices = body.devices_recommendation.trim().to_string();
    let benefits: Vec<String> = body
        .benefits
        .into_iter()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .collect();
    validate_plan_input(&name, &devices, &benefits)?;

    let now = BsonDateTime::now();
    let plan = AiPlan {
        id: None,
        name,
        mbps: body.mbps,
        devices_recommendation: devices,
        benefits,
        active: body.active.unwrap_or(true),
        display_order: body.display_order.unwrap_or(0),
        price_usd: body.price_usd,
        created_at: now,
        updated_at: now,
    };
    let saved = state.db.create_ai_plan(plan).await.map_err(ApiError::DatabaseError)?;
    state.redis.invalidate_ai_plans_cache().await;
    Ok((StatusCode::CREATED, Json(AiPlanResponse { ok: true, data: plan_to_item(saved) })))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/ai-agent/plans/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    request_body = UpdateAiPlanRequest,
    responses(
        (status = 200, description = "Plan actualizado", body = AiPlanResponse),
        (status = 404, description = "ai_plan_not_found"),
        (status = 422, description = "Validación"),
    )
)]
pub async fn update_plan_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAiPlanRequest>,
) -> Result<Json<AiPlanResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let mut plan = state
        .db
        .find_ai_plan_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(plan_not_found)?;

    if let Some(v) = body.name { plan.name = v.trim().to_string(); }
    if let Some(v) = body.mbps { plan.mbps = v; }
    if let Some(v) = body.devices_recommendation {
        plan.devices_recommendation = v.trim().to_string();
    }
    if let Some(v) = body.benefits {
        plan.benefits = v.into_iter()
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .collect();
    }
    if let Some(v) = body.active { plan.active = v; }
    if let Some(v) = body.display_order { plan.display_order = v; }
    if let Some(v) = body.price_usd { plan.price_usd = v; }

    validate_plan_input(&plan.name, &plan.devices_recommendation, &plan.benefits)?;
    plan.updated_at = BsonDateTime::now();

    let saved = state
        .db
        .replace_ai_plan(&oid, plan)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(plan_not_found)?;
    state.redis.invalidate_ai_plans_cache().await;
    Ok(Json(AiPlanResponse { ok: true, data: plan_to_item(saved) }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/ai-agent/plans/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    responses(
        (status = 200, description = "Plan eliminado", body = AiBusinessDataDeleteResponse),
        (status = 404, description = "ai_plan_not_found"),
    )
)]
pub async fn delete_plan_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<AiBusinessDataDeleteResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let ok = state.db.delete_ai_plan(&oid).await.map_err(ApiError::DatabaseError)?;
    if !ok {
        return Err(plan_not_found());
    }
    state.redis.invalidate_ai_plans_cache().await;
    Ok(Json(AiBusinessDataDeleteResponse { ok: true }))
}

// ============================================
// CRUD Coverage Zones
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/coverage-zones",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Lista de zonas (incluye inactivas)", body = AiCoverageZonesListResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere SUPERADMIN"),
    )
)]
pub async fn list_coverage_zones_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
) -> Result<Json<AiCoverageZonesListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let zones = state
        .db
        .list_ai_coverage_zones(false)
        .await
        .map_err(ApiError::DatabaseError)?;
    Ok(Json(AiCoverageZonesListResponse {
        ok: true,
        data: zones.into_iter().map(zone_to_item).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/coverage-zones",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    request_body = CreateAiCoverageZoneRequest,
    responses(
        (status = 201, description = "Zona creada", body = AiCoverageZoneResponse),
        (status = 422, description = "Validación — invalid_state, invalid_municipality, too_many_aliases, etc."),
    )
)]
pub async fn create_coverage_zone_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<CreateAiCoverageZoneRequest>,
) -> Result<(StatusCode, Json<AiCoverageZoneResponse>), ApiError> {
    require_superadmin(&current_user)?;

    let display_name = body.display_name.trim().to_string();
    validate_required(&display_name, "display_name")?;
    validate_max_len(&display_name, "display_name", ZONE_DISPLAY_NAME_MAX)?;

    let state_val = validate_state(&body.state)?.to_string();
    let municipality = validate_municipality(&state_val, &body.municipality)?.to_string();

    let parish = match body.parish.as_deref() {
        Some(v) if !v.trim().is_empty() => {
            let t = v.trim();
            validate_max_len(t, "parish", ZONE_PARISH_MAX)?;
            Some(t.to_string())
        }
        _ => None,
    };
    let sector = match body.sector.as_deref() {
        Some(v) if !v.trim().is_empty() => {
            let t = v.trim();
            validate_max_len(t, "sector", ZONE_SECTOR_MAX)?;
            Some(t.to_string())
        }
        _ => None,
    };

    let aliases = normalize_aliases(body.aliases)?;
    let is_active = body.is_active.unwrap_or(false);

    // Validate connection_types: required, min 1 element.
    if body.connection_types.is_empty() {
        return Err(ApiError::ValidationError {
            code: "missing_field".into(),
            field: "connection_types".into(),
            message: "'connection_types' es requerido (mínimo 1 elemento: 'fibra' o 'antena')".into(),
        });
    }

    // Activation gate on create: if admin wants is_active=true right away,
    // state + municipality are already validated above so it's fine.
    // needs_review is always false on fresh creates (design decision #4).

    let now = BsonDateTime::now();
    let zone = AiCoverageZone {
        id: None,
        display_name,
        state: state_val,
        municipality,
        parish,
        sector,
        aliases,
        connection_types: body.connection_types,
        is_active,
        needs_review: false,
        created_at: now,
        updated_at: now,
    };
    let saved = state.db.create_ai_coverage_zone(zone).await.map_err(ApiError::DatabaseError)?;
    state.redis.invalidate_ai_coverage_cache_v2().await;
    Ok((StatusCode::CREATED, Json(AiCoverageZoneResponse { ok: true, data: zone_to_item(saved) })))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/ai-agent/coverage-zones/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    request_body = UpdateAiCoverageZoneRequest,
    responses(
        (status = 200, description = "Zona actualizada", body = AiCoverageZoneResponse),
        (status = 404, description = "ai_coverage_zone_not_found"),
        (status = 422, description = "Validación — invalid_state, invalid_municipality, incomplete_zone, cannot_activate_unreviewed"),
    )
)]
pub async fn update_coverage_zone_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAiCoverageZoneRequest>,
) -> Result<Json<AiCoverageZoneResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let mut zone = state
        .db
        .find_ai_coverage_zone_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(zone_not_found)?;

    if let Some(v) = body.display_name {
        let t = v.trim().to_string();
        validate_required(&t, "display_name")?;
        validate_max_len(&t, "display_name", ZONE_DISPLAY_NAME_MAX)?;
        zone.display_name = t;
    }

    // State + municipality: validate only when provided.
    if let Some(ref s) = body.state {
        zone.state = validate_state(s)?.to_string();
    }
    if let Some(ref m) = body.municipality {
        // Use the (possibly just-updated) state to validate municipality.
        zone.municipality = validate_municipality(&zone.state, m)?.to_string();
    }

    // Double-Option PATCH semantics: Some(Some(v)) = set, Some(None) = clear, None = no-op
    if let Some(v) = body.parish {
        zone.parish = match v {
            Some(s) if !s.trim().is_empty() => {
                let t = s.trim();
                validate_max_len(t, "parish", ZONE_PARISH_MAX)?;
                Some(t.to_string())
            }
            _ => None,
        };
    }
    if let Some(v) = body.sector {
        zone.sector = match v {
            Some(s) if !s.trim().is_empty() => {
                let t = s.trim();
                validate_max_len(t, "sector", ZONE_SECTOR_MAX)?;
                Some(t.to_string())
            }
            _ => None,
        };
    }

    if let Some(aliases_input) = body.aliases {
        zone.aliases = normalize_aliases(aliases_input)?;
    }

    if let Some(ref types) = body.connection_types {
        if types.is_empty() {
            return Err(ApiError::ValidationError {
                code: "missing_field".into(),
                field: "connection_types".into(),
                message: "'connection_types' debe tener mínimo 1 elemento".into(),
            });
        }
        zone.connection_types = types.clone();
    }

    // Activation gate
    if let Some(activate) = body.is_active {
        if activate {
            // Check if state+municipality are complete (after applying updates above)
            if zone.state.is_empty() || zone.municipality.is_empty() {
                if zone.needs_review {
                    return Err(ApiError::ValidationError {
                        code: "cannot_activate_unreviewed".into(),
                        field: "is_active".into(),
                        message: "No podés activar una zona que necesita revisión: completá `state` y `municipality` antes de activarla".into(),
                    });
                } else {
                    return Err(ApiError::ValidationError {
                        code: "incomplete_zone".into(),
                        field: "is_active".into(),
                        message: "No podés activar una zona sin `state` y `municipality`".into(),
                    });
                }
            }
            // All good: activate and clear needs_review
            zone.is_active = true;
            zone.needs_review = false;
        } else {
            zone.is_active = false;
        }
    }

    zone.updated_at = BsonDateTime::now();

    let saved = state
        .db
        .replace_ai_coverage_zone(&oid, zone)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(zone_not_found)?;
    state.redis.invalidate_ai_coverage_cache_v2().await;
    Ok(Json(AiCoverageZoneResponse { ok: true, data: zone_to_item(saved) }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/ai-agent/coverage-zones/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    responses(
        (status = 200, description = "Zona eliminada", body = AiBusinessDataDeleteResponse),
        (status = 404, description = "ai_coverage_zone_not_found"),
    )
)]
pub async fn delete_coverage_zone_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<AiBusinessDataDeleteResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let ok = state.db.delete_ai_coverage_zone(&oid).await.map_err(ApiError::DatabaseError)?;
    if !ok {
        return Err(zone_not_found());
    }
    state.redis.invalidate_ai_coverage_cache_v2().await;
    Ok(Json(AiBusinessDataDeleteResponse { ok: true }))
}

// ============================================
// GET /ai/zones/political-divisions
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai/zones/political-divisions",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "División política canónica de Venezuela", body = PoliticalDivisionsResponse),
        (status = 401, description = "No autorizado"),
        (status = 403, description = "Requiere SUPERADMIN"),
    )
)]
pub async fn list_political_divisions_handler(
    Extension(current_user): Extension<User>,
) -> Result<Json<PoliticalDivisionsResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let data = crate::data::ve_political_divisions::DIVISIONS
        .iter()
        .map(|e| PoliticalDivisionItem {
            state: e.state.to_string(),
            municipalities: e.municipalities.iter().map(|s| s.to_string()).collect(),
        })
        .collect();
    Ok(Json(PoliticalDivisionsResponse { ok: true, data }))
}

// ============================================
// Discovery: GET /tools
// ============================================

#[derive(Debug, Serialize, ToSchema)]
pub struct AiToolMetaItem {
    /// Identificador estable. Se guarda en `AiAgent.tools[].name`.
    pub name: String,
    /// Etiqueta corta para el editor.
    pub display_name: String,
    /// Descripción default que va a Gemini si el agente no usa
    /// `description_override`. El front la muestra como helper text.
    pub description: String,
    /// Categoría visual para agrupar en la UI.
    pub category: String,
    /// Si la tool se incluye habilitada en agentes nuevos.
    pub default_enabled: bool,
    /// JSON Schema (subset) que describe el shape esperado de
    /// `AiAgent.tools[].config`. `null` cuando la tool no tiene config.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub config_schema: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiToolsListResponse {
    pub ok: bool,
    pub data: Vec<AiToolMetaItem>,
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/tools",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Tools soportadas por el back", body = AiToolsListResponse),
        (status = 403, description = "Requiere SUPERADMIN"),
    )
)]
pub async fn list_tools_handler(
    Extension(current_user): Extension<User>,
) -> Result<Json<AiToolsListResponse>, ApiError> {
    require_superadmin(&current_user)?;

    let data = vec![
        AiToolMetaItem {
            name: "lookup_customer".into(),
            display_name: "Buscar cliente".into(),
            description: "Busca clientes ISP por teléfono o cédula. La IA debe llamar antes de hablar de datos personales.".into(),
            category: "lookup".into(),
            default_enabled: true,
            config_schema: None,
        },
        AiToolMetaItem {
            name: "get_invoices".into(),
            display_name: "Consultar deudas / facturas".into(),
            description: "Devuelve las deudas activas o recientes del cliente identificado.".into(),
            category: "lookup".into(),
            default_enabled: true,
            config_schema: None,
        },
        AiToolMetaItem {
            name: "list_plans".into(),
            display_name: "Listar planes de internet".into(),
            description: "Catálogo de planes (sin precio). Para uso típico del agente de Ventas.".into(),
            category: "info".into(),
            default_enabled: false,
            config_schema: None,
        },
        AiToolMetaItem {
            name: "check_coverage".into(),
            display_name: "Verificar cobertura por zona".into(),
            description: "Indica si una zona/sector tiene cobertura activa.".into(),
            category: "info".into(),
            default_enabled: false,
            config_schema: None,
        },
        AiToolMetaItem {
            name: "request_human".into(),
            display_name: "Derivar a humano".into(),
            description: "Pausa la IA y libera la conversación para que un agente humano la tome.".into(),
            category: "escalation".into(),
            default_enabled: true,
            config_schema: None,
        },
        AiToolMetaItem {
            name: "create_ticket".into(),
            display_name: "Crear ticket de soporte".into(),
            description: "Crea un ticket categorizado y cierra la conversación, escalando a humano.".into(),
            category: "escalation".into(),
            default_enabled: true,
            config_schema: None,
        },
        AiToolMetaItem {
            name: "get_installation_info".into(),
            display_name: "Info de instalación".into(),
            description: "Retorna el costo base y detalles de instalación para un tipo de conexión (fibra o antena). Usar al cotizar instalación.".into(),
            category: "info".into(),
            default_enabled: false,
            config_schema: None,
        },
        AiToolMetaItem {
            name: "get_active_promotions".into(),
            display_name: "Promociones activas".into(),
            description: "Lista las promociones vigentes. Llamar al cotizar para informar al cliente de descuentos o beneficios actuales.".into(),
            category: "info".into(),
            default_enabled: false,
            config_schema: None,
        },
        AiToolMetaItem {
            name: "transfer_to_agent".into(),
            display_name: "Transferir a otro agente IA".into(),
            description: "Deriva la conversación a otro agente IA del whitelist (Soporte, Pagos, etc).".into(),
            category: "transfer".into(),
            default_enabled: false,
            config_schema: Some(serde_json::json!({
                "type": "object",
                "required": ["allowed_targets"],
                "properties": {
                    "allowed_targets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "ui_widget": "ai_agent_multiselect",
                        "description": "ObjectId hex de cada agente IA destino. El front filtra excluyendo el id del agente que se está editando."
                    }
                }
            })),
        },
    ];

    Ok(Json(AiToolsListResponse { ok: true, data }))
}

// ============================================
// CRUD Installations
// ============================================

fn installation_to_item(c: AiInstallationConfig) -> AiInstallationConfigItem {
    AiInstallationConfigItem {
        connection_type: c.connection_type,
        base_cost_usd: c.base_cost_usd,
        includes: c.includes,
        excedente_per_meter_usd: c.excedente_per_meter_usd,
        excedente_notes: c.excedente_notes,
        notes: c.notes,
        updated_at: iso(c.updated_at),
        editor_id: if c.editor_id.is_empty() { None } else { Some(c.editor_id) },
    }
}

/// Siembra un doc de instalación con valores 0 si no existe aún.
async fn ensure_installation(
    state: &Arc<AppState>,
    connection_type: ConnectionType,
    editor_id: &str,
) -> Result<AiInstallationConfig, ApiError> {
    if let Some(existing) = state
        .db
        .get_ai_installation(connection_type)
        .await
        .map_err(ApiError::DatabaseError)?
    {
        return Ok(existing);
    }
    let now = BsonDateTime::now();
    let default_config = AiInstallationConfig {
        id: None,
        connection_type,
        base_cost_usd: 0.0,
        includes: String::new(),
        excedente_per_meter_usd: None,
        excedente_notes: String::new(),
        notes: String::new(),
        updated_at: now,
        editor_id: editor_id.to_string(),
    };
    state
        .db
        .upsert_ai_installation(default_config)
        .await
        .map_err(ApiError::DatabaseError)
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/installations",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Configuraciones de instalación (fibra + antena)", body = AiInstallationConfigsListResponse),
        (status = 403, description = "Requiere SUPERADMIN"),
    )
)]
pub async fn list_installations_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
) -> Result<Json<AiInstallationConfigsListResponse>, ApiError> {
    require_superadmin(&current_user)?;

    let mut items = state
        .db
        .list_ai_installations()
        .await
        .map_err(ApiError::DatabaseError)?;

    // Si la colección está vacía o le faltan tipos, sembramos los defaults lazy.
    let has_fibra = items.iter().any(|c| matches!(c.connection_type, ConnectionType::Fibra));
    let has_antena = items.iter().any(|c| matches!(c.connection_type, ConnectionType::Antena));

    if !has_fibra {
        let seeded = ensure_installation(&state, ConnectionType::Fibra, &current_user.id).await?;
        items.push(seeded);
    }
    if !has_antena {
        let seeded = ensure_installation(&state, ConnectionType::Antena, &current_user.id).await?;
        items.push(seeded);
    }

    // Ordenar estable: fibra primero.
    items.sort_by_key(|c| c.connection_type.as_slug().to_string());

    Ok(Json(AiInstallationConfigsListResponse {
        ok: true,
        data: items.into_iter().map(installation_to_item).collect(),
    }))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/ai-agent/installations/{type}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("type" = String, Path, description = "Tipo de conexión: 'fibra' o 'antena'")),
    request_body = UpdateAiInstallationConfigRequest,
    responses(
        (status = 200, description = "Configuración actualizada", body = AiInstallationConfigResponse),
        (status = 422, description = "type inválido"),
    )
)]
pub async fn update_installation_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(type_slug): Path<String>,
    Json(body): Json<UpdateAiInstallationConfigRequest>,
) -> Result<Json<AiInstallationConfigResponse>, ApiError> {
    require_superadmin(&current_user)?;

    let connection_type = ConnectionType::from_slug(&type_slug).ok_or_else(|| {
        ApiError::ValidationError {
            code: "invalid_connection_type".into(),
            field: "type".into(),
            message: format!("'{}' no es un tipo válido. Usar 'fibra' o 'antena'", type_slug),
        }
    })?;

    // Asegurar que el doc existe (lazy seed si no).
    let mut config = ensure_installation(&state, connection_type, &current_user.id).await?;

    // Aplicar PATCH semántico.
    if let Some(v) = body.base_cost_usd { config.base_cost_usd = v; }
    if let Some(v) = body.includes { config.includes = v.trim().to_string(); }
    if let Some(tri) = body.excedente_per_meter_usd {
        config.excedente_per_meter_usd = tri;
    }
    if let Some(v) = body.excedente_notes { config.excedente_notes = v.trim().to_string(); }
    if let Some(v) = body.notes { config.notes = v.trim().to_string(); }

    config.editor_id = current_user.id.clone();
    config.updated_at = BsonDateTime::now();

    let saved = state
        .db
        .upsert_ai_installation(config)
        .await
        .map_err(ApiError::DatabaseError)?;

    Ok(Json(AiInstallationConfigResponse {
        ok: true,
        data: installation_to_item(saved),
    }))
}

// ============================================
// CRUD Promotions
// ============================================

fn promotion_to_item(p: AiPromotion) -> AiPromotionItem {
    AiPromotionItem {
        id: p.id.map(|o| o.to_hex()).unwrap_or_default(),
        name: p.name,
        description: p.description,
        conditions: p.conditions,
        benefit: p.benefit,
        starts_at: iso(p.starts_at),
        ends_at: iso(p.ends_at),
        is_active: p.is_active,
        created_at: iso(p.created_at),
        updated_at: iso(p.updated_at),
    }
}

fn promotion_not_found() -> ApiError {
    ApiError::domain_simple(
        StatusCode::NOT_FOUND,
        "ai_promotion_not_found",
        "Promoción no encontrada",
    )
}

/// Parsea una fecha ISO8601 con timezone a BsonDateTime.
fn parse_iso_datetime(s: &str, field: &str) -> Result<mongodb::bson::DateTime, ApiError> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .map(|dt| mongodb::bson::DateTime::from_millis(dt.timestamp_millis()))
        .map_err(|_| ApiError::ValidationError {
            code: "invalid_datetime".into(),
            field: field.into(),
            message: format!("'{}' debe ser ISO8601 con timezone (ej: 2026-04-01T00:00:00-04:00)", field),
        })
}

fn validate_promotion_dates(
    starts_at: mongodb::bson::DateTime,
    ends_at: mongodb::bson::DateTime,
) -> Result<(), ApiError> {
    if ends_at <= starts_at {
        return Err(ApiError::ValidationError {
            code: "invalid_date_range".into(),
            field: "ends_at".into(),
            message: "'ends_at' debe ser posterior a 'starts_at'".into(),
        });
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/promotions",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Lista de promociones", body = AiPromotionsListResponse),
        (status = 403, description = "Requiere SUPERADMIN"),
    )
)]
pub async fn list_promotions_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
) -> Result<Json<AiPromotionsListResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let promos = state
        .db
        .list_ai_promotions()
        .await
        .map_err(ApiError::DatabaseError)?;
    Ok(Json(AiPromotionsListResponse {
        ok: true,
        data: promos.into_iter().map(promotion_to_item).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/v1/auth-user/whatsapp/ai-agent/promotions",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    request_body = CreateAiPromotionRequest,
    responses(
        (status = 201, description = "Promoción creada", body = AiPromotionResponse),
        (status = 422, description = "Validación (fechas, campos requeridos)"),
    )
)]
pub async fn create_promotion_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Json(body): Json<CreateAiPromotionRequest>,
) -> Result<(StatusCode, Json<AiPromotionResponse>), ApiError> {
    require_superadmin(&current_user)?;

    validate_required(body.name.trim(), "name")?;
    validate_required(body.description.trim(), "description")?;
    validate_required(body.benefit.trim(), "benefit")?;

    let starts_at = parse_iso_datetime(&body.starts_at, "starts_at")?;
    let ends_at = parse_iso_datetime(&body.ends_at, "ends_at")?;
    validate_promotion_dates(starts_at, ends_at)?;

    let now = BsonDateTime::now();
    let promo = AiPromotion {
        id: None,
        name: body.name.trim().to_string(),
        description: body.description.trim().to_string(),
        conditions: body.conditions.trim().to_string(),
        benefit: body.benefit.trim().to_string(),
        starts_at,
        ends_at,
        is_active: body.is_active.unwrap_or(true),
        created_at: now,
        updated_at: now,
    };

    let saved = state
        .db
        .create_ai_promotion(promo)
        .await
        .map_err(ApiError::DatabaseError)?;
    Ok((StatusCode::CREATED, Json(AiPromotionResponse { ok: true, data: promotion_to_item(saved) })))
}

#[utoipa::path(
    patch,
    path = "/v1/auth-user/whatsapp/ai-agent/promotions/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    request_body = UpdateAiPromotionRequest,
    responses(
        (status = 200, description = "Promoción actualizada", body = AiPromotionResponse),
        (status = 404, description = "ai_promotion_not_found"),
        (status = 422, description = "Validación"),
    )
)]
pub async fn update_promotion_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAiPromotionRequest>,
) -> Result<Json<AiPromotionResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let mut promo = state
        .db
        .find_ai_promotion_by_id(&oid)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(promotion_not_found)?;

    if let Some(v) = body.name { promo.name = v.trim().to_string(); }
    if let Some(v) = body.description { promo.description = v.trim().to_string(); }
    if let Some(v) = body.conditions { promo.conditions = v.trim().to_string(); }
    if let Some(v) = body.benefit { promo.benefit = v.trim().to_string(); }
    if let Some(v) = body.is_active { promo.is_active = v; }

    if let Some(ref s) = body.starts_at {
        promo.starts_at = parse_iso_datetime(s, "starts_at")?;
    }
    if let Some(ref e) = body.ends_at {
        promo.ends_at = parse_iso_datetime(e, "ends_at")?;
    }

    validate_promotion_dates(promo.starts_at, promo.ends_at)?;
    promo.updated_at = BsonDateTime::now();

    let saved = state
        .db
        .replace_ai_promotion(&oid, promo)
        .await
        .map_err(ApiError::DatabaseError)?
        .ok_or_else(promotion_not_found)?;
    Ok(Json(AiPromotionResponse { ok: true, data: promotion_to_item(saved) }))
}

#[utoipa::path(
    delete,
    path = "/v1/auth-user/whatsapp/ai-agent/promotions/{id}",
    tag = "WhatsApp — AI Agent",
    security(("bearerAuth" = [])),
    params(("id" = String, Path, description = "ObjectId hex")),
    responses(
        (status = 200, description = "Promoción eliminada", body = AiBusinessDataDeleteResponse),
        (status = 404, description = "ai_promotion_not_found"),
    )
)]
pub async fn delete_promotion_handler(
    State(state): State<Arc<AppState>>,
    Extension(current_user): Extension<User>,
    Path(id): Path<String>,
) -> Result<Json<AiBusinessDataDeleteResponse>, ApiError> {
    require_superadmin(&current_user)?;
    let oid = parse_oid(&id, "id")?;
    let ok = state
        .db
        .delete_ai_promotion(&oid)
        .await
        .map_err(ApiError::DatabaseError)?;
    if !ok {
        return Err(promotion_not_found());
    }
    Ok(Json(AiBusinessDataDeleteResponse { ok: true }))
}

// ============================================
// Tests
// ============================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_state_ok() {
        assert!(validate_state("Carabobo").is_ok());
        assert!(validate_state("Distrito Capital").is_ok());
    }

    #[test]
    fn test_validate_state_err() {
        let err = validate_state("Narnia").unwrap_err();
        if let ApiError::ValidationError { code, field, .. } = err {
            assert_eq!(code, "invalid_state");
            assert_eq!(field, "state");
        } else {
            panic!("Se esperaba ValidationError");
        }
    }

    #[test]
    fn test_validate_state_empty() {
        let err = validate_state("").unwrap_err();
        if let ApiError::ValidationError { code, field, .. } = err {
            assert_eq!(code, "missing_field");
            assert_eq!(field, "state");
        } else {
            panic!("Se esperaba ValidationError");
        }
    }

    #[test]
    fn test_validate_municipality_ok() {
        assert!(validate_municipality("Carabobo", "Valencia").is_ok());
        assert!(validate_municipality("Carabobo", "Naguanagua").is_ok());
    }

    #[test]
    fn test_validate_municipality_wrong_state() {
        // Libertador existe en Miranda, NO en Carabobo (hay otro Libertador en Carabobo también)
        // Usemos uno que definitivamente no está en Carabobo: "Baruta" pertenece a Miranda
        let err = validate_municipality("Carabobo", "Baruta").unwrap_err();
        if let ApiError::ValidationError { code, field, .. } = err {
            assert_eq!(code, "invalid_municipality");
            assert_eq!(field, "municipality");
        } else {
            panic!("Se esperaba ValidationError");
        }
    }

    #[test]
    fn test_validate_municipality_empty() {
        let err = validate_municipality("Carabobo", "").unwrap_err();
        if let ApiError::ValidationError { code, field, .. } = err {
            assert_eq!(code, "missing_field");
            assert_eq!(field, "municipality");
        } else {
            panic!("Se esperaba ValidationError");
        }
    }

    #[test]
    fn test_normalize_aliases_dedup() {
        // "Naguanagua" y "naguanagua" deben deduplicarse (mismo normalized key)
        let input = vec!["Naguanagua".to_string(), "naguanagua".to_string()];
        let result = normalize_aliases(input).unwrap();
        assert_eq!(result.len(), 1, "Debe deduplicar aliases con misma normalización");
    }

    #[test]
    fn test_normalize_aliases_max_count() {
        let input: Vec<String> = (0..6).map(|i| format!("alias{}", i)).collect();
        let err = normalize_aliases(input).unwrap_err();
        if let ApiError::ValidationError { code, field, .. } = err {
            assert_eq!(code, "too_many_aliases");
            assert_eq!(field, "aliases");
        } else {
            panic!("Se esperaba ValidationError");
        }
    }

    #[test]
    fn test_normalize_aliases_item_too_long() {
        let long_alias = "a".repeat(101);
        let input = vec![long_alias];
        let err = normalize_aliases(input).unwrap_err();
        if let ApiError::ValidationError { code, field, .. } = err {
            assert_eq!(code, "field_too_long");
            assert_eq!(field, "aliases");
        } else {
            panic!("Se esperaba ValidationError");
        }
    }

    #[test]
    fn test_normalize_aliases_trims_drops_empty() {
        let input = vec!["  ".to_string(), "Valencia".to_string(), "".to_string()];
        let result = normalize_aliases(input).unwrap();
        assert_eq!(result, vec!["Valencia".to_string()]);
    }
}
