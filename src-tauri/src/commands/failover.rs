//! 故障转移队列命令
//!
//! 管理代理模式下的故障转移队列（基于 providers 表的 in_failover_queue 字段）

use crate::database::FailoverQueueItem;
use crate::error::AppError;
use crate::provider::Provider;
use crate::store::AppState;
use std::str::FromStr;
use tauri::Emitter;

fn require_failover_app(app_type: &str) -> Result<(), AppError> {
    let app = crate::app_config::AppType::from_str(app_type)
        .map_err(|error| AppError::from(format!("无效的应用类型: {error}")))?;
    if !app.supports_local_proxy() {
        return Err(AppError::from(format!("{} 不支持故障转移", app.as_str())));
    }
    Ok(())
}

fn require_failover_provider(
    db: &crate::database::Database,
    app_type: &str,
    provider_id: &str,
) -> Result<Provider, AppError> {
    let provider = db
        .get_provider_by_id(provider_id, app_type)?
        .ok_or_else(|| AppError::from(format!("供应商不存在: {provider_id}")))?;
    if !crate::proxy::provider_router::provider_supports_failover(app_type, &provider) {
        return Err(AppError::from(
            "Codex Official 账号卡不支持自动故障转移".to_string(),
        ));
    }
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::{require_failover_app, require_failover_provider};
    use crate::database::Database;
    use crate::provider::{AuthBinding, AuthBindingSource, Provider, ProviderMeta};
    use serde_json::json;

    #[test]
    fn failover_rejects_apps_without_a_proxy_data_plane() {
        assert!(require_failover_app("claude").is_ok());
        assert!(require_failover_app("pi").is_err());
    }

    #[test]
    fn failover_rejects_codex_official_account_cards() {
        let db = Database::memory().expect("memory db");
        let mut official = Provider::with_id(
            "official-a".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        official.category = Some("official".to_string());
        official.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("account-a".to_string()),
            }),
            ..Default::default()
        });
        db.save_provider("codex", &official).expect("save official");

        assert!(require_failover_provider(&db, "codex", &official.id).is_err());
    }
}

/// 获取故障转移队列
#[tauri::command]
pub async fn get_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<Vec<FailoverQueueItem>, AppError> {
    require_failover_app(&app_type)?;
    let queue = state.db.get_failover_queue(&app_type)?;
    if app_type != "codex" {
        return Ok(queue);
    }
    let providers = state.db.get_all_providers(&app_type)?;
    Ok(queue
        .into_iter()
        .filter(|item| {
            providers.get(&item.provider_id).is_some_and(|provider| {
                crate::proxy::provider_router::provider_supports_failover(&app_type, provider)
            })
        })
        .collect())
}

/// 获取可添加到故障转移队列的供应商（不在队列中的）
#[tauri::command]
pub async fn get_available_providers_for_failover(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<Vec<Provider>, AppError> {
    require_failover_app(&app_type)?;
    let providers = state.db.get_available_providers_for_failover(&app_type)?;
    Ok(providers
        .into_iter()
        .filter(|provider| {
            crate::proxy::provider_router::provider_supports_failover(&app_type, provider)
        })
        .collect())
}

/// 添加供应商到故障转移队列
#[tauri::command]
pub async fn add_to_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
    provider_id: String,
) -> Result<(), AppError> {
    require_failover_app(&app_type)?;
    require_failover_provider(&state.db, &app_type, &provider_id)?;
    state.db.add_to_failover_queue(&app_type, &provider_id)
}

/// 从故障转移队列移除供应商
#[tauri::command]
pub async fn remove_from_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
    provider_id: String,
) -> Result<(), AppError> {
    require_failover_app(&app_type)?;
    state.db.remove_from_failover_queue(&app_type, &provider_id)
}

/// 获取指定应用的自动故障转移开关状态（从 proxy_config 表读取）
#[tauri::command]
pub async fn get_auto_failover_enabled(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<bool, AppError> {
    require_failover_app(&app_type)?;
    state
        .db
        .get_proxy_config_for_app(&app_type)
        .await
        .map(|config| config.auto_failover_enabled)
}

/// 设置指定应用的自动故障转移开关状态（写入 proxy_config 表）
///
/// 注意：关闭故障转移时不会清除队列，队列内容会保留供下次开启时使用
#[tauri::command]
pub async fn set_auto_failover_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    app_type: String,
    enabled: bool,
) -> Result<(), AppError> {
    require_failover_app(&app_type)?;
    log::info!(
        "[Failover] Setting auto_failover_enabled: app_type='{app_type}', enabled={enabled}"
    );

    let p1_provider_id = state
        .proxy_service
        .set_auto_failover_enabled(&app_type, enabled)
        .await?;

    if let Some(p1_provider_id) = p1_provider_id {
        // 发射 provider-switched 事件（让前端刷新当前供应商）
        let event_data = serde_json::json!({
            "appType": app_type,
            "providerId": p1_provider_id,
            "source": "failoverEnabled"
        });
        let _ = app.emit("provider-switched", event_data);
    }

    // 刷新托盘菜单，确保状态同步
    if let Ok(new_menu) = crate::tray::create_tray_menu(&app, &state) {
        if let Some(tray) = app.tray_by_id(crate::tray::TRAY_ID) {
            let _ = tray.set_menu(Some(new_menu));
        }
    }

    Ok(())
}
