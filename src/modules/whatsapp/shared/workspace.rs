use std::sync::Arc;

use crate::{db::WhatsAppRepository, state::AppState};

pub(crate) async fn resolve_workspace_name(
    state: &Arc<AppState>,
    business_phone: &str,
) -> Option<String> {
    if business_phone.is_empty() {
        return None;
    }

    state
        .db
        .get_workspace_names(&[business_phone.to_string()])
        .await
        .ok()
        .and_then(|m| m.get(business_phone).cloned())
}
