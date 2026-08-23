//! Prompt import from deep link
//!
//! Handles importing prompt configurations via ccswitch:// URLs.

use super::utils::decode_base64_param;
use super::DeepLinkImportRequest;
use crate::error::AppError;
use crate::prompt::Prompt;
use crate::services::PromptService;
use crate::store::AppState;
use crate::AppType;
use sha2::{Digest, Sha256};
use std::str::FromStr;

fn stable_deeplink_prompt_id(
    app_type: &AppType,
    name: &str,
    content: &str,
    description: Option<&str>,
) -> String {
    let sanitized_name = name
        .chars()
        .filter(|character| character.is_alphanumeric() || matches!(*character, '-' | '_'))
        .take(64)
        .collect::<String>()
        .to_lowercase();
    let mut hasher = Sha256::new();
    for part in [
        app_type.as_str(),
        name,
        content,
        description.unwrap_or_default(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let fingerprint = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let id_name = if sanitized_name.is_empty() {
        "prompt"
    } else {
        &sanitized_name
    };
    format!("deeplink-{id_name}-{fingerprint}")
}

/// Import a prompt from deep link request
pub fn import_prompt_from_deeplink(
    state: &AppState,
    request: DeepLinkImportRequest,
) -> Result<String, AppError> {
    // Verify this is a prompt request
    if request.resource != "prompt" {
        return Err(AppError::InvalidInput(format!(
            "Expected prompt resource, got '{}'",
            request.resource
        )));
    }

    // Extract required fields
    let app_str = request
        .app
        .as_ref()
        .ok_or_else(|| AppError::InvalidInput("Missing 'app' field for prompt".to_string()))?;

    let name = request
        .name
        .ok_or_else(|| AppError::InvalidInput("Missing 'name' field for prompt".to_string()))?;

    // Parse app type
    let app_type = AppType::from_str(app_str)
        .map_err(|_| AppError::InvalidInput(format!("Invalid app type: {app_str}")))?;

    // Decode content
    let content_b64 = request
        .content
        .as_ref()
        .ok_or_else(|| AppError::InvalidInput("Missing 'content' field for prompt".to_string()))?;

    let content = decode_base64_param("content", content_b64)?;
    let content = String::from_utf8(content)
        .map_err(|e| AppError::InvalidInput(format!("Invalid UTF-8 in content: {e}")))?;

    let timestamp = chrono::Utc::now().timestamp_millis();
    let id = stable_deeplink_prompt_id(&app_type, &name, &content, request.description.as_deref());
    let created_at = state
        .db
        .get_prompts(app_type.as_str())?
        .get(&id)
        .and_then(|prompt| prompt.created_at)
        .unwrap_or(timestamp);

    // Check if we should enable this prompt
    let should_enable = request.enabled.unwrap_or(false);

    // Create Prompt (initially disabled)
    let prompt = Prompt {
        id: id.clone(),
        name: name.clone(),
        content,
        description: request.description,
        enabled: false, // Always start as disabled, will be enabled later if needed
        created_at: Some(created_at),
        updated_at: Some(timestamp),
    };

    // Save using PromptService
    PromptService::upsert_prompt(state, app_type.clone(), &id, prompt)?;

    // If enabled flag is set, enable this prompt (which will disable others)
    if should_enable {
        PromptService::enable_prompt(state, app_type, &id)?;
        log::info!("Successfully imported and enabled prompt '{name}' for {app_str}");
    } else {
        log::info!("Successfully imported prompt '{name}' for {app_str} (disabled)");
    }

    Ok(id)
}
