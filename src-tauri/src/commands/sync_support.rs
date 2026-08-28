use serde_json::{json, Value};

use crate::error::AppError;
use crate::services::{model_pricing, PromptService, ProviderService};
use crate::settings;
use crate::store::AppState;

const POST_IMPORT_SYNC_PENDING_KEY: &str = "post_import_sync_pending";

fn perform_post_import_sync(app_state: &AppState) -> Result<(), AppError> {
    let mut failures = Vec::new();

    if let Err(error) = ProviderService::sync_current_to_live(app_state) {
        failures.push(format!("live configuration: {error}"));
    }
    if let Err(error) = PromptService::sync_all_to_live(app_state) {
        failures.push(format!("prompts: {error}"));
    }
    if let Err(error) = model_pricing::sync_local_model_pricing(&app_state.db) {
        failures.push(format!("model pricing: {error}"));
    }
    if let Err(error) = settings::reload_settings() {
        failures.push(format!("settings cache: {error}"));
    }

    match app_state.db.get_log_config() {
        Ok(log_config) => log::set_max_level(log_config.to_level_filter()),
        Err(error) => {
            log::set_max_level(log::LevelFilter::Info);
            failures.push(format!("runtime log level: {error}"));
        }
    }
    app_state.usage_cache.invalidate_all();

    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "部分导入后同步失败: {}",
            failures.join("; ")
        )))
    }
}

fn run_with_pending_marker<F>(app_state: &AppState, sync: F) -> Result<(), AppError>
where
    F: FnOnce() -> Result<(), AppError>,
{
    app_state
        .db
        .set_setting(POST_IMPORT_SYNC_PENDING_KEY, "true")?;
    sync()?;
    app_state
        .db
        .set_setting(POST_IMPORT_SYNC_PENDING_KEY, "false")
}

pub(crate) fn run_post_import_sync(app_state: &AppState) -> Result<(), AppError> {
    run_with_pending_marker(app_state, || perform_post_import_sync(app_state))
}

pub(crate) fn retry_pending_post_import_sync(app_state: &AppState) -> Result<bool, AppError> {
    if !app_state.db.get_bool_flag(POST_IMPORT_SYNC_PENDING_KEY)? {
        return Ok(false);
    }

    run_post_import_sync(app_state)?;
    Ok(true)
}

fn post_sync_warning<E: std::fmt::Display>(err: E) -> String {
    AppError::localized(
        "sync.post_operation_sync_failed",
        format!("后置同步状态失败: {err}"),
        format!("Post-operation synchronization failed: {err}"),
    )
    .to_string()
}

pub(crate) fn post_sync_warning_from_result(
    result: Result<Result<(), AppError>, AppError>,
) -> Option<String> {
    match result {
        Ok(Ok(())) => None,
        Ok(Err(err)) => Some(post_sync_warning(err)),
        Err(err) => Some(post_sync_warning(err)),
    }
}

pub(crate) fn attach_warning(mut value: Value, warning: Option<String>) -> Value {
    if let Some(message) = warning {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("warning".to_string(), Value::String(message));
        }
    }
    value
}

pub(crate) fn success_payload_with_warning(backup_id: String, warning: Option<String>) -> Value {
    attach_warning(
        json!({
            "success": true,
            "message": "SQL imported successfully",
            "backupId": backup_id
        }),
        warning,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        attach_warning, post_sync_warning_from_result, run_with_pending_marker,
        POST_IMPORT_SYNC_PENDING_KEY,
    };
    use crate::database::Database;
    use crate::store::AppState;
    use serde_json::json;
    use std::sync::Arc;

    fn test_state() -> AppState {
        AppState::new(Arc::new(Database::memory().expect("memory database")))
    }

    #[test]
    fn pending_marker_is_cleared_only_after_success() {
        let state = test_state();
        run_with_pending_marker(&state, || Ok(())).expect("sync succeeds");
        assert!(!state
            .db
            .get_bool_flag(POST_IMPORT_SYNC_PENDING_KEY)
            .expect("read marker"));
    }

    #[test]
    fn pending_marker_survives_projection_failure() {
        let state = test_state();
        let result = run_with_pending_marker(&state, || {
            Err(crate::error::AppError::Config("projection failed".into()))
        });
        assert!(result.is_err());
        assert!(state
            .db
            .get_bool_flag(POST_IMPORT_SYNC_PENDING_KEY)
            .expect("read marker"));
    }

    #[test]
    fn post_sync_warning_from_result_returns_none_on_success() {
        let warning = post_sync_warning_from_result(Ok(Ok(())));
        assert!(warning.is_none());
    }

    #[test]
    fn post_sync_warning_from_result_returns_some_on_sync_error() {
        let warning =
            post_sync_warning_from_result(Ok(Err(crate::error::AppError::Config("boom".into()))));
        assert!(warning.is_some());
    }

    #[tokio::test]
    async fn post_sync_warning_from_result_returns_some_on_join_error() {
        let handle = tokio::spawn(async move {
            panic!("forced join error");
        });
        let join_err = handle.await.expect_err("task should panic");
        let warning =
            post_sync_warning_from_result(Err(crate::error::AppError::from(join_err.to_string())));
        assert!(warning.is_some());
    }

    #[test]
    fn attach_warning_adds_warning_without_dropping_existing_fields() {
        let payload = json!({ "status": "downloaded" });
        let updated = attach_warning(payload, Some("post sync warning".to_string()));
        assert_eq!(
            updated.get("status").and_then(|v| v.as_str()),
            Some("downloaded")
        );
        assert_eq!(
            updated.get("warning").and_then(|v| v.as_str()),
            Some("post sync warning")
        );
    }
}
