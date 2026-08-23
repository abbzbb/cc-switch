//! 统一供应商 (Universal Provider) DAO
//!
//! 提供统一供应商的 CRUD 操作。

use crate::database::{lock_conn, to_json_string, Database};
use crate::error::AppError;
use crate::provider::{Provider, UniversalProvider};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;
use std::collections::HashMap;

/// 统一供应商的 Settings Key
const UNIVERSAL_PROVIDERS_KEY: &str = "universal_providers";

impl Database {
    fn load_universal_providers_from_conn(
        conn: &Connection,
    ) -> Result<HashMap<String, UniversalProvider>, AppError> {
        let json: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [UNIVERSAL_PROVIDERS_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| AppError::Database(e.to_string()))?;
        match json {
            Some(json) => serde_json::from_str(&json)
                .map_err(|e| AppError::Database(format!("解析统一供应商数据失败: {e}"))),
            None => Ok(HashMap::new()),
        }
    }

    fn save_universal_providers_on_conn(
        conn: &Connection,
        providers: &HashMap<String, UniversalProvider>,
    ) -> Result<(), AppError> {
        let json = to_json_string(providers)?;
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![UNIVERSAL_PROVIDERS_KEY, json],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// 获取所有统一供应商
    pub fn get_all_universal_providers(
        &self,
    ) -> Result<HashMap<String, UniversalProvider>, AppError> {
        let conn = lock_conn!(self.conn);

        Self::load_universal_providers_from_conn(&conn)
    }

    /// 获取单个统一供应商
    pub fn get_universal_provider(&self, id: &str) -> Result<Option<UniversalProvider>, AppError> {
        let providers = self.get_all_universal_providers()?;
        Ok(providers.get(id).cloned())
    }

    /// 保存统一供应商（添加或更新）
    pub fn save_universal_provider(&self, provider: &UniversalProvider) -> Result<(), AppError> {
        let mut providers = self.get_all_universal_providers()?;
        providers.insert(provider.id.clone(), provider.clone());
        self.save_all_universal_providers(&providers)
    }

    /// 删除统一供应商
    pub fn delete_universal_provider(&self, id: &str) -> Result<bool, AppError> {
        let mut providers = self.get_all_universal_providers()?;
        let existed = providers.remove(id).is_some();
        if existed {
            self.save_all_universal_providers(&providers)?;
        }
        Ok(existed)
    }

    /// 保存所有统一供应商（内部方法）
    fn save_all_universal_providers(
        &self,
        providers: &HashMap<String, UniversalProvider>,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        Self::save_universal_providers_on_conn(&conn, providers)
    }

    fn merge_json(base: &mut Value, patch: &Value) {
        match (base, patch) {
            (Value::Object(base_map), Value::Object(patch_map)) => {
                for (key, patch_value) in patch_map {
                    match base_map.get_mut(key) {
                        Some(base_value) => Self::merge_json(base_value, patch_value),
                        None => {
                            base_map.insert(key.clone(), patch_value.clone());
                        }
                    }
                }
            }
            (base, patch) => *base = patch.clone(),
        }
    }

    fn merge_existing_provider_config(
        tx: &Transaction<'_>,
        app_type: &str,
        provider: &mut Provider,
    ) -> Result<(), AppError> {
        let existing: Option<String> = tx
            .query_row(
                "SELECT settings_config FROM providers WHERE id = ?1 AND app_type = ?2",
                params![provider.id, app_type],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| AppError::Database(e.to_string()))?;
        if let Some(existing) = existing {
            let mut existing: Value = serde_json::from_str(&existing).map_err(|e| {
                AppError::Database(format!("解析 {app_type} 派生供应商配置失败: {e}"))
            })?;
            Self::merge_json(&mut existing, &provider.settings_config);
            provider.settings_config = existing;
        }
        Ok(())
    }

    fn project_universal_provider_in_transaction(
        tx: &Transaction<'_>,
        provider: &UniversalProvider,
    ) -> Result<(), AppError> {
        let projections = [
            ("claude", provider.to_claude_provider()),
            ("codex", provider.to_codex_provider()),
            ("gemini", provider.to_gemini_provider()),
        ];
        for (app_type, projection) in projections {
            if let Some(mut projection) = projection {
                Self::merge_existing_provider_config(tx, app_type, &mut projection)?;
                Self::save_provider_in_transaction(tx, app_type, &projection)?;
            } else {
                tx.execute(
                    "DELETE FROM providers WHERE id = ?1 AND app_type = ?2",
                    params![format!("universal-{app_type}-{}", provider.id), app_type],
                )
                .map_err(|e| AppError::Database(e.to_string()))?;
            }
        }
        Ok(())
    }

    pub fn save_and_sync_universal_provider(
        &self,
        provider: &UniversalProvider,
    ) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut providers = Self::load_universal_providers_from_conn(&tx)?;
        providers.insert(provider.id.clone(), provider.clone());
        Self::save_universal_providers_on_conn(&tx, &providers)?;
        Self::project_universal_provider_in_transaction(&tx, provider)?;
        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn sync_universal_provider(&self, id: &str) -> Result<(), AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;
        let providers = Self::load_universal_providers_from_conn(&tx)?;
        let provider = providers
            .get(id)
            .ok_or_else(|| AppError::Message(format!("统一供应商 {id} 不存在")))?;
        Self::project_universal_provider_in_transaction(&tx, provider)?;
        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn delete_universal_provider_cascade(&self, id: &str) -> Result<bool, AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut providers = Self::load_universal_providers_from_conn(&tx)?;
        let existed = providers.remove(id).is_some();
        if !existed {
            return Ok(false);
        }
        Self::save_universal_providers_on_conn(&tx, &providers)?;
        for app_type in ["claude", "codex", "gemini"] {
            tx.execute(
                "DELETE FROM providers WHERE id = ?1 AND app_type = ?2",
                params![format!("universal-{app_type}-{id}"), app_type],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        }
        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn universal(id: &str) -> UniversalProvider {
        let mut provider = UniversalProvider::new(
            id.to_string(),
            "Atomic Universal".to_string(),
            "custom".to_string(),
            "https://example.com".to_string(),
            "secret".to_string(),
        );
        provider.apps.claude = true;
        provider.apps.codex = true;
        provider.apps.gemini = true;
        provider
    }

    #[test]
    fn save_and_sync_rolls_back_parent_and_prior_projection_on_midway_failure() {
        let db = Database::memory().expect("memory db");
        {
            let conn = db.conn.lock().expect("lock database");
            conn.execute_batch(
                "CREATE TRIGGER fail_universal_codex_insert
                 BEFORE INSERT ON providers
                 WHEN NEW.app_type = 'codex'
                 BEGIN SELECT RAISE(ABORT, 'forced codex projection failure'); END;",
            )
            .expect("install failure trigger");
        }

        let result = db.save_and_sync_universal_provider(&universal("rollback-save"));
        assert!(result.is_err());
        assert!(db
            .get_universal_provider("rollback-save")
            .expect("read parent")
            .is_none());
        assert!(db
            .get_provider_by_id("universal-claude-rollback-save", "claude")
            .expect("read Claude projection")
            .is_none());
    }

    #[test]
    fn cascade_delete_rolls_back_parent_and_prior_deletes_on_midway_failure() {
        let db = Database::memory().expect("memory db");
        db.save_and_sync_universal_provider(&universal("rollback-delete"))
            .expect("seed universal provider");
        {
            let conn = db.conn.lock().expect("lock database");
            conn.execute_batch(
                "CREATE TRIGGER fail_universal_codex_delete
                 BEFORE DELETE ON providers
                 WHEN OLD.app_type = 'codex'
                 BEGIN SELECT RAISE(ABORT, 'forced codex projection delete failure'); END;",
            )
            .expect("install failure trigger");
        }

        let result = db.delete_universal_provider_cascade("rollback-delete");
        assert!(result.is_err());
        assert!(db
            .get_universal_provider("rollback-delete")
            .expect("read parent")
            .is_some());
        assert!(db
            .get_provider_by_id("universal-claude-rollback-delete", "claude")
            .expect("read Claude projection")
            .is_some());
        assert!(db
            .get_provider_by_id("universal-codex-rollback-delete", "codex")
            .expect("read Codex projection")
            .is_some());
    }
}
