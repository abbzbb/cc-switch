use indexmap::IndexMap;
use std::str::FromStr;

use tauri::State;

use crate::app_config::AppType;
use crate::error::AppError;
use crate::prompt::Prompt;
use crate::services::pi_prompt_files::{
    PiPromptFileKind, PiPromptFileService, PiPromptFileSnapshot, PiPromptTemplate,
    PiPromptTemplateService,
};
use crate::services::prompt::PromptService;
use crate::store::AppState;

#[tauri::command]
pub async fn get_prompts(
    app: String,
    state: State<'_, AppState>,
) -> Result<IndexMap<String, Prompt>, AppError> {
    let app_type = AppType::from_str(&app)?;
    PromptService::get_prompts(&state, app_type)
}

#[tauri::command]
pub async fn upsert_prompt(
    app: String,
    id: String,
    prompt: Prompt,
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    let app_type = AppType::from_str(&app)?;
    PromptService::upsert_prompt(&state, app_type, &id, prompt)
}

#[tauri::command]
pub async fn delete_prompt(
    app: String,
    id: String,
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    let app_type = AppType::from_str(&app)?;
    PromptService::delete_prompt(&state, app_type, &id)
}

#[tauri::command]
pub async fn enable_prompt(
    app: String,
    id: String,
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    let app_type = AppType::from_str(&app)?;
    PromptService::enable_prompt(&state, app_type, &id)
}

#[tauri::command]
pub async fn import_prompt_from_file(
    app: String,
    state: State<'_, AppState>,
) -> Result<String, AppError> {
    let app_type = AppType::from_str(&app)?;
    PromptService::import_from_file(&state, app_type)
}

#[tauri::command]
pub async fn get_current_prompt_file_content(app: String) -> Result<Option<String>, AppError> {
    let app_type = AppType::from_str(&app)?;
    PromptService::get_current_file_content(app_type)
}

#[tauri::command]
pub async fn get_pi_prompt_file(kind: PiPromptFileKind) -> Result<PiPromptFileSnapshot, AppError> {
    PiPromptFileService::read(kind)
}

#[tauri::command]
pub async fn replace_pi_prompt_file(
    kind: PiPromptFileKind,
    #[allow(non_snake_case)] expectedRevision: String,
    content: String,
) -> Result<PiPromptFileSnapshot, AppError> {
    PiPromptFileService::replace(kind, &expectedRevision, &content)
}

#[tauri::command]
pub async fn delete_pi_prompt_file(
    kind: PiPromptFileKind,
    #[allow(non_snake_case)] expectedRevision: String,
) -> Result<bool, AppError> {
    PiPromptFileService::delete(kind, &expectedRevision)
}

#[tauri::command]
pub async fn list_pi_prompt_templates() -> Result<Vec<PiPromptTemplate>, AppError> {
    PiPromptTemplateService::list()
}

#[tauri::command]
pub async fn upsert_pi_prompt_template(
    slug: String,
    #[allow(non_snake_case)] originalSlug: Option<String>,
    #[allow(non_snake_case)] expectedRevision: String,
    content: String,
) -> Result<PiPromptTemplate, AppError> {
    PiPromptTemplateService::upsert(&slug, originalSlug.as_deref(), &expectedRevision, &content)
}

#[tauri::command]
pub async fn delete_pi_prompt_template(
    slug: String,
    #[allow(non_snake_case)] expectedRevision: String,
) -> Result<bool, AppError> {
    PiPromptTemplateService::delete(&slug, &expectedRevision)
}
