use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;
use tauri::Runtime;
use tauri_plugin_store::StoreExt;

use crate::error::AppError;

/// Store 中的键名
const STORE_KEY_APP_CONFIG_DIR: &str = "app_config_dir_override";

/// 当前进程启动时的 app_config_dir 覆盖路径。
///
/// 该值一旦初始化就不再变更：设置页写入的新路径只供下次启动使用，
/// 避免当前进程的数据库、OAuth、备份和 Skills 在运行中分裂到不同目录。
static APP_CONFIG_DIR_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 获取缓存中的 app_config_dir 覆盖路径
pub fn get_app_config_dir_override() -> Option<PathBuf> {
    APP_CONFIG_DIR_OVERRIDE.get().cloned().flatten()
}

pub(crate) fn read_app_config_dir_override_from_store<R: Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<Option<PathBuf>, AppError> {
    let store = app
        .store_builder("app_paths.json")
        .build()
        .map_err(|e| AppError::Message(format!("读取 CC Switch 配置目录 Store 失败: {e}")))?;

    match store.get(STORE_KEY_APP_CONFIG_DIR) {
        Some(Value::String(path_str)) => {
            let path_str = path_str.trim();
            if path_str.is_empty() {
                return Ok(None);
            }

            let resolved = resolve_path(path_str);
            if !resolved.is_absolute() {
                return Err(AppError::Message(format!(
                    "Store 中配置的 CC Switch 配置目录必须是绝对路径: {}",
                    resolved.display()
                )));
            }

            let Some(path) = validate_app_config_dir_override(Some(path_str))? else {
                return Ok(None);
            };
            log::info!("使用 Store 中的 app_config_dir: {path:?}");
            Ok(Some(path))
        }
        Some(_) => Err(AppError::Message(format!(
            "CC Switch 配置目录 Store 中的 {STORE_KEY_APP_CONFIG_DIR} 类型不正确，应为字符串"
        ))),
        None => Ok(None),
    }
}

/// 在应用启动时从 Store 初始化进程级 app_config_dir。
///
/// 后续调用只返回首次初始化的值，不会把运行中写入 Store 的待生效值
/// 应用到当前进程。
pub fn initialize_app_config_dir_override<R: Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<Option<PathBuf>, AppError> {
    if let Some(value) = APP_CONFIG_DIR_OVERRIDE.get() {
        return Ok(value.clone());
    }

    let value = read_app_config_dir_override_from_store(app)?;
    let _ = APP_CONFIG_DIR_OVERRIDE.set(value.clone());
    Ok(APP_CONFIG_DIR_OVERRIDE.get().cloned().unwrap_or(value))
}

/// 验证并规范化待持久化的 app_config_dir。
///
/// 返回的路径始终是已存在目录的绝对规范路径；该函数不修改 Store。
pub fn validate_app_config_dir_override(path: Option<&str>) -> Result<Option<PathBuf>, AppError> {
    let Some(raw) = path.map(str::trim).filter(|path| !path.is_empty()) else {
        return Ok(None);
    };

    let resolved = resolve_path(raw);
    if !resolved.exists() {
        return Err(AppError::Message(format!(
            "CC Switch 配置目录不存在: {}",
            resolved.display()
        )));
    }
    if !resolved.is_dir() {
        return Err(AppError::Message(format!(
            "CC Switch 配置目录必须是文件夹: {}",
            resolved.display()
        )));
    }

    resolved.canonicalize().map(Some).map_err(|e| {
        AppError::Message(format!(
            "无法解析 CC Switch 配置目录 {}: {e}",
            resolved.display()
        ))
    })
}

/// 写入 app_config_dir 到 Tauri Store
pub fn set_app_config_dir_to_store<R: Runtime>(
    app: &tauri::AppHandle<R>,
    path: Option<&str>,
) -> Result<(), AppError> {
    let resolved_override = validate_app_config_dir_override(path)?;

    let store = app
        .store_builder("app_paths.json")
        .build()
        .map_err(|e| AppError::Message(format!("创建 Store 失败: {e}")))?;

    match resolved_override {
        Some(resolved) => {
            let resolved = resolved.to_string_lossy().into_owned();
            store.set(STORE_KEY_APP_CONFIG_DIR, Value::String(resolved.clone()));
            log::info!("已将 app_config_dir 写入 Store: {resolved}");
        }
        None => {
            store.delete(STORE_KEY_APP_CONFIG_DIR);
            log::info!("已从 Store 中删除 app_config_dir 配置");
        }
    }

    store
        .save()
        .map_err(|e| AppError::Message(format!("保存 Store 失败: {e}")))?;

    Ok(())
}

/// 解析路径，支持 ~ 开头的相对路径
fn resolve_path(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    } else if let Some(stripped) = raw.strip_prefix("~\\") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }

    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::Path;
    use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};

    fn build_test_app(store_root: &Path) -> tauri::App<MockRuntime> {
        let mut context = mock_context(noop_assets());
        context.config_mut().identifier = store_root.to_string_lossy().into_owned();
        mock_builder()
            .plugin(tauri_plugin_store::Builder::new().build())
            .build(context)
            .expect("build test app")
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir_in("target").expect("create temporary directory under target")
    }

    fn raw_stored_override(app: &tauri::AppHandle<MockRuntime>) -> Option<Value> {
        app.store_builder("app_paths.json")
            .build()
            .expect("open app paths store")
            .get(STORE_KEY_APP_CONFIG_DIR)
    }

    #[test]
    #[serial]
    fn persisted_override_does_not_change_current_process_config_dir() {
        // Mirror an app launched without an override and avoid leaking a custom path into
        // unrelated tests that share this process-level cache.
        let _ = APP_CONFIG_DIR_OVERRIDE.set(None);
        let active_dir = crate::config::get_app_config_dir();

        let store_root = tempdir();
        let pending_dir = tempdir();
        let app = build_test_app(store_root.path());

        set_app_config_dir_to_store(
            app.handle(),
            Some(pending_dir.path().to_string_lossy().as_ref()),
        )
        .expect("persist pending override");

        assert_eq!(crate::config::get_app_config_dir(), active_dir);
        assert_eq!(
            read_app_config_dir_override_from_store(app.handle()).expect("read pending override"),
            Some(pending_dir.path().canonicalize().expect("canonical path"))
        );

        // OnceLock is process-global and cannot be reset to model a real restart in-process.
        // Rebuild the mock app instead to prove the pending Store value survives disk reload.
        drop(app);
        let restarted_app = build_test_app(store_root.path());
        assert_eq!(
            read_app_config_dir_override_from_store(restarted_app.handle())
                .expect("read persisted override"),
            Some(pending_dir.path().canonicalize().expect("canonical path"))
        );
    }

    #[test]
    #[serial]
    fn missing_override_is_rejected_without_polluting_store() {
        let store_root = tempdir();
        let missing_dir = store_root.path().join("missing");
        let app = build_test_app(store_root.path());

        let error =
            set_app_config_dir_to_store(app.handle(), Some(missing_dir.to_string_lossy().as_ref()))
                .expect_err("missing directory must be rejected");

        assert!(error.to_string().contains("配置目录不存在"));
        assert_eq!(raw_stored_override(app.handle()), None);
    }

    #[test]
    #[serial]
    fn regular_file_override_is_rejected_without_polluting_store() {
        let store_root = tempdir();
        let file = tempfile::NamedTempFile::new_in("target").expect("create regular file");
        let app = build_test_app(store_root.path());

        let error =
            set_app_config_dir_to_store(app.handle(), Some(file.path().to_string_lossy().as_ref()))
                .expect_err("regular file must be rejected");

        assert!(error.to_string().contains("必须是文件夹"));
        assert_eq!(raw_stored_override(app.handle()), None);
    }

    #[test]
    #[serial]
    fn tilde_override_is_resolved_before_persisting() {
        let store_root = tempdir();
        let home = dirs::home_dir().expect("test user home directory");
        let app = build_test_app(store_root.path());

        set_app_config_dir_to_store(app.handle(), Some("~"))
            .expect("persist resolved home directory");

        assert_eq!(
            raw_stored_override(app.handle()),
            Some(Value::String(
                home.canonicalize()
                    .expect("canonical home")
                    .to_string_lossy()
                    .into_owned()
            ))
        );
    }

    #[test]
    #[serial]
    fn persisted_missing_override_is_an_error_instead_of_defaulting() {
        let store_root = tempdir();
        let configured_dir = tempdir();
        let configured_path = configured_dir
            .path()
            .canonicalize()
            .expect("canonical path");
        let app = build_test_app(store_root.path());
        let store = app
            .store_builder("app_paths.json")
            .build()
            .expect("open app paths store");
        store.set(
            STORE_KEY_APP_CONFIG_DIR,
            Value::String(configured_path.to_string_lossy().into_owned()),
        );
        store.save().expect("persist configured path");
        drop(configured_dir);

        let error = read_app_config_dir_override_from_store(app.handle())
            .expect_err("unavailable configured directory must fail closed");

        assert!(error.to_string().contains("配置目录不存在"));
        assert!(error
            .to_string()
            .contains(&configured_path.display().to_string()));
    }

    #[test]
    #[serial]
    fn persisted_relative_override_is_rejected_instead_of_using_process_cwd() {
        let store_root = tempdir();
        let app = build_test_app(store_root.path());
        let store = app
            .store_builder("app_paths.json")
            .build()
            .expect("open app paths store");
        store.set(
            STORE_KEY_APP_CONFIG_DIR,
            Value::String("relative/cc-switch".to_string()),
        );
        store.save().expect("persist relative path");

        let error = read_app_config_dir_override_from_store(app.handle())
            .expect_err("persisted relative path must fail closed");

        assert!(error.to_string().contains("必须是绝对路径"));
    }

    #[test]
    #[serial]
    fn validation_is_side_effect_free_and_returns_a_canonical_path() {
        let store_root = tempdir();
        let configured_dir = tempdir();
        let app = build_test_app(store_root.path());

        let validated = validate_app_config_dir_override(Some(
            configured_dir.path().to_string_lossy().as_ref(),
        ))
        .expect("validate directory");

        assert_eq!(
            validated,
            Some(
                configured_dir
                    .path()
                    .canonicalize()
                    .expect("canonical path")
            )
        );
        assert_eq!(raw_stored_override(app.handle()), None);
    }
}
