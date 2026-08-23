//! Anthropic Claude Pro/Max OAuth state wrapper.

use crate::error::AppError;
use crate::proxy::providers::anthropic_oauth_auth::AnthropicOAuthManager;
use crate::services::subscription::{CredentialStatus, SubscriptionQuota};
use crate::store::AppState;
use std::sync::Arc;
use tauri::State;

pub struct AnthropicOAuthState(pub Arc<AnthropicOAuthManager>);

#[tauri::command(rename_all = "camelCase")]
pub async fn get_anthropic_oauth_quota(
    account_id: Option<String>,
    state: State<'_, AnthropicOAuthState>,
    app_state: State<'_, AppState>,
) -> Result<SubscriptionQuota, AppError> {
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
        return Ok(SubscriptionQuota::not_found("anthropic_oauth"));
    };

    let token = match manager.get_valid_token_for_account(&id).await {
        Ok(token) => token,
        Err(error) => {
            return Ok(SubscriptionQuota::error(
                "anthropic_oauth",
                CredentialStatus::Expired,
                format!("Claude OAuth token unavailable: {error}"),
            ));
        }
    };

    let quota = crate::services::subscription::query_claude_quota(&token).await?;
    app_state
        .proxy_service
        .record_official_quota_snapshot(&id, &quota)
        .await;
    Ok(quota)
}
