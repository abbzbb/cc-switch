//! 供应商路由器模块
//!
//! 负责选择和管理代理目标供应商，实现智能故障转移

use crate::{
    app_config::AppType,
    database::Database,
    error::AppError,
    provider::Provider,
    proxy::{
        circuit_breaker::{AllowResult, CircuitBreaker, CircuitBreakerConfig},
        combo::{
            combo_id_from_request_model, find_combo, order_failover, order_round_robin,
            resolve_combo_targets, ComboRoundRobinState, ComboStrategy, ResolvedComboTarget,
        },
        model_routing::{
            current_live_codex_model, persist_third_party_live_codex_model,
            request_should_follow_current_provider, request_should_follow_session,
            routed_session_pick, SessionModelFollow,
        },
        official_pool::{
            hottest_quota_from_response_headers, hottest_quota_utilization, keep_official_affinity,
            pick_official_pool, pool_card_secret, pool_family_for_unprefixed_model,
            provider_chatgpt_account_id, provider_pool_family, quota_identity,
            subscription_pool_candidates, OfficialPoolState, SubscriptionPoolFamily,
            OFFICIAL_POOL_AUTO_SWITCH_THRESHOLD,
        },
        providers::{
            anthropic_oauth_auth::AnthropicOAuthManager, kimi_oauth_auth::KimiOAuthManager,
        },
    },
};
use std::{
    collections::{HashMap, VecDeque},
    str::FromStr,
    sync::Arc,
    time::Instant,
};
use tokio::sync::RwLock;

/// Providers selected for one request, optionally with per-attempt upstream models.
#[derive(Debug, Clone)]
pub struct ProviderSelection {
    pub providers: Vec<Provider>,
    /// When set (combo routes), rewrite `body.model` to this id for each attempt.
    pub attempt_upstream_models: Option<Vec<String>>,
    /// True only for classic failover. Pin / combo / pool must not change current.
    pub promote_current_on_success: bool,
}

impl ProviderSelection {
    pub fn from_providers(providers: Vec<Provider>) -> Self {
        Self {
            providers,
            attempt_upstream_models: None,
            promote_current_on_success: true,
        }
    }
}

fn circuit_app_type_from_key(key: &str) -> &str {
    let mut best_len = 0usize;
    for app in AppType::all() {
        let prefix = app.as_str();
        if key.starts_with(prefix)
            && key.as_bytes().get(prefix.len()) == Some(&b':')
            && prefix.len() > best_len
        {
            best_len = prefix.len();
        }
    }
    if best_len > 0 {
        &key[..best_len]
    } else {
        key.split(':').next().unwrap_or("claude")
    }
}

fn selection_from_resolved(targets: Vec<ResolvedComboTarget>) -> ProviderSelection {
    let attempt_upstream_models = Some(
        targets
            .iter()
            .map(|target| target.upstream_model.clone())
            .collect(),
    );
    ProviderSelection {
        providers: targets.into_iter().map(|target| target.provider).collect(),
        attempt_upstream_models,
        promote_current_on_success: false,
    }
}

/// Codex Official requests carry the selected account's native Authorization
/// header. Reusing that request against another account card would cross the
/// account boundary, so these cards must never participate in provider retry.
pub(crate) fn provider_supports_failover(app_type: &str, provider: &Provider) -> bool {
    app_type != AppType::Codex.as_str()
        || !crate::proxy::providers::is_codex_official_provider(provider)
}

async fn resolve_subscription_pool_secret(
    provider: &Provider,
    family: SubscriptionPoolFamily,
    kimi: Option<&KimiOAuthManager>,
    anthropic: Option<&AnthropicOAuthManager>,
) -> Option<String> {
    if let Some(secret) = pool_card_secret(provider) {
        return Some(secret);
    }
    match family {
        SubscriptionPoolFamily::CodexOfficial => None,
        SubscriptionPoolFamily::KimiCoding => {
            let account_id = provider
                .meta
                .as_ref()
                .and_then(|meta| meta.managed_account_id_for("kimi_oauth"))?;
            kimi?.get_valid_token_for_account(&account_id).await.ok()
        }
        SubscriptionPoolFamily::AnthropicOauth => {
            let account_id = provider
                .meta
                .as_ref()
                .and_then(|meta| meta.managed_account_id_for("anthropic_oauth"))?;
            anthropic?
                .get_valid_token_for_account(&account_id)
                .await
                .ok()
        }
    }
}

async fn fetch_subscription_pool_quota(
    provider: &Provider,
    kimi: Option<&KimiOAuthManager>,
    anthropic: Option<&AnthropicOAuthManager>,
) -> Option<(String, f64)> {
    let family = provider_pool_family(provider)?;
    if matches!(family, SubscriptionPoolFamily::CodexOfficial) {
        return None;
    }
    let identity = quota_identity(provider)?;
    let secret = resolve_subscription_pool_secret(provider, family, kimi, anthropic).await?;
    let quota = match family {
        SubscriptionPoolFamily::CodexOfficial => return None,
        SubscriptionPoolFamily::AnthropicOauth => {
            crate::services::subscription::query_claude_quota(&secret)
                .await
                .ok()?
        }
        SubscriptionPoolFamily::KimiCoding => crate::services::coding_plan::get_coding_plan_quota(
            crate::proxy::providers::KIMI_CODING_API_BASE_URL,
            &secret,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .ok()?,
    };
    hottest_quota_utilization(&quota).map(|score| (identity, score))
}

/// 供应商路由器
pub struct ProviderRouter {
    /// 数据库连接
    db: Arc<Database>,
    /// 熔断器管理器 - key 格式: "app_type:provider_id"
    circuit_breakers: Arc<RwLock<HashMap<String, Arc<CircuitBreaker>>>>,
    /// Smooth weighted round-robin counters keyed by combo id.
    combo_rr: Arc<RwLock<HashMap<String, ComboRoundRobinState>>>,
    /// ChatGPT Official account-pool affinity + cooldown (process-local).
    official_pool: Arc<RwLock<OfficialPoolState>>,
    /// Codex session → last third-party routed pick (Grok, …).
    session_follow: Arc<RwLock<HashMap<String, SessionModelFollow>>>,
    session_follow_order: Arc<RwLock<VecDeque<String>>>,
}

impl ProviderRouter {
    /// 创建新的供应商路由器
    #[cfg(test)]
    pub fn new(db: Arc<Database>) -> Self {
        Self::with_official_pool(db, Arc::new(RwLock::new(OfficialPoolState::default())))
    }

    pub fn with_official_pool(
        db: Arc<Database>,
        official_pool: Arc<RwLock<OfficialPoolState>>,
    ) -> Self {
        Self {
            db,
            circuit_breakers: Arc::new(RwLock::new(HashMap::new())),
            combo_rr: Arc::new(RwLock::new(HashMap::new())),
            official_pool,
            session_follow: Arc::new(RwLock::new(HashMap::new())),
            session_follow_order: Arc::new(RwLock::new(VecDeque::new())),
        }
    }

    pub fn inbound_capability_tokens(&self) -> Vec<String> {
        crate::proxy::inbound_auth::inbound_capability_tokens(&self.db)
    }

    /// 选择可用的供应商（支持故障转移）
    ///
    /// 返回按优先级排序的可用供应商列表：
    /// - 故障转移关闭时：仅返回当前供应商
    /// - 故障转移开启时：仅使用故障转移队列，按队列顺序依次尝试（P1 → P2 → ...）
    pub async fn select_providers(&self, app_type: &str) -> Result<Vec<Provider>, AppError> {
        let mut result = Vec::new();
        let mut total_providers = 0usize;
        let mut circuit_open_count = 0usize;
        let current_id = AppType::from_str(app_type)
            .ok()
            .and_then(|app_enum| {
                crate::settings::get_effective_current_provider(&self.db, &app_enum)
                    .ok()
                    .flatten()
            })
            .or_else(|| self.db.get_current_provider(app_type).ok().flatten());
        let current_provider = current_id
            .as_deref()
            .map(|id| self.db.get_provider_by_id(id, app_type))
            .transpose()?
            .flatten();

        // 检查该应用的自动故障转移开关是否开启（从 proxy_config 表读取）
        let auto_failover_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.auto_failover_enabled,
            Err(e) => {
                log::error!("[{app_type}] 读取 proxy_config 失败: {e}，默认禁用故障转移");
                false
            }
        };

        if auto_failover_enabled
            && current_provider
                .as_ref()
                .is_some_and(|provider| !provider_supports_failover(app_type, provider))
        {
            // A selected Codex Official account is an explicit account choice.
            // Keep it as a single route even if an old failover setting remains
            // enabled; retrying would reuse its inbound token for another card.
            total_providers = 1;
            result.push(current_provider.expect("checked above"));
        } else if auto_failover_enabled {
            // 故障转移开启：仅按队列顺序依次尝试（P1 → P2 → ...）
            let all_providers = self.db.get_all_providers(app_type)?;

            // 使用 DAO 返回的排序结果，确保和前端展示一致
            let ordered_ids: Vec<String> = self
                .db
                .get_failover_queue(app_type)?
                .into_iter()
                .map(|item| item.provider_id)
                .collect();

            for provider_id in ordered_ids {
                let Some(provider) = all_providers.get(&provider_id).cloned() else {
                    continue;
                };
                if !provider_supports_failover(app_type, &provider) {
                    continue;
                }
                total_providers += 1;

                let circuit_key = format!("{app_type}:{}", provider.id);
                let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

                if breaker.is_available().await {
                    result.push(provider);
                } else {
                    circuit_open_count += 1;
                }
            }
        } else {
            // 故障转移关闭：仅使用当前供应商，跳过熔断器检查
            if let Some(current) = current_provider {
                total_providers = 1;
                result.push(current);
            }
        }

        if result.is_empty() {
            if total_providers > 0 && circuit_open_count == total_providers {
                log::warn!("[{app_type}] [FO-004] 所有供应商均已熔断");
                return Err(AppError::AllProvidersCircuitOpen);
            } else {
                log::warn!("[{app_type}] [FO-005] 未配置供应商");
                return Err(AppError::NoProvidersConfigured);
            }
        }

        Ok(result)
    }

    /// Select providers for a request, honouring `combo/{id}` then `provider/model`.
    ///
    /// A matching routing slug pins that card (no failover). An unprefixed model
    /// that uniquely belongs to one card is also pinned. Two or more subscription
    /// cards of the same family advertising the same native id form an account
    /// pool. Otherwise this falls back to [`Self::select_providers`].
    #[cfg(test)]
    pub async fn select_providers_for_request(
        &self,
        app_type: &str,
        request_model: &str,
    ) -> Result<ProviderSelection, AppError> {
        self.select_providers_for_request_with_session(app_type, request_model, None)
            .await
    }

    pub async fn select_providers_for_request_with_session(
        &self,
        app_type: &str,
        request_model: &str,
        session_id: Option<&str>,
    ) -> Result<ProviderSelection, AppError> {
        let all_providers: Vec<Provider> =
            self.db.get_all_providers(app_type)?.into_values().collect();
        let combos = self.db.list_model_combos()?;
        if let Some(combo_id) = combo_id_from_request_model(request_model, !combos.is_empty()) {
            return self
                .select_combo_targets(&all_providers, &combos, combo_id)
                .await;
        }

        let followed = if app_type == AppType::Codex.as_str() {
            self.apply_codex_session_follow(&all_providers, request_model, session_id)
                .await
        } else {
            None
        };
        let effective_model = followed.as_deref().unwrap_or(request_model);

        match crate::proxy::model_routing::decide_model_route(&all_providers, effective_model) {
            crate::proxy::model_routing::ModelRouteDecision::Pinned {
                provider_id,
                upstream_model,
            } => {
                let provider = all_providers
                    .into_iter()
                    .find(|provider| provider.id == provider_id)
                    .ok_or_else(|| {
                        AppError::InvalidInput(format!("unknown routed provider '{request_model}'"))
                    })?;
                Ok(ProviderSelection {
                    providers: vec![provider],
                    attempt_upstream_models: Some(vec![upstream_model]),
                    promote_current_on_success: false,
                })
            }
            crate::proxy::model_routing::ModelRouteDecision::Default => {
                if let Some(pooled) = self
                    .try_official_pool(app_type, request_model, &all_providers, session_id)
                    .await
                {
                    return Ok(ProviderSelection {
                        providers: vec![pooled],
                        attempt_upstream_models: None,
                        promote_current_on_success: false,
                    });
                }
                self.select_providers(app_type)
                    .await
                    .map(ProviderSelection::from_providers)
            }
        }
    }

    async fn apply_codex_session_follow(
        &self,
        providers: &[Provider],
        request_model: &str,
        session_id: Option<&str>,
    ) -> Option<String> {
        if let Some(session_id) = session_id {
            let follow = self.session_follow.read().await.get(session_id).cloned();
            if let Some(follow) = follow {
                if request_should_follow_session(providers, request_model, &follow) {
                    return Some(follow.model);
                }
            }
        }
        if let Some(pick) = routed_session_pick(providers, request_model) {
            let displaced = current_live_codex_model().filter(|value| value != &pick);
            persist_third_party_live_codex_model(&pick);
            if let Some(session_id) = session_id {
                self.remember_codex_session_follow(session_id, pick, displaced)
                    .await;
            }
        }
        if let Some(current_id) =
            crate::settings::get_effective_current_provider(&self.db, &AppType::Codex)
                .ok()
                .flatten()
        {
            if let Some(follow) =
                request_should_follow_current_provider(providers, request_model, &current_id)
            {
                return Some(follow);
            }
        }
        None
    }

    async fn remember_codex_session_follow(
        &self,
        session_id: &str,
        model: String,
        displaced_default: Option<String>,
    ) {
        const SESSION_FOLLOW_CAP: usize = 256;
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        let mut map = self.session_follow.write().await;
        let mut order = self.session_follow_order.write().await;
        let follow = SessionModelFollow {
            model,
            displaced_default,
        };
        if map.insert(session_id.to_string(), follow).is_none() {
            order.push_back(session_id.to_string());
            while order.len() > SESSION_FOLLOW_CAP {
                if let Some(oldest) = order.pop_front() {
                    map.remove(&oldest);
                }
            }
        }
    }

    async fn try_official_pool(
        &self,
        app_type: &str,
        request_model: &str,
        providers: &[Provider],
        session_id: Option<&str>,
    ) -> Option<Provider> {
        let family = pool_family_for_unprefixed_model(request_model)?;
        let candidates = subscription_pool_candidates(providers, request_model, family);
        if candidates.len() < 2 {
            return None;
        }

        let current_id = AppType::from_str(app_type)
            .ok()
            .and_then(|app_enum| {
                crate::settings::get_effective_current_provider(&self.db, &app_enum)
                    .ok()
                    .flatten()
            })
            .or_else(|| self.db.get_current_provider(app_type).ok().flatten());
        let now = Instant::now();
        let state = self.official_pool.read().await;
        let usage = state.quota_map().clone();

        let eligible: Vec<&Provider> = candidates
            .iter()
            .copied()
            .filter(|provider| !state.is_cooling(&provider.id, now))
            .collect();
        let affinity_pool = if eligible.is_empty() {
            candidates.as_slice()
        } else {
            eligible.as_slice()
        };

        if let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
            if let Some(bound_id) = state.bound_provider(session_id) {
                if !state.is_cooling(bound_id, now) {
                    if let Some(bound) = affinity_pool
                        .iter()
                        .find(|provider| provider.id == bound_id)
                    {
                        if keep_official_affinity(
                            bound,
                            affinity_pool,
                            &usage,
                            OFFICIAL_POOL_AUTO_SWITCH_THRESHOLD,
                        ) {
                            return Some((*bound).clone());
                        }
                    }
                }
            }
        }

        pick_official_pool(
            current_id.as_deref(),
            &candidates,
            state.cooldown_map(),
            now,
            &usage,
        )
    }

    pub async fn bind_official_affinity(&self, session_id: &str, provider_id: &str) {
        self.official_pool
            .write()
            .await
            .bind(session_id, provider_id);
    }

    pub async fn clear_official_affinity(&self, session_id: &str) {
        self.official_pool.write().await.clear_session(session_id);
    }

    pub async fn note_official_cooldown(&self, provider_id: &str) {
        self.official_pool
            .write()
            .await
            .note_cooldown(provider_id, Instant::now());
    }

    pub async fn note_official_quota(&self, account_id: &str, utilization: f64) {
        self.official_pool
            .write()
            .await
            .note_quota(account_id, utilization);
    }

    pub async fn note_official_quota_from_headers(
        &self,
        provider: &Provider,
        headers: &http::HeaderMap,
    ) {
        let Some(account_id) = provider_chatgpt_account_id(provider) else {
            return;
        };
        if let Some(utilization) = hottest_quota_from_response_headers(headers) {
            self.note_official_quota(&account_id, utilization).await;
        }
    }

    pub async fn schedule_official_quota_refresh(
        &self,
        account_id: String,
        manager: Arc<crate::proxy::providers::codex_oauth_auth::CodexOAuthManager>,
    ) {
        #[cfg(test)]
        {
            let _ = (account_id, manager);
            return;
        }
        #[cfg(not(test))]
        {
            let mut state = self.official_pool.write().await;
            if !state.try_begin_wham_poll(&account_id, Instant::now()) {
                return;
            }
            drop(state);
            let pool = self.official_pool.clone();
            tokio::spawn(async move {
                let token = match manager.get_valid_token_for_account(&account_id).await {
                    Ok(token) => token,
                    Err(_) => return,
                };
                if let Ok(quota) = crate::services::subscription::query_codex_quota(
                    &token,
                    Some(&account_id),
                    "codex_oauth",
                    "Codex OAuth access token expired or rejected. Please re-login via cc-switch.",
                )
                .await
                {
                    if let Some(score) = hottest_quota_utilization(&quota) {
                        pool.write().await.note_quota(&account_id, score);
                    }
                }
            });
        }
    }

    pub async fn schedule_subscription_pool_quota_refresh(
        &self,
        provider: &Provider,
        kimi: Option<Arc<KimiOAuthManager>>,
        anthropic: Option<Arc<AnthropicOAuthManager>>,
    ) {
        #[cfg(test)]
        {
            let _ = (provider, kimi, anthropic);
            return;
        }
        #[cfg(not(test))]
        {
            let Some(identity) = quota_identity(provider) else {
                return;
            };
            let mut state = self.official_pool.write().await;
            if !state.try_begin_wham_poll(&identity, Instant::now()) {
                return;
            }
            drop(state);
            let provider = provider.clone();
            let pool = self.official_pool.clone();
            tokio::spawn(async move {
                if let Some((account_id, score)) =
                    fetch_subscription_pool_quota(&provider, kimi.as_deref(), anthropic.as_deref())
                        .await
                {
                    pool.write().await.note_quota(&account_id, score);
                }
            });
        }
    }

    /// Resolve and query pool quota immediately (no spawn, no WHAM throttle).
    #[cfg(test)]
    pub(crate) async fn refresh_subscription_pool_quota_now(
        &self,
        provider: &Provider,
        kimi: Option<&KimiOAuthManager>,
        anthropic: Option<&AnthropicOAuthManager>,
    ) -> bool {
        let Some((identity, score)) =
            fetch_subscription_pool_quota(provider, kimi, anthropic).await
        else {
            return false;
        };
        self.note_official_quota(&identity, score).await;
        true
    }

    #[cfg(test)]
    pub(crate) async fn quota_for_test(&self, account_id: &str) -> Option<f64> {
        self.official_pool
            .read()
            .await
            .quota_map()
            .get(account_id)
            .copied()
    }

    async fn select_combo_targets(
        &self,
        providers: &[Provider],
        combos: &[crate::proxy::combo::ModelCombo],
        combo_id: &str,
    ) -> Result<ProviderSelection, AppError> {
        let combo = find_combo(combos, combo_id)
            .ok_or_else(|| AppError::InvalidInput(format!("unknown combo '{combo_id}'")))?;
        let resolved = resolve_combo_targets(combo, providers);
        if resolved.is_empty() {
            return Err(AppError::NoProvidersConfigured);
        }
        let ordered = match combo.strategy {
            ComboStrategy::Failover => order_failover(resolved),
            ComboStrategy::RoundRobin => {
                let mut state = self.combo_rr.write().await;
                order_round_robin(&combo.id, resolved, combo.sticky_limit, &mut state)
            }
        };
        Ok(selection_from_resolved(ordered))
    }

    /// 请求执行前获取熔断器“放行许可”
    ///
    /// - Closed：直接放行
    /// - Open：超时到达后切到 HalfOpen 并放行一次探测
    /// - HalfOpen：按限流规则放行探测
    ///
    /// 注意：调用方必须在请求结束后通过 `record_result()` 释放 HalfOpen 名额，
    /// 否则会导致该 Provider 长时间无法进入探测状态。
    pub async fn allow_provider_request(&self, provider_id: &str, app_type: &str) -> AllowResult {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        breaker.allow_request().await
    }

    /// 记录供应商请求结果
    pub async fn record_result(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
        success: bool,
        error_msg: Option<String>,
    ) -> Result<(), AppError> {
        // 1. 按应用独立获取熔断器配置
        let failure_threshold = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => app_config.circuit_failure_threshold,
            Err(_) => 5, // 默认值
        };

        // 2. 更新熔断器状态
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

        if success {
            breaker.record_success(used_half_open_permit).await;
        } else {
            breaker.record_failure(used_half_open_permit).await;
        }

        // 3. 更新数据库健康状态（使用配置的阈值）
        self.db
            .update_provider_health_with_threshold(
                provider_id,
                app_type,
                success,
                error_msg.clone(),
                failure_threshold,
            )
            .await?;

        Ok(())
    }

    /// 重置熔断器（手动恢复）
    pub async fn reset_circuit_breaker(&self, circuit_key: &str) {
        let breakers = self.circuit_breakers.read().await;
        if let Some(breaker) = breakers.get(circuit_key) {
            breaker.reset().await;
        }
    }

    /// 重置指定供应商的熔断器
    pub async fn reset_provider_breaker(&self, provider_id: &str, app_type: &str) {
        let circuit_key = format!("{app_type}:{provider_id}");
        self.reset_circuit_breaker(&circuit_key).await;
    }

    pub async fn forget_provider(&self, app_type: &str, provider_id: &str) {
        let circuit_key = format!("{app_type}:{provider_id}");
        self.circuit_breakers.write().await.remove(&circuit_key);
        let mut pool = self.official_pool.write().await;
        pool.forget_provider(provider_id);
    }

    /// 整流器等路径：请求结果不计入健康度。
    ///
    /// HalfOpen 名额由 `AllowResult` Drop 释放。这里不再减计数，避免与
    /// 仍存活的 permit 双释放、偷走后续探测。
    #[allow(clippy::unused_async)]
    pub async fn release_permit_neutral(
        &self,
        _provider_id: &str,
        _app_type: &str,
        _used_half_open_permit: bool,
    ) {
    }

    /// 更新所有熔断器的配置（热更新）
    pub async fn update_all_configs(&self, config: CircuitBreakerConfig) {
        let breakers = self.circuit_breakers.read().await;
        for breaker in breakers.values() {
            breaker.update_config(config.clone()).await;
        }
    }

    /// 更新指定应用已创建熔断器的配置（热更新）
    pub async fn update_app_configs(&self, app_type: &str, config: CircuitBreakerConfig) {
        let prefix = format!("{app_type}:");
        let breakers = self.circuit_breakers.read().await;
        for (key, breaker) in breakers.iter() {
            if key.starts_with(&prefix) {
                breaker.update_config(config.clone()).await;
            }
        }
    }

    /// 获取熔断器状态
    #[allow(dead_code)]
    pub async fn get_circuit_breaker_stats(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<crate::proxy::circuit_breaker::CircuitBreakerStats> {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breakers = self.circuit_breakers.read().await;

        if let Some(breaker) = breakers.get(&circuit_key) {
            Some(breaker.get_stats().await)
        } else {
            None
        }
    }

    /// 获取或创建熔断器
    async fn get_or_create_circuit_breaker(&self, key: &str) -> Arc<CircuitBreaker> {
        // 先尝试读锁获取
        {
            let breakers = self.circuit_breakers.read().await;
            if let Some(breaker) = breakers.get(key) {
                return breaker.clone();
            }
        }

        // 如果不存在，获取写锁创建
        let mut breakers = self.circuit_breakers.write().await;

        // 双重检查，防止竞争条件
        if let Some(breaker) = breakers.get(key) {
            return breaker.clone();
        }

        // 从 key 中提取 app_type (格式: "app_type:provider_id")
        let app_type = circuit_app_type_from_key(key);

        // 按应用独立读取熔断器配置
        let config = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => crate::proxy::circuit_breaker::CircuitBreakerConfig {
                failure_threshold: app_config.circuit_failure_threshold,
                success_threshold: app_config.circuit_success_threshold,
                timeout_seconds: app_config.circuit_timeout_seconds as u64,
                error_rate_threshold: app_config.circuit_error_rate_threshold,
                min_requests: app_config.circuit_min_requests,
            },
            Err(_) => crate::proxy::circuit_breaker::CircuitBreakerConfig::default(),
        };

        let breaker = Arc::new(CircuitBreaker::new(config));
        breakers.insert(key.to_string(), breaker.clone());

        breaker
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        database::Database,
        provider::{AuthBinding, AuthBindingSource, ProviderMeta},
    };
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    fn managed_codex_official(id: &str, account_id: &str) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        provider.category = Some("official".to_string());
        provider.meta = Some(ProviderMeta {
            provider_type: Some("codex_oauth".to_string()),
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some(account_id.to_string()),
            }),
            ..Default::default()
        });
        provider
    }

    fn anthropic_oauth_card(id: &str, token: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            format!("Claude {id}"),
            json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": token,
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                    "ANTHROPIC_MODEL": "claude-sonnet-4-6"
                }
            }),
            None,
        )
    }

    fn kimi_coding_card(id: &str, key: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            format!("Kimi {id}"),
            json!({
                "auth": { "OPENAI_API_KEY": key },
                "config": format!(
                    "model = \"kimi-for-coding\"\n\n[model_providers.{id}]\nbase_url = \"https://api.kimi.com/coding/v1\"\n"
                ),
                "modelCatalog": { "models": [{ "model": "kimi-for-coding" }] }
            }),
            None,
        )
    }

    struct TempHome {
        #[allow(dead_code)]
        dir: TempDir,
        original_home: Option<String>,
        original_userprofile: Option<String>,
        original_test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("failed to create temp home");
            let original_home = env::var("HOME").ok();
            let original_userprofile = env::var("USERPROFILE").ok();
            let original_test_home = env::var("CC_SWITCH_TEST_HOME").ok();

            env::set_var("HOME", dir.path());
            env::set_var("USERPROFILE", dir.path());
            env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload settings");

            Self {
                dir,
                original_home,
                original_userprofile,
                original_test_home,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.original_home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }

            match &self.original_userprofile {
                Some(value) => env::set_var("USERPROFILE", value),
                None => env::remove_var("USERPROFILE"),
            }

            match &self.original_test_home {
                Some(value) => env::set_var("CC_SWITCH_TEST_HOME", value),
                None => env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_provider_router_creation() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);

        let breaker = router.get_or_create_circuit_breaker("claude:test").await;
        assert!(breaker.allow_request().await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_disabled_uses_current_provider() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_order_ignoring_current() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 设置 sort_index 来控制顺序：b=1, a=2
        let mut provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        provider_a.sort_index = Some(2);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        db.add_to_failover_queue("claude", "b").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 2);
        // 故障转移开启时：仅按队列顺序选择（忽略当前供应商）
        assert_eq!(providers[0].id, "b");
        assert_eq!(providers[1].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_only_even_if_current_not_in_queue() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        // 只把 b 加入故障转移队列（模拟“当前供应商不在队列里”的常见配置）
        db.add_to_failover_queue("claude", "b").unwrap();

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "b");
    }

    #[tokio::test]
    #[serial]
    async fn codex_official_current_stays_single_route_when_failover_is_stale() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let official = managed_codex_official("official-a", "account-a");
        let fallback = Provider::with_id(
            "fallback".to_string(),
            "Fallback".to_string(),
            json!({}),
            None,
        );
        db.save_provider("codex", &official).unwrap();
        db.save_provider("codex", &fallback).unwrap();
        db.set_current_provider("codex", &official.id).unwrap();
        db.add_to_failover_queue("codex", &fallback.id).unwrap();

        let mut config = db.get_proxy_config_for_app("codex").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let providers = ProviderRouter::new(db)
            .select_providers("codex")
            .await
            .unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, official.id);
    }

    #[tokio::test]
    #[serial]
    async fn stale_codex_official_queue_entries_are_not_retry_targets() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let current = Provider::with_id(
            "third-party".to_string(),
            "Third Party".to_string(),
            json!({}),
            None,
        );
        let official = managed_codex_official("official-a", "account-a");
        let fallback = Provider::with_id(
            "fallback".to_string(),
            "Fallback".to_string(),
            json!({}),
            None,
        );
        db.save_provider("codex", &current).unwrap();
        db.save_provider("codex", &official).unwrap();
        db.save_provider("codex", &fallback).unwrap();
        db.set_current_provider("codex", &current.id).unwrap();
        db.add_to_failover_queue("codex", &official.id).unwrap();
        db.add_to_failover_queue("codex", &fallback.id).unwrap();

        let mut config = db.get_proxy_config_for_app("codex").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let providers = ProviderRouter::new(db)
            .select_providers("codex")
            .await
            .unwrap();
        assert_eq!(
            providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fallback"]
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_select_providers_does_not_consume_half_open_permit() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();

        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        router
            .record_result("b", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        let providers = router.select_providers("claude").await.unwrap();
        assert_eq!(providers.len(), 2);

        assert!(router.allow_provider_request("b", "claude").await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_release_permit_neutral_frees_half_open_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 配置熔断器：1 次失败即熔断，0 秒超时立即进入 HalfOpen
        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        db.save_provider("claude", &provider_a).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        // 触发熔断：1 次失败
        router
            .record_result("a", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        // 第一次请求：获取 HalfOpen 探测名额
        let first = router.allow_provider_request("a", "claude").await;
        assert!(first.allowed);
        assert!(first.used_half_open_permit);

        // 第二次请求应被拒绝（名额已被占用）
        let second = router.allow_provider_request("a", "claude").await;
        assert!(!second.allowed);

        // 名额由 AllowResult Drop 释放；与仍存活的 first 叠用
        // release_permit_neutral 不得再减计数。
        drop(first);

        // 第三次请求应被允许（名额已释放）
        let third = router.allow_provider_request("a", "claude").await;
        assert!(third.allowed);
        assert!(third.used_half_open_permit);
    }

    fn catalog_provider(id: &str, name: &str, model: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            name.to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "sk-test" },
                "config": format!(
                    "model_provider = \"{id}\"\nmodel = \"{model}\"\n\n[model_providers.{id}]\nbase_url = \"https://example.com/v1\"\nwire_api = \"chat\"\n"
                ),
                "modelCatalog": { "models": [{ "model": model }] }
            }),
            None,
        )
    }

    #[tokio::test]
    #[serial]
    async fn request_prefix_pins_card_and_skips_failover() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let kimi = catalog_provider("kimi", "Kimi", "k2");
        let deepseek = catalog_provider("deepseek", "DeepSeek", "deepseek-v4");
        db.save_provider("codex", &kimi).unwrap();
        db.save_provider("codex", &deepseek).unwrap();
        db.set_current_provider("codex", "kimi").unwrap();
        db.add_to_failover_queue("codex", "kimi").unwrap();
        db.add_to_failover_queue("codex", "deepseek").unwrap();
        let mut config = db.get_proxy_config_for_app("codex").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let pinned = router
            .select_providers_for_request("codex", "deepseek/deepseek-v4")
            .await
            .unwrap();
        assert_eq!(pinned.providers.len(), 1);
        assert_eq!(pinned.providers[0].id, "deepseek");
        assert!(!pinned.promote_current_on_success);

        let unique = router
            .select_providers_for_request("codex", "k2")
            .await
            .unwrap();
        assert_eq!(unique.providers.len(), 1);
        assert_eq!(unique.providers[0].id, "kimi");

        let unknown = router
            .select_providers_for_request("codex", "anthropic/claude-sonnet-5")
            .await
            .unwrap();
        assert!(unknown
            .providers
            .iter()
            .any(|provider| provider.id == "kimi"));
        assert!(!unknown.providers.is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn official_prefix_does_not_failover() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let official = managed_codex_official("codex-official", "acct-1");
        let kimi = catalog_provider("kimi", "Kimi", "k2");
        db.save_provider("codex", &official).unwrap();
        db.save_provider("codex", &kimi).unwrap();
        db.set_current_provider("codex", "kimi").unwrap();
        db.add_to_failover_queue("codex", "kimi").unwrap();
        db.add_to_failover_queue("codex", "codex-official").unwrap();
        let mut config = db.get_proxy_config_for_app("codex").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db);
        let pinned = router
            .select_providers_for_request("codex", "codex-official/gpt-5.5")
            .await
            .unwrap();
        assert_eq!(pinned.providers.len(), 1);
        assert_eq!(pinned.providers[0].id, "codex-official");
    }

    #[tokio::test]
    #[serial]
    async fn grok_session_follow_rewrites_official_default() {
        let _home = TempHome::new();
        let config_dir = crate::codex_config::get_codex_config_dir();
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "model = \"default/gpt-5.6-sol\"\nmodel_provider = \"cm\"\n",
        )
        .unwrap();

        let db = Arc::new(Database::memory().unwrap());
        db.save_provider("codex", &managed_codex_official("codex-official", "acct-1"))
            .unwrap();
        db.save_provider("codex", &catalog_provider("grok", "Grok", "grok-4.6"))
            .unwrap();
        db.set_current_provider("codex", "codex-official").unwrap();

        let router = ProviderRouter::new(db);
        let grok = router
            .select_providers_for_request_with_session("codex", "grok/grok-4.6", Some("sess-1"))
            .await
            .unwrap();
        assert_eq!(grok.providers[0].id, "grok");
        assert_eq!(
            grok.attempt_upstream_models.as_deref(),
            Some(["grok-4.6".to_string()].as_slice())
        );

        let followed = router
            .select_providers_for_request_with_session(
                "codex",
                "default/gpt-5.6-sol",
                Some("sess-1"),
            )
            .await
            .unwrap();
        assert_eq!(followed.providers[0].id, "grok");
        assert_eq!(
            followed.attempt_upstream_models.as_deref(),
            Some(["grok-4.6".to_string()].as_slice())
        );

        let other_session = router
            .select_providers_for_request_with_session(
                "codex",
                "default/gpt-5.6-sol",
                Some("sess-2"),
            )
            .await
            .unwrap();
        assert_eq!(other_session.providers[0].id, "codex-official");

        let persisted = std::fs::read_to_string(config_dir.join("config.toml")).unwrap();
        assert!(
            persisted.contains("model = \"grok/grok-4.6\""),
            "{persisted}"
        );

        let followed_codes = router
            .select_providers_for_request_with_session("codex", "codes/gpt-5.4", Some("sess-1"))
            .await
            .unwrap();
        assert_eq!(followed_codes.providers[0].id, "grok");
        assert_eq!(
            followed_codes.attempt_upstream_models.as_deref(),
            Some(["grok-4.6".to_string()].as_slice())
        );
    }

    #[tokio::test]
    #[serial]
    async fn session_follow_does_not_steal_known_third_party_gpt_pin() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        db.save_provider("codex", &managed_codex_official("codex-official", "acct-1"))
            .unwrap();
        db.save_provider("codex", &catalog_provider("grok", "Grok", "grok-4.6"))
            .unwrap();
        db.save_provider("codex", &catalog_provider("codes", "Niuma", "gpt-5.4"))
            .unwrap();
        db.set_current_provider("codex", "codex-official").unwrap();

        let router = ProviderRouter::new(db);
        router
            .select_providers_for_request_with_session("codex", "grok/grok-4.6", Some("sess-1"))
            .await
            .unwrap();
        let pinned = router
            .select_providers_for_request_with_session("codex", "codes/gpt-5.4", Some("sess-1"))
            .await
            .unwrap();
        assert_eq!(pinned.providers[0].id, "codes");
        assert_eq!(
            pinned.attempt_upstream_models.as_deref(),
            Some(["gpt-5.4".to_string()].as_slice())
        );
    }

    #[tokio::test]
    #[serial]
    async fn current_grok_rewrites_guardian_leftover_default_in_other_session() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::set_current_provider(&AppType::Codex, Some("grok"))
            .expect("set current grok");

        let db = Arc::new(Database::memory().unwrap());
        let mut official = managed_codex_official("codex-official", "acct-1");
        official.settings_config["config"] = json!("model = \"gpt-5.6-sol\"\n");
        db.save_provider("codex", &official).unwrap();
        db.save_provider("codex", &catalog_provider("grok", "Grok", "grok-4.6"))
            .unwrap();
        db.set_current_provider("codex", "grok").unwrap();

        let router = ProviderRouter::new(db);
        let guardian = router
            .select_providers_for_request_with_session(
                "codex",
                "default/gpt-5.6-sol",
                Some("guardian-sess"),
            )
            .await
            .unwrap();
        assert_eq!(guardian.providers[0].id, "grok");
        assert_eq!(
            guardian.attempt_upstream_models.as_deref(),
            Some(["grok-4.6".to_string()].as_slice())
        );

        let explicit = router
            .select_providers_for_request_with_session(
                "codex",
                "codex-official/gpt-5.5",
                Some("other-thread"),
            )
            .await
            .unwrap();
        assert_eq!(explicit.providers[0].id, "codex-official");
    }

    #[tokio::test]
    #[serial]
    async fn official_pool_picks_current_then_affinity_then_skips_cooldown() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let mut main = managed_codex_official("main", "account-a");
        main.sort_index = Some(0);
        let mut backup = managed_codex_official("backup", "account-b");
        backup.sort_index = Some(1);
        db.save_provider("codex", &main).unwrap();
        db.save_provider("codex", &backup).unwrap();
        db.set_current_provider("codex", "main").unwrap();

        let router = ProviderRouter::new(db);
        let selected = router
            .select_providers_for_request("codex", "gpt-5.5")
            .await
            .unwrap();
        assert_eq!(selected.providers.len(), 1);
        assert_eq!(selected.providers[0].id, "main");

        router
            .bind_official_affinity("codex_thread-1", "backup")
            .await;
        let sticky = router
            .select_providers_for_request_with_session("codex", "gpt-5.5", Some("codex_thread-1"))
            .await
            .unwrap();
        assert_eq!(sticky.providers[0].id, "backup");

        router.note_official_cooldown("backup").await;
        let after_cooldown = router
            .select_providers_for_request_with_session("codex", "gpt-5.5", Some("codex_thread-1"))
            .await
            .unwrap();
        assert_eq!(after_cooldown.providers[0].id, "main");

        router.note_official_cooldown("backup").await;
        let pinned_while_cooling = router
            .select_providers_for_request("codex", "backup/gpt-5.5")
            .await
            .unwrap();
        assert_eq!(pinned_while_cooling.providers[0].id, "backup");
    }

    #[tokio::test]
    #[serial]
    async fn official_pool_picks_lowest_quota_and_auto_switches_hot_thread() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let mut main = managed_codex_official("main", "account-a");
        main.sort_index = Some(0);
        let mut backup = managed_codex_official("backup", "account-b");
        backup.sort_index = Some(1);
        db.save_provider("codex", &main).unwrap();
        db.save_provider("codex", &backup).unwrap();
        db.set_current_provider("codex", "main").unwrap();

        let router = ProviderRouter::new(db);
        router.note_official_quota("account-a", 90.0).await;
        router.note_official_quota("account-b", 12.0).await;

        let selected = router
            .select_providers_for_request("codex", "gpt-5.5")
            .await
            .unwrap();
        assert_eq!(selected.providers[0].id, "backup");

        router
            .bind_official_affinity("codex_thread-hot", "main")
            .await;
        let sticky_cool = router
            .select_providers_for_request_with_session("codex", "gpt-5.5", Some("codex_thread-hot"))
            .await
            .unwrap();
        // Bound account is at 90%, backup is cooler → rebind.
        assert_eq!(sticky_cool.providers[0].id, "backup");

        router.note_official_quota("account-a", 40.0).await;
        router
            .bind_official_affinity("codex_thread-ok", "main")
            .await;
        let sticky_ok = router
            .select_providers_for_request_with_session("codex", "gpt-5.5", Some("codex_thread-ok"))
            .await
            .unwrap();
        assert_eq!(sticky_ok.providers[0].id, "main");

        let pinned = router
            .select_providers_for_request("codex", "main/gpt-5.5")
            .await
            .unwrap();
        assert_eq!(pinned.providers[0].id, "main");
    }

    #[tokio::test]
    #[serial]
    async fn kimi_coding_pool_picks_lowest_quota_and_keeps_slug_pin() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let mut main = kimi_coding_card("kimi-a", "key-a");
        main.sort_index = Some(0);
        let mut backup = kimi_coding_card("kimi-b", "key-b");
        backup.sort_index = Some(1);
        db.save_provider("codex", &main).unwrap();
        db.save_provider("codex", &backup).unwrap();
        db.set_current_provider("codex", "kimi-a").unwrap();

        let router = ProviderRouter::new(db);
        router.note_official_quota("kimi-a", 88.0).await;
        router.note_official_quota("kimi-b", 11.0).await;

        let selected = router
            .select_providers_for_request("codex", "kimi-for-coding")
            .await
            .unwrap();
        assert_eq!(selected.providers[0].id, "kimi-b");

        let pinned = router
            .select_providers_for_request("codex", "kimi-a/kimi-for-coding")
            .await
            .unwrap();
        assert_eq!(pinned.providers[0].id, "kimi-a");
    }

    #[tokio::test]
    #[serial]
    async fn anthropic_oauth_pool_picks_lowest_quota_on_claude_app() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let mut main = anthropic_oauth_card("claude-a", "sk-ant-oat01-a");
        main.sort_index = Some(0);
        let mut backup = anthropic_oauth_card("claude-b", "sk-ant-oat01-b");
        backup.sort_index = Some(1);
        db.save_provider("claude", &main).unwrap();
        db.save_provider("claude", &backup).unwrap();
        db.set_current_provider("claude", "claude-a").unwrap();

        let router = ProviderRouter::new(db);
        router.note_official_quota("claude-a", 90.0).await;
        router.note_official_quota("claude-b", 15.0).await;

        let selected = router
            .select_providers_for_request("claude", "claude-sonnet-4-6")
            .await
            .unwrap();
        assert_eq!(selected.providers[0].id, "claude-b");

        let pinned = router
            .select_providers_for_request("claude", "claude-a/claude-sonnet-4-6")
            .await
            .unwrap();
        assert_eq!(pinned.providers[0].id, "claude-a");
    }

    #[tokio::test]
    #[serial]
    async fn combo_expands_to_target_order_even_when_failover_off() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let kimi = catalog_provider("kimi", "Kimi", "k2");
        let deepseek = catalog_provider("deepseek", "DeepSeek", "deepseek-v4");
        db.save_provider("codex", &kimi).unwrap();
        db.save_provider("codex", &deepseek).unwrap();
        db.set_current_provider("codex", "kimi").unwrap();
        let mut config = db.get_proxy_config_for_app("codex").await.unwrap();
        config.auto_failover_enabled = false;
        db.update_proxy_config_for_app(config).await.unwrap();
        db.save_model_combos(&[crate::proxy::combo::ModelCombo {
            id: "main".into(),
            targets: vec![
                crate::proxy::combo::parse_combo_target_spec("deepseek/deepseek-v4").unwrap(),
                crate::proxy::combo::parse_combo_target_spec("kimi/k2").unwrap(),
            ],
            strategy: crate::proxy::combo::ComboStrategy::Failover,
            sticky_limit: 1,
        }])
        .unwrap();

        let router = ProviderRouter::new(db);
        let selected = router
            .select_providers_for_request("codex", "combo/main")
            .await
            .unwrap();
        assert_eq!(
            selected
                .providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["deepseek", "kimi"]
        );
        assert_eq!(
            selected.attempt_upstream_models.as_deref(),
            Some(["deepseek-v4".to_string(), "k2".to_string()].as_slice())
        );
        assert!(!selected.promote_current_on_success);

        let claude = router
            .select_providers_for_request("codex", "anthropic/combo/main")
            .await
            .unwrap();
        assert_eq!(claude.providers[0].id, "deepseek");

        let err = router
            .select_providers_for_request("codex", "combo/missing")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    struct RestoreEnv {
        key: &'static str,
        original: Option<String>,
    }

    impl RestoreEnv {
        fn set(key: &'static str, value: &str) -> Self {
            let original = env::var(key).ok();
            env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }

    async fn spawn_json_get(body: serde_json::Value) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let body = body.clone();
                async move { axum::Json(body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind usage mock");
        let addr = listener.local_addr().expect("usage mock addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve usage mock");
        });
        (format!("http://{addr}/"), handle)
    }

    fn managed_kimi_oauth_card(id: &str, account_id: &str) -> Provider {
        let mut provider = kimi_coding_card(id, "PROXY_MANAGED");
        provider.meta = Some(ProviderMeta {
            provider_type: Some("kimi_oauth".to_string()),
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("kimi_oauth".to_string()),
                account_id: Some(account_id.to_string()),
            }),
            ..Default::default()
        });
        provider
    }

    fn managed_anthropic_oauth_card(id: &str, account_id: &str) -> Provider {
        let mut provider = anthropic_oauth_card(id, "PROXY_MANAGED");
        provider.meta = Some(ProviderMeta {
            provider_type: Some("anthropic_oauth".to_string()),
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("anthropic_oauth".to_string()),
                account_id: Some(account_id.to_string()),
            }),
            ..Default::default()
        });
        provider
    }

    #[tokio::test]
    #[serial]
    async fn managed_kimi_oauth_quota_refresh_uses_account_store_token() {
        let _home = TempHome::new();
        let (url, handle) = spawn_json_get(json!({
            "limits": [{ "detail": { "limit": 100, "remaining": 86 } }]
        }))
        .await;
        let _env = RestoreEnv::set("CC_SWITCH_TEST_KIMI_USAGE_URL", &url);

        let oauth_dir = _home.dir.path().join("oauth");
        std::fs::create_dir_all(&oauth_dir).unwrap();
        let manager = KimiOAuthManager::new(oauth_dir);
        manager
            .add_test_account_with_access_token("acct-k", "kimi-managed-token", Some("k@kimi.com"))
            .await
            .unwrap();

        let provider = managed_kimi_oauth_card("kimi-a", "acct-k");
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);
        assert!(
            router
                .refresh_subscription_pool_quota_now(&provider, Some(&manager), None)
                .await
        );
        let quota = router.quota_for_test("acct-k").await.expect("kimi quota");
        assert!(
            (quota - 14.0).abs() < 1e-6,
            "expected ~14% used, got {quota}"
        );
        handle.abort();
    }

    #[tokio::test]
    #[serial]
    async fn managed_anthropic_oauth_quota_refresh_uses_account_store_token() {
        let _home = TempHome::new();
        let (url, handle) = spawn_json_get(json!({
            "five_hour": { "utilization": 22.5 }
        }))
        .await;
        let _env = RestoreEnv::set("CC_SWITCH_TEST_CLAUDE_USAGE_URL", &url);

        let oauth_dir = _home.dir.path().join("oauth");
        std::fs::create_dir_all(&oauth_dir).unwrap();
        let manager = AnthropicOAuthManager::new(oauth_dir);
        manager
            .add_test_account_with_access_token(
                "acct-a",
                "sk-ant-oat01-managed",
                Some("a@claude.ai"),
            )
            .await
            .unwrap();

        let provider = managed_anthropic_oauth_card("claude-a", "acct-a");
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);
        assert!(
            router
                .refresh_subscription_pool_quota_now(&provider, None, Some(&manager))
                .await
        );
        assert_eq!(router.quota_for_test("acct-a").await, Some(22.5));
        handle.abort();
    }

    #[tokio::test]
    #[serial]
    async fn managed_card_without_store_token_skips_quota_refresh() {
        let _home = TempHome::new();
        let provider = managed_kimi_oauth_card("kimi-a", "acct-missing");
        let oauth_dir = _home.dir.path().join("oauth");
        std::fs::create_dir_all(&oauth_dir).unwrap();
        let manager = KimiOAuthManager::new(oauth_dir);
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);
        assert!(
            !router
                .refresh_subscription_pool_quota_now(&provider, Some(&manager), None)
                .await
        );
        assert_eq!(router.quota_for_test("acct-missing").await, None);
    }
}
