#![allow(non_snake_case)]
use crate::error::AppError;

use crate::session_manager;

#[tauri::command]
pub async fn list_sessions() -> Result<Vec<session_manager::SessionMeta>, AppError> {
    let sessions = tauri::async_runtime::spawn_blocking(session_manager::scan_sessions)
        .await
        .map_err(|e| AppError::from(format!("Failed to scan sessions: {e}")))?;
    Ok(sessions)
}

#[tauri::command]
pub async fn get_session_messages(
    providerId: String,
    sourcePath: String,
) -> Result<Vec<session_manager::SessionMessage>, AppError> {
    let provider_id = providerId.clone();
    let source_path = sourcePath.clone();
    tauri::async_runtime::spawn_blocking(move || {
        session_manager::load_messages(&provider_id, &source_path)
    })
    .await
    .map_err(|e| AppError::from(format!("Failed to load session messages: {e}")))?
    .map_err(AppError::from)
}

/// Resume a known session. The renderer supplies identifiers only; the backend
/// verifies them against a fresh scan and constructs the shell command itself.
#[tauri::command]
pub async fn launch_session_terminal(
    providerId: String,
    sessionId: String,
    sourcePath: String,
) -> Result<bool, AppError> {
    // Read preferred terminal from global settings
    let preferred = crate::settings::get_preferred_terminal();
    // Map global setting terminal names to session terminal names
    // Global uses "iterm2", session terminal uses "iterm"
    let target = match preferred.as_deref() {
        Some("iterm2") => "iterm".to_string(),
        Some(t) => t.to_string(),
        None => "terminal".to_string(), // Default to Terminal.app on macOS
    };

    tauri::async_runtime::spawn_blocking(move || {
        let session = session_manager::scan_sessions()
            .into_iter()
            .find(|session| {
                session.provider_id == providerId
                    && session.session_id == sessionId
                    && session.source_path.as_deref() == Some(sourcePath.as_str())
            })
            .ok_or_else(|| "Session no longer exists or its source has changed".to_string())?;
        let command = session_manager::terminal::build_resume_command(
            &session.provider_id,
            &session.session_id,
            session.source_path.as_deref(),
        )?
        .ok_or_else(|| "This provider does not support terminal resume".to_string())?;

        session_manager::terminal::launch_terminal(
            &target,
            &command,
            session.project_dir.as_deref(),
            None,
        )
    })
    .await
    .map_err(|e| AppError::from(format!("Failed to launch terminal: {e}")))??;

    Ok(true)
}

#[tauri::command]
pub async fn delete_session(
    providerId: String,
    sessionId: String,
    sourcePath: String,
) -> Result<bool, AppError> {
    let provider_id = providerId.clone();
    let session_id = sessionId.clone();
    let source_path = sourcePath.clone();

    tauri::async_runtime::spawn_blocking(move || {
        session_manager::delete_session(&provider_id, &session_id, &source_path)
    })
    .await
    .map_err(|e| AppError::from(format!("Failed to delete session: {e}")))?
    .map_err(AppError::from)
}

#[tauri::command]
pub async fn delete_sessions(
    items: Vec<session_manager::DeleteSessionRequest>,
) -> Result<Vec<session_manager::DeleteSessionOutcome>, AppError> {
    tauri::async_runtime::spawn_blocking(move || session_manager::delete_sessions(&items))
        .await
        .map_err(|e| AppError::from(format!("Failed to delete sessions: {e}")))
}
