//! Datos de negocio editables desde el front:
//! - Planes (`AiPlans`) que la tool `list_plans` devuelve a la IA.
//! - Zonas de cobertura (`AiCoverageZones`) que la tool `check_coverage` matchea.
//!
//! También expone:
//! - Discovery (`GET /tools`) con metadata de todas las tools soportadas.
//! - División política canónica (`GET /coverage-zones/political-divisions`).

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
    db::AiAgentRepository,
    error::ApiError,
    models::{
        ai_agent::{
            AiBusinessDataDeleteResponse, AiCoverageZone, AiCoverageZoneItem,
            AiCoverageZoneResponse, AiCoverageZonesListResponse, AiPlan, AiPlanItem,
            AiPlanResponse, AiPlansListResponse, CreateAiCoverageZoneRequest,
            CreateAiPlanRequest, PoliticalDivisionItem, PoliticalDivisionsResponse,
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
// GET /coverage-zones/political-divisions
// ============================================

#[utoipa::path(
    get,
    path = "/v1/auth-user/whatsapp/ai-agent/coverage-zones/political-divisions",
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
