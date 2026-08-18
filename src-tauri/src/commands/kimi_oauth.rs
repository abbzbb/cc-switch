//! Kimi For Coding OAuth state wrapper.

use crate::proxy::providers::kimi_oauth_auth::KimiOAuthManager;
use crate::services::subscription::{CredentialStatus, SubscriptionQuota};
use crate::store::AppState;
use std::sync::Arc;
use tauri::State;

pub struct KimiOAuthState(pub Arc<KimiOAuthManager>);

#[tauri::command(rename_all = "camelCase")]
pub async fn get_kimi_oauth_quota(
    account_id: Option<String>,
    state: State<'_, KimiOAuthState>,
    app_state: State<'_, AppState>,
) -> Result<SubscriptionQuota, String> {
    let manager = &state.0;
    let resolved = match account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(id) => Some(id.to_string()),
        None => manager.default_account_id().await,
    };
    let Some(id) = resolved else {
        return Ok(SubscriptionQuota::not_found("kimi_oauth"));
    };

    let token = match manager.get_valid_token_for_account(&id).await {
        Ok(token) => token,
        Err(error) => {
            return Ok(SubscriptionQuota::error(
                "kimi_oauth",
                CredentialStatus::Expired,
                format!("Kimi OAuth token unavailable: {error}"),
            ));
        }
    };

    let quota = crate::services::coding_plan::get_coding_plan_quota(
        crate::proxy::providers::KIMI_CODING_API_BASE_URL,
        &token,
        None,
        None,
        None,
        None,
        None,
    )
    .await?;
    app_state
        .proxy_service
        .record_official_quota_snapshot(&id, &quota)
        .await;
    Ok(quota)
}
