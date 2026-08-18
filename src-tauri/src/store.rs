use crate::database::Database;
use crate::proxy::providers::anthropic_oauth_auth::AnthropicOAuthManager;
use crate::proxy::providers::codex_oauth_auth::CodexOAuthManager;
use crate::proxy::providers::kimi_oauth_auth::KimiOAuthManager;
use crate::services::{ProxyService, UsageCache};
use std::sync::Arc;

/// 全局应用状态
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub proxy_service: ProxyService,
    pub usage_cache: Arc<UsageCache>,
    // 内部已使用细粒度锁（accounts/access_tokens/refresh_locks），所有方法均为
    // `&self`，无需外层 RwLock；避免持有粗粒度锁跨网络刷新导致的连锁阻塞。
    pub codex_oauth_manager: Arc<CodexOAuthManager>,
    pub kimi_oauth_manager: Arc<KimiOAuthManager>,
    pub anthropic_oauth_manager: Arc<AnthropicOAuthManager>,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new(db: Arc<Database>) -> Self {
        let app_config_dir = crate::config::get_app_config_dir();
        let codex_oauth_manager = Arc::new(CodexOAuthManager::new(app_config_dir.clone()));
        let kimi_oauth_manager = Arc::new(KimiOAuthManager::new(app_config_dir.clone()));
        let anthropic_oauth_manager = Arc::new(AnthropicOAuthManager::new(app_config_dir));
        let proxy_service = ProxyService::new_with_managed_auth(
            db.clone(),
            codex_oauth_manager.clone(),
            kimi_oauth_manager.clone(),
            anthropic_oauth_manager.clone(),
        );

        Self {
            db,
            proxy_service,
            usage_cache: Arc::new(UsageCache::new()),
            codex_oauth_manager,
            kimi_oauth_manager,
            anthropic_oauth_manager,
        }
    }
}
