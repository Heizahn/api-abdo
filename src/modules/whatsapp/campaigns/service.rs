use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use futures::stream::StreamExt;
use mongodb::bson::{doc, oid::ObjectId, Bson, Document};

use crate::{error::ApiError, state::AppState};

use super::{
    dto::{
        BalanceFilter, CampaignPreviewRecipient, CampaignPreviewRequest, CampaignPreviewResponse,
        CampaignPreviewTotals, ClientStateFilter, DerivedClientState, PhoneStatus,
    },
    phone::normalize_phone_to_whatsapp,
};

const DEFAULT_PER_PAGE: u32 = 100;
const MAX_PER_PAGE: u32 = 500;

struct CandidateClient {
    id: String,
    name: String,
    phone: String,
    provider_id: Option<String>,
    provider_name: Option<String>,
    provider_tag: Option<String>,
    sector_id: Option<String>,
    sector_name: Option<String>,
    state: String,
    balance: f64,
}

pub async fn preview_recipients(
    state: &AppState,
    request: CampaignPreviewRequest,
) -> Result<CampaignPreviewResponse, ApiError> {
    if !has_allowed_filter(&request) {
        return Err(ApiError::domain_simple(
            StatusCode::BAD_REQUEST,
            "campaign_preview_requires_filter",
            "Provide at least one filter, or explicitly request all active clients.",
        ));
    }

    let page = request.page.unwrap_or(1).max(1);
    let per_page = request
        .per_page
        .unwrap_or(DEFAULT_PER_PAGE)
        .clamp(1, MAX_PER_PAGE);
    let filter = build_client_filter(&request)?;
    let projection = doc! {
        "_id": 1,
        "sName": 1,
        "sPhone": 1,
        "idOwner": 1,
        "idSector": 1,
        "sState": 1,
        "nBalance": 1,
    };

    let mut cursor = state
        .db
        .db
        .collection::<Document>("Clients")
        .find(filter)
        .projection(projection)
        .sort(doc! { "sName": 1, "_id": 1 })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let mut candidates = Vec::new();
    let mut provider_ids = HashSet::new();
    let mut sector_ids = HashSet::new();

    while let Some(result) = cursor.next().await {
        let doc = result.map_err(|e| ApiError::DatabaseError(e.to_string()))?;
        let provider_id = get_string_or_object_id(&doc, "idOwner");
        let sector_id = get_string_or_object_id(&doc, "idSector");

        if let Some(id) = provider_id.as_ref() {
            provider_ids.insert(id.clone());
        }
        if let Some(id) = sector_id.as_ref() {
            sector_ids.insert(id.clone());
        }

        candidates.push(CandidateClient {
            id: doc
                .get_object_id("_id")
                .map(|id| id.to_hex())
                .unwrap_or_default(),
            name: doc.get_str("sName").unwrap_or_default().to_string(),
            phone: doc.get_str("sPhone").unwrap_or_default().to_string(),
            provider_id,
            provider_name: None,
            provider_tag: None,
            sector_id,
            sector_name: None,
            state: doc.get_str("sState").unwrap_or_default().to_string(),
            balance: get_bson_amount(&doc, "nBalance"),
        });
    }

    let providers = load_providers(state, provider_ids).await?;
    let sectors = load_sectors(state, sector_ids).await?;

    let (totals, recipients) = build_preview_recipients(candidates, &providers, &sectors);
    let start = ((page - 1) * per_page) as usize;
    let recipients = recipients
        .into_iter()
        .skip(start)
        .take(per_page as usize)
        .collect();

    Ok(CampaignPreviewResponse {
        ok: true,
        totals,
        recipients,
        page,
        per_page,
    })
}

fn build_preview_recipients(
    candidates: Vec<CandidateClient>,
    providers: &HashMap<String, ProviderInfo>,
    sectors: &HashMap<String, String>,
) -> (CampaignPreviewTotals, Vec<CampaignPreviewRecipient>) {
    let mut totals = CampaignPreviewTotals {
        matched: candidates.len(),
        ..Default::default()
    };
    let mut seen_phones = HashSet::new();
    let mut recipients = Vec::with_capacity(candidates.len());

    for mut candidate in candidates {
        if let Some(provider) = candidate
            .provider_id
            .as_ref()
            .and_then(|id| providers.get(id))
        {
            candidate.provider_name = provider.name.clone();
            candidate.provider_tag = provider.tag.clone();
        }
        if let Some(sector_name) = candidate.sector_id.as_ref().and_then(|id| sectors.get(id)) {
            candidate.sector_name = Some(sector_name.clone());
        }

        let derived = derive_client_state(&candidate.state, candidate.balance);
        let normalized = normalize_phone_to_whatsapp(&candidate.phone).ok();
        let (phone_status, can_send, reason) = match normalized.as_ref() {
            None => {
                totals.invalid_phone += 1;
                (
                    PhoneStatus::Invalid,
                    false,
                    Some("invalid_phone".to_string()),
                )
            }
            Some(phone) if !seen_phones.insert(phone.clone()) => {
                totals.duplicated_phone += 1;
                (
                    PhoneStatus::Duplicated,
                    false,
                    Some("duplicated_phone".to_string()),
                )
            }
            Some(_) => {
                totals.can_send += 1;
                (PhoneStatus::Valid, true, None)
            }
        };

        recipients.push(CampaignPreviewRecipient {
            client_id: candidate.id,
            name: candidate.name,
            phone_original: candidate.phone,
            phone_normalized: normalized,
            phone_status,
            can_send,
            reason,
            provider_id: candidate.provider_id,
            provider_name: candidate.provider_name,
            provider_tag: candidate.provider_tag,
            sector_id: candidate.sector_id,
            sector_name: candidate.sector_name,
            client_state_raw: candidate.state,
            client_state_derived: derived,
            balance: candidate.balance,
        });
    }

    (totals, recipients)
}

fn has_allowed_filter(request: &CampaignPreviewRequest) -> bool {
    has_values(&request.provider_ids)
        || has_values(&request.sector_ids)
        || request.balance_filter.is_some()
        || matches!(request.client_state, Some(ClientStateFilter::Active))
        || request.include_all_active.unwrap_or(false)
        || matches!(
            request.client_state,
            Some(
                ClientStateFilter::Suspended
                    | ClientStateFilter::Moroso
                    | ClientStateFilter::Solvente
            )
        )
}

fn has_values(values: &Option<Vec<String>>) -> bool {
    values
        .as_ref()
        .is_some_and(|items| items.iter().any(|item| !item.trim().is_empty()))
}

fn build_client_filter(request: &CampaignPreviewRequest) -> Result<Document, ApiError> {
    let mut clauses: Vec<Document> = Vec::new();

    if let Some(provider_ids) = non_empty_ids(&request.provider_ids) {
        clauses.push(doc! { "idOwner": { "$in": string_or_object_id_bsons(provider_ids) } });
    }

    if let Some(sector_ids) = non_empty_ids(&request.sector_ids) {
        clauses.push(doc! { "idSector": { "$in": string_or_object_id_bsons(sector_ids) } });
    }

    if let Some(balance_filter) = &request.balance_filter {
        clauses.push(doc! { "nBalance": build_balance_filter(balance_filter)? });
    }

    match request.client_state.as_ref() {
        Some(ClientStateFilter::Active) => clauses.push(doc! { "sState": "Activo" }),
        Some(ClientStateFilter::Suspended) => clauses.push(doc! { "sState": "Suspendido" }),
        Some(ClientStateFilter::Moroso) => {
            clauses.push(doc! { "sState": "Activo", "nBalance": { "$lt": 0.0 } })
        }
        Some(ClientStateFilter::Solvente) => {
            clauses.push(doc! { "sState": "Activo", "nBalance": { "$gte": 0.0 } })
        }
        Some(ClientStateFilter::Any) | None => {
            if request.include_all_active.unwrap_or(false) {
                clauses.push(doc! { "sState": "Activo" });
            }
        }
    }

    Ok(if clauses.len() == 1 {
        clauses.remove(0)
    } else {
        doc! { "$and": clauses }
    })
}

fn build_balance_filter(filter: &BalanceFilter) -> Result<Document, ApiError> {
    let mut doc = Document::new();
    if let Some(value) = filter.lt {
        doc.insert("$lt", value);
    }
    if let Some(value) = filter.lte {
        doc.insert("$lte", value);
    }
    if let Some(value) = filter.gt {
        doc.insert("$gt", value);
    }
    if let Some(value) = filter.gte {
        doc.insert("$gte", value);
    }
    if let Some(value) = filter.eq {
        doc.insert("$eq", value);
    }
    if let Some(range) = &filter.between {
        doc.insert("$gte", range.min);
        doc.insert("$lte", range.max);
    }

    if doc.is_empty() {
        return Err(ApiError::BadRequest("invalid_balance_filter".to_string()));
    }

    Ok(doc)
}

fn non_empty_ids(ids: &Option<Vec<String>>) -> Option<Vec<String>> {
    ids.as_ref()
        .map(|values| {
            values
                .iter()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|values| !values.is_empty())
}

fn string_or_object_id_bsons(ids: Vec<String>) -> Vec<Bson> {
    ids.into_iter()
        .flat_map(|id| {
            let mut values = vec![Bson::String(id.clone())];
            if let Ok(object_id) = ObjectId::parse_str(&id) {
                values.push(Bson::ObjectId(object_id));
            }
            values
        })
        .collect()
}

#[derive(Default)]
struct ProviderInfo {
    name: Option<String>,
    tag: Option<String>,
}

async fn load_providers(
    state: &AppState,
    provider_ids: HashSet<String>,
) -> Result<std::collections::HashMap<String, ProviderInfo>, ApiError> {
    if provider_ids.is_empty() {
        return Ok(Default::default());
    }

    let id_values = string_or_object_id_bsons(provider_ids.into_iter().collect());
    let mut cursor = state
        .db
        .db
        .collection::<Document>("Users")
        .find(doc! { "_id": { "$in": id_values } })
        .projection(doc! { "_id": 1, "sName": 1, "nTag": 1 })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let mut providers = std::collections::HashMap::new();
    while let Some(result) = cursor.next().await {
        let doc = result.map_err(|e| ApiError::DatabaseError(e.to_string()))?;
        let id = get_string_or_object_id(&doc, "_id").unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let tag = get_numeric_tag(&doc, "nTag").map(|tag| format!("ABDO77-{tag}"));
        providers.insert(
            id,
            ProviderInfo {
                name: doc.get_str("sName").ok().map(|name| name.to_string()),
                tag,
            },
        );
    }

    Ok(providers)
}

async fn load_sectors(
    state: &AppState,
    sector_ids: HashSet<String>,
) -> Result<std::collections::HashMap<String, String>, ApiError> {
    if sector_ids.is_empty() {
        return Ok(Default::default());
    }

    let id_values = string_or_object_id_bsons(sector_ids.into_iter().collect());
    let mut cursor = state
        .db
        .db
        .collection::<Document>("Sectors")
        .find(doc! { "_id": { "$in": id_values } })
        .projection(doc! { "_id": 1, "sName": 1 })
        .await
        .map_err(|e| ApiError::DatabaseError(e.to_string()))?;

    let mut sectors = std::collections::HashMap::new();
    while let Some(result) = cursor.next().await {
        let doc = result.map_err(|e| ApiError::DatabaseError(e.to_string()))?;
        if let (Some(id), Ok(name)) = (get_string_or_object_id(&doc, "_id"), doc.get_str("sName")) {
            sectors.insert(id, name.to_string());
        }
    }

    Ok(sectors)
}

fn get_string_or_object_id(doc: &Document, field: &str) -> Option<String> {
    doc.get_str(field)
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| doc.get_object_id(field).ok().map(|id| id.to_hex()))
}

fn get_numeric_tag(doc: &Document, field: &str) -> Option<i64> {
    doc.get_i32(field)
        .ok()
        .map(i64::from)
        .or_else(|| doc.get_i64(field).ok())
}

fn derive_client_state(raw: &str, balance: f64) -> DerivedClientState {
    if raw != "Activo" {
        DerivedClientState::Suspended
    } else if balance < 0.0 {
        DerivedClientState::Moroso
    } else {
        DerivedClientState::Solvente
    }
}

fn get_bson_amount(doc: &Document, key: &str) -> f64 {
    doc.get_f64(key)
        .or_else(|_| doc.get_i32(key).map(|v| v as f64))
        .or_else(|_| doc.get_i64(key).map(|v| v as f64))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, name: &str, phone: &str) -> CandidateClient {
        CandidateClient {
            id: id.to_string(),
            name: name.to_string(),
            phone: phone.to_string(),
            provider_id: None,
            provider_name: None,
            provider_tag: None,
            sector_id: None,
            sector_name: None,
            state: "Activo".to_string(),
            balance: 0.0,
        }
    }

    #[test]
    fn suspended_filter_matches_only_suspended_state() {
        let request = CampaignPreviewRequest {
            provider_ids: None,
            sector_ids: None,
            balance_filter: None,
            client_state: Some(ClientStateFilter::Suspended),
            include_all_active: None,
            page: None,
            per_page: None,
        };

        assert_eq!(
            build_client_filter(&request).unwrap(),
            doc! { "sState": "Suspendido" }
        );
    }

    #[test]
    fn duplicate_detection_is_global_and_keeps_first_valid_occurrence() {
        let (totals, recipients) = build_preview_recipients(
            vec![
                candidate("1", "First", "0412 123 4567"),
                candidate("2", "Second", "4121234567"),
                candidate("3", "Third", "0414 111 1111"),
            ],
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(totals.matched, 3);
        assert_eq!(totals.can_send, 2);
        assert_eq!(totals.duplicated_phone, 1);
        assert_eq!(totals.invalid_phone, 0);

        assert!(recipients[0].can_send);
        assert!(matches!(recipients[0].phone_status, PhoneStatus::Valid));
        assert!(!recipients[1].can_send);
        assert!(matches!(
            recipients[1].phone_status,
            PhoneStatus::Duplicated
        ));
        assert!(recipients[2].can_send);

        let page_two = recipients.into_iter().skip(1).take(1).collect::<Vec<_>>();
        assert!(matches!(page_two[0].phone_status, PhoneStatus::Duplicated));
        assert!(!page_two[0].can_send);
    }
}
