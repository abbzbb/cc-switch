//! Subscription account pools + thread stickiness.
//!
//! Two or more cards of the same family that advertise the same native model
//! form a pool. Unprefixed requests pick the lowest known hottest-window usage
//! (unknown sorts last, never as 0%). A client-provided session stays on that
//! card until 401/403/429, or the hottest window is ≥ 80% and a cooler eligible
//! account exists. `{slug}/model` still pins that card fail-closed.
//!
//! Families:
//! - ChatGPT Official (`gpt-*` / `o1` `o3` `o4*` / `codex-*` / `chatgpt-*`)
//! - Anthropic OAuth cards holding `sk-ant-oat*` (`claude-*`)
//! - Kimi For Coding cards (`kimi-*` / `k2*` / `k3` / `k3-*`)
//!
//! Managed Official cards inject the bound ChatGPT account's token when the
//! inbound session does not already match, so Codex logged in as account A can
//! still hit `backup/gpt-5.5`.

use crate::provider::Provider;
use crate::proxy::providers::is_codex_official_provider;
use http::HeaderMap;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const PROXY_AUTH_PLACEHOLDER: &str = "PROXY_MANAGED";
const AFFINITY_CAP: usize = 1024;
pub const OFFICIAL_POOL_COOLDOWN: Duration = Duration::from_secs(60);
/// Hottest-window utilization at or above this may rebind a live thread.
pub const OFFICIAL_POOL_AUTO_SWITCH_THRESHOLD: f64 = 80.0;
pub const OFFICIAL_POOL_WHAM_MIN_INTERVAL: Duration = Duration::from_secs(60);

/// In-memory Official pool stickiness. Process-local, like OpenCodex.
#[derive(Debug, Default)]
pub struct OfficialPoolState {
    /// Client session id → Official provider id.
    affinity: HashMap<String, String>,
    affinity_order: VecDeque<String>,
    /// Provider id → cooldown deadline after 401/403/429.
    cooldown_until: HashMap<String, Instant>,
    /// Quota identity → hottest known window utilization (0–100).
    /// ChatGPT Official uses the ChatGPT account id; other families use provider id.
    quota_by_account: HashMap<String, f64>,
    last_wham_poll: HashMap<String, Instant>,
}

impl OfficialPoolState {
    pub fn bind(&mut self, session_id: &str, provider_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        if self
            .affinity
            .insert(session_id.to_string(), provider_id.to_string())
            .is_none()
        {
            self.affinity_order.push_back(session_id.to_string());
            while self.affinity_order.len() > AFFINITY_CAP {
                if let Some(oldest) = self.affinity_order.pop_front() {
                    self.affinity.remove(&oldest);
                }
            }
        }
    }

    pub fn clear_session(&mut self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        self.affinity.remove(session_id);
        self.affinity_order.retain(|id| id != session_id);
    }

    pub fn bound_provider(&self, session_id: &str) -> Option<&str> {
        self.affinity.get(session_id).map(String::as_str)
    }

    pub fn note_cooldown(&mut self, provider_id: &str, now: Instant) {
        self.cooldown_until
            .insert(provider_id.to_string(), now + OFFICIAL_POOL_COOLDOWN);
    }

    pub fn is_cooling(&self, provider_id: &str, now: Instant) -> bool {
        self.cooldown_until
            .get(provider_id)
            .is_some_and(|until| *until > now)
    }

    pub fn cooldown_map(&self) -> &HashMap<String, Instant> {
        &self.cooldown_until
    }

    pub fn note_quota(&mut self, account_id: &str, utilization: f64) {
        let account_id = account_id.trim();
        if account_id.is_empty() || !utilization.is_finite() {
            return;
        }
        self.quota_by_account
            .insert(account_id.to_string(), utilization.clamp(0.0, 100.0));
    }

    pub fn quota_map(&self) -> &HashMap<String, f64> {
        &self.quota_by_account
    }

    /// Returns true once if a WHAM poll may start for this account.
    pub fn try_begin_wham_poll(&mut self, account_id: &str, now: Instant) -> bool {
        let account_id = account_id.trim();
        if account_id.is_empty() {
            return false;
        }
        if self.last_wham_poll.get(account_id).is_some_and(|last| {
            now.saturating_duration_since(*last) < OFFICIAL_POOL_WHAM_MIN_INTERVAL
        }) {
            return false;
        }
        self.last_wham_poll.insert(account_id.to_string(), now);
        true
    }
}

pub fn provider_chatgpt_account_id(provider: &Provider) -> Option<String> {
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
        .map(|account_id| account_id.trim().to_string())
        .filter(|account_id| !account_id.is_empty())
}

fn managed_quota_account_id(provider: &Provider, auth_provider: &str) -> Option<String> {
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for(auth_provider))
        .map(|account_id| account_id.trim().to_string())
        .filter(|account_id| !account_id.is_empty())
}

/// Key used in the in-memory quota map for this card.
pub fn quota_identity(provider: &Provider) -> Option<String> {
    if let Some(account_id) = provider_chatgpt_account_id(provider) {
        return Some(account_id);
    }
    if let Some(account_id) = managed_quota_account_id(provider, "anthropic_oauth") {
        return Some(account_id);
    }
    if let Some(account_id) = managed_quota_account_id(provider, "kimi_oauth") {
        return Some(account_id);
    }
    if is_anthropic_oauth_provider(provider) || is_kimi_coding_provider(provider) {
        let id = provider.id.trim();
        if id.is_empty() {
            return None;
        }
        return Some(id.to_string());
    }
    None
}

pub fn usage_of_provider(provider: &Provider, usage: &HashMap<String, f64>) -> Option<f64> {
    quota_identity(provider).and_then(|identity| usage.get(&identity).copied())
}

/// Stored inference credential, if the card has a real key (not PROXY_MANAGED).
pub fn pool_card_secret(provider: &Provider) -> Option<String> {
    let settings = &provider.settings_config;
    if let Some(env) = settings.get("env") {
        for key in [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
        ] {
            if let Some(value) = env
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|value| is_usable_pool_secret(value))
            {
                return Some(value.to_string());
            }
        }
    }
    if let Some(auth) = settings.get("auth") {
        if let Some(key) = crate::codex_config::extract_codex_auth_api_key(auth) {
            let key = key.trim();
            if is_usable_pool_secret(key) {
                return Some(key.to_string());
            }
        }
    }
    for key in ["apiKey", "api_key"] {
        if let Some(value) = settings
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| is_usable_pool_secret(value))
        {
            return Some(value.to_string());
        }
    }
    None
}

fn is_usable_pool_secret(value: &str) -> bool {
    !value.is_empty() && !value.contains(PROXY_AUTH_PLACEHOLDER)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionPoolFamily {
    CodexOfficial,
    AnthropicOauth,
    KimiCoding,
}

pub fn pool_family_for_unprefixed_model(model: &str) -> Option<SubscriptionPoolFamily> {
    if is_unprefixed_openai_native_model(model) {
        Some(SubscriptionPoolFamily::CodexOfficial)
    } else if is_unprefixed_claude_native_model(model) {
        Some(SubscriptionPoolFamily::AnthropicOauth)
    } else if is_unprefixed_kimi_native_model(model) {
        Some(SubscriptionPoolFamily::KimiCoding)
    } else {
        None
    }
}

pub fn provider_pool_family(provider: &Provider) -> Option<SubscriptionPoolFamily> {
    if is_codex_official_provider(provider) {
        Some(SubscriptionPoolFamily::CodexOfficial)
    } else if is_anthropic_oauth_provider(provider) {
        Some(SubscriptionPoolFamily::AnthropicOauth)
    } else if is_kimi_coding_provider(provider) {
        Some(SubscriptionPoolFamily::KimiCoding)
    } else {
        None
    }
}

pub fn is_subscription_pool_provider(provider: &Provider) -> bool {
    provider_pool_family(provider).is_some()
}

/// Claude Pro/Max OAuth: managed store binding, or `sk-ant-oat…` on the card.
pub fn is_anthropic_oauth_provider(provider: &Provider) -> bool {
    if provider.is_anthropic_oauth() {
        return true;
    }
    if is_codex_official_provider(provider)
        || provider.is_codex_oauth()
        || provider.is_xai_oauth()
        || provider.is_kimi_oauth()
        || provider.is_github_copilot()
    {
        return false;
    }
    pool_card_secret(provider)
        .is_some_and(|secret| secret.to_ascii_lowercase().starts_with("sk-ant-oat"))
}

/// Kimi For Coding / Kimi Code subscription cards (API key or managed OAuth).
pub fn is_kimi_coding_provider(provider: &Provider) -> bool {
    if provider.is_kimi_oauth() {
        return true;
    }
    if is_codex_official_provider(provider)
        || is_anthropic_oauth_provider(provider)
        || provider.is_codex_oauth()
        || provider.is_xai_oauth()
        || provider.is_github_copilot()
    {
        return false;
    }
    if pool_card_secret(provider).is_none() {
        return false;
    }
    let ids = crate::proxy::model_routing::provider_upstream_model_ids(provider);
    if ids.is_empty() {
        return false;
    }
    let hay = provider_base_url_haystack(provider).to_ascii_lowercase();
    let coding_host = hay.contains("api.kimi.com/coding") || hay.contains("kimi.com/coding");
    coding_host || ids.iter().all(|id| is_kimi_native_model_id(id))
}

fn provider_base_url_haystack(provider: &Provider) -> String {
    let settings = &provider.settings_config;
    let mut parts = Vec::new();
    if let Some(env) = settings.get("env") {
        if let Some(url) = env.get("ANTHROPIC_BASE_URL").and_then(|v| v.as_str()) {
            parts.push(url.to_string());
        }
    }
    for key in ["base_url", "baseURL", "apiEndpoint"] {
        if let Some(url) = settings.get(key).and_then(|v| v.as_str()) {
            parts.push(url.to_string());
        }
    }
    if let Some(config_text) = settings.get("config").and_then(Value::as_str) {
        parts.push(config_text.to_string());
    } else if let Some(config) = settings.get("config") {
        if let Some(url) = config.get("base_url").and_then(|v| v.as_str()) {
            parts.push(url.to_string());
        }
    }
    parts.join("\n")
}

/// Hottest known window (0–100). Failed / empty snapshots are unknown, not 0%.
pub fn hottest_quota_utilization(
    quota: &crate::services::subscription::SubscriptionQuota,
) -> Option<f64> {
    if !quota.success {
        return None;
    }
    quota
        .tiers
        .iter()
        .map(|tier| tier.utilization)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value.min(100.0))
        .max_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal))
}

/// Capture leftover `x-codex-*-used-percent` headers when ChatGPT still sends them.
pub fn hottest_quota_from_response_headers(headers: &HeaderMap) -> Option<f64> {
    let mut hottest: Option<f64> = None;
    for name in [
        "x-codex-primary-used-percent",
        "x-codex-secondary-used-percent",
    ] {
        let Some(raw) = headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let Ok(parsed) = raw.parse::<f64>() else {
            continue;
        };
        if !parsed.is_finite() || parsed < 0.0 {
            continue;
        }
        let clamped = parsed.min(100.0);
        hottest = Some(hottest.map_or(clamped, |current| current.max(clamped)));
    }
    hottest
}

/// Keep a bound thread unless the bound account is at/over the switch
/// threshold and another eligible card has strictly lower known usage.
/// Unknown bound usage yields to a known account still below the threshold.
pub fn keep_official_affinity(
    bound: &Provider,
    eligible: &[&Provider],
    usage: &HashMap<String, f64>,
    threshold: f64,
) -> bool {
    match usage_of_provider(bound, usage) {
        Some(score) if score < threshold => true,
        Some(score) => !eligible.iter().any(|provider| {
            provider.id != bound.id
                && usage_of_provider(provider, usage).is_some_and(|other| other < score)
        }),
        None => !eligible.iter().any(|provider| {
            provider.id != bound.id
                && usage_of_provider(provider, usage).is_some_and(|other| other < threshold)
        }),
    }
}

/// Bare OpenAI-native ids that ChatGPT Official cards advertise.
pub fn is_unprefixed_openai_native_model(model: &str) -> bool {
    let model = model.trim();
    if model.is_empty() || model.contains('/') {
        return false;
    }
    let lower = model.to_ascii_lowercase();
    lower.starts_with("gpt-")
        || lower.starts_with("chatgpt-")
        || lower.starts_with("codex-")
        || lower == "o1"
        || lower.starts_with("o1-")
        || lower == "o3"
        || lower.starts_with("o3-")
        || lower == "o4"
        || lower.starts_with("o4-")
}

pub fn is_unprefixed_claude_native_model(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && !model.contains('/') && model.to_ascii_lowercase().starts_with("claude-")
}

pub fn is_unprefixed_kimi_native_model(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && !model.contains('/') && is_kimi_native_model_id(model)
}

fn is_kimi_native_model_id(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    let leaf = lower.rsplit('/').next().unwrap_or(lower.as_str());
    leaf.starts_with("kimi")
        || leaf.starts_with("k2")
        || leaf == "k3"
        || leaf.starts_with("k3-")
        || leaf.starts_with("moonshot")
}

/// Official cards that advertise `model` in their catalog / fallback list.
#[cfg(test)]
pub fn official_pool_candidates<'a>(providers: &'a [Provider], model: &str) -> Vec<&'a Provider> {
    subscription_pool_candidates(providers, model, SubscriptionPoolFamily::CodexOfficial)
}

pub fn subscription_pool_candidates<'a>(
    providers: &'a [Provider],
    model: &str,
    family: SubscriptionPoolFamily,
) -> Vec<&'a Provider> {
    let model = model.trim();
    providers
        .iter()
        .filter(|provider| provider_pool_family(provider) == Some(family))
        .filter(|provider| {
            crate::proxy::model_routing::provider_upstream_model_ids(provider)
                .iter()
                .any(|id| crate::proxy::model_routing::models_match(id, model))
        })
        .collect()
}

/// Pick among Official pool candidates.
///
/// Cooling cards are skipped unless every candidate is cooling. Known quota
/// (hottest window) wins lowest-usage; unknown sorts last (never treated as
/// 0%). Current card and `sort_index` / id are tie-breakers.
pub fn pick_official_pool(
    current_id: Option<&str>,
    candidates: &[&Provider],
    cooldown_until: &HashMap<String, Instant>,
    now: Instant,
    usage: &HashMap<String, f64>,
) -> Option<Provider> {
    if candidates.len() < 2 {
        return None;
    }
    let eligible: Vec<&Provider> = candidates
        .iter()
        .copied()
        .filter(|provider| {
            cooldown_until
                .get(&provider.id)
                .is_none_or(|until| *until <= now)
        })
        .collect();
    let mut pool = if eligible.is_empty() {
        candidates.to_vec()
    } else {
        eligible
    };

    pool.sort_by(|left, right| {
        match (
            usage_of_provider(left, usage),
            usage_of_provider(right, usage),
        ) {
            (Some(left_usage), Some(right_usage)) => left_usage
                .partial_cmp(&right_usage)
                .unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| {
            let left_current = current_id == Some(left.id.as_str());
            let right_current = current_id == Some(right.id.as_str());
            right_current.cmp(&left_current)
        })
        .then_with(|| left.sort_index.cmp(&right.sort_index))
        .then_with(|| left.id.cmp(&right.id))
    });
    pool.first().copied().cloned()
}

/// When set, skip Official passthrough/validate and inject this ChatGPT account.
pub fn official_pool_inject_account_id(headers: &HeaderMap, provider: &Provider) -> Option<String> {
    if !is_codex_official_provider(provider) {
        return None;
    }
    let expected = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
        .map(|account_id| account_id.trim().to_string())
        .filter(|account_id| !account_id.is_empty())?;

    let authorization = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let request_account_id = headers
        .get("chatgpt-account-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|account_id| !account_id.is_empty());

    let needs_inject = match authorization {
        None => true,
        Some(value) if value.contains(PROXY_AUTH_PLACEHOLDER) => true,
        Some(_) => request_account_id != Some(expected.as_str()),
    };
    needs_inject.then_some(expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AuthBinding, AuthBindingSource, ProviderMeta};
    use http::HeaderValue;
    use serde_json::json;

    fn managed_official(id: &str, account_id: &str, sort_index: Option<usize>) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            format!("Official {id}"),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        provider.category = Some("official".to_string());
        provider.sort_index = sort_index;
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

    fn unbound_official() -> Provider {
        let mut provider = Provider::with_id(
            "codex-official".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        provider.category = Some("official".to_string());
        provider
    }

    fn auth_headers(authorization: Option<&str>, account_id: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(authorization) = authorization {
            headers.insert(
                http::header::AUTHORIZATION,
                HeaderValue::from_str(authorization).expect("authorization"),
            );
        }
        if let Some(account_id) = account_id {
            headers.insert(
                "chatgpt-account-id",
                HeaderValue::from_str(account_id).expect("account id"),
            );
        }
        headers
    }

    #[test]
    fn native_model_grammar() {
        assert!(is_unprefixed_openai_native_model("gpt-5.5"));
        assert!(is_unprefixed_openai_native_model("o3-mini"));
        assert!(is_unprefixed_openai_native_model("codex-mini"));
        assert!(!is_unprefixed_openai_native_model("backup/gpt-5.5"));
        assert!(!is_unprefixed_openai_native_model("k2"));
        assert!(!is_unprefixed_openai_native_model(
            "anthropic/claude-sonnet-5"
        ));
        assert!(is_unprefixed_claude_native_model("claude-sonnet-4-6"));
        assert!(!is_unprefixed_claude_native_model(
            "anthropic/claude-sonnet-4-6"
        ));
        assert!(is_unprefixed_kimi_native_model("kimi-for-coding"));
        assert!(is_unprefixed_kimi_native_model("k2"));
        assert!(is_unprefixed_kimi_native_model("k3-256k"));
        assert!(!is_unprefixed_kimi_native_model("kimi/k2"));
    }

    #[test]
    fn inject_passthrough_when_inbound_account_matches() {
        let provider = managed_official("backup", "account-b", None);
        let headers = auth_headers(Some("Bearer live-token"), Some("account-b"));
        assert_eq!(official_pool_inject_account_id(&headers, &provider), None);
    }

    #[test]
    fn inject_when_inbound_account_mismatches() {
        let provider = managed_official("backup", "account-b", None);
        let headers = auth_headers(Some("Bearer account-a-token"), Some("account-a"));
        assert_eq!(
            official_pool_inject_account_id(&headers, &provider).as_deref(),
            Some("account-b")
        );
    }

    #[test]
    fn inject_when_proxy_managed_or_missing_auth() {
        let provider = managed_official("backup", "account-b", None);
        assert_eq!(
            official_pool_inject_account_id(
                &auth_headers(Some("Bearer PROXY_MANAGED"), None),
                &provider
            )
            .as_deref(),
            Some("account-b")
        );
        assert_eq!(
            official_pool_inject_account_id(&HeaderMap::new(), &provider).as_deref(),
            Some("account-b")
        );
    }

    #[test]
    fn unbound_official_never_injects() {
        let provider = unbound_official();
        let headers = auth_headers(Some("Bearer PROXY_MANAGED"), Some("account-a"));
        assert_eq!(official_pool_inject_account_id(&headers, &provider), None);
    }

    #[test]
    fn pick_skips_cooldown_then_falls_back() {
        let main = managed_official("main", "account-a", Some(0));
        let backup = managed_official("backup", "account-b", Some(1));
        let candidates = vec![&main, &backup];
        let now = Instant::now();

        let picked = pick_official_pool(
            Some("main"),
            &candidates,
            &HashMap::new(),
            now,
            &HashMap::new(),
        )
        .expect("pool of two");
        assert_eq!(picked.id, "main");

        let mut cooldown = HashMap::new();
        cooldown.insert("main".to_string(), now + Duration::from_secs(30));
        let picked = pick_official_pool(Some("main"), &candidates, &cooldown, now, &HashMap::new())
            .expect("skip cooling current");
        assert_eq!(picked.id, "backup");

        cooldown.insert("backup".to_string(), now + Duration::from_secs(30));
        let picked = pick_official_pool(Some("main"), &candidates, &cooldown, now, &HashMap::new())
            .expect("all cooling still picks current");
        assert_eq!(picked.id, "main");
    }

    #[test]
    fn pick_sorts_when_current_is_not_in_pool() {
        let main = managed_official("main", "account-a", Some(1));
        let backup = managed_official("backup", "account-b", Some(0));
        let candidates = vec![&main, &backup];
        let picked = pick_official_pool(
            Some("kimi"),
            &candidates,
            &HashMap::new(),
            Instant::now(),
            &HashMap::new(),
        )
        .expect("sorted pick");
        assert_eq!(picked.id, "backup");
    }

    #[test]
    fn affinity_evicts_oldest_and_clears() {
        let mut state = OfficialPoolState::default();
        state.bind("s1", "main");
        state.bind("s2", "backup");
        assert_eq!(state.bound_provider("s1"), Some("main"));
        state.clear_session("s1");
        assert_eq!(state.bound_provider("s1"), None);
        assert_eq!(state.bound_provider("s2"), Some("backup"));

        let now = Instant::now();
        state.note_cooldown("main", now);
        assert!(state.is_cooling("main", now));
        assert!(!state.is_cooling("backup", now));
    }

    #[test]
    fn pick_lowest_known_usage_beats_current() {
        let main = managed_official("main", "account-a", Some(0));
        let backup = managed_official("backup", "account-b", Some(1));
        let providers = [main.clone(), backup.clone()];
        let candidates = official_pool_candidates(&providers, "gpt-5.5");
        assert_eq!(candidates.len(), 2);
        let mut usage = HashMap::new();
        usage.insert("account-a".to_string(), 90.0);
        usage.insert("account-b".to_string(), 10.0);
        let picked = pick_official_pool(
            Some("main"),
            &candidates,
            &HashMap::new(),
            Instant::now(),
            &usage,
        )
        .expect("quota pick");
        assert_eq!(picked.id, "backup");
    }

    #[test]
    fn unknown_usage_sorts_after_known() {
        let main = managed_official("main", "account-a", Some(0));
        let backup = managed_official("backup", "account-b", Some(1));
        let candidates = vec![&main, &backup];
        let mut usage = HashMap::new();
        usage.insert("account-b".to_string(), 40.0);
        let picked = pick_official_pool(
            Some("main"),
            &candidates,
            &HashMap::new(),
            Instant::now(),
            &usage,
        )
        .expect("known beats unknown");
        assert_eq!(picked.id, "backup");
    }

    #[test]
    fn keep_affinity_until_threshold_then_switch() {
        let main = managed_official("main", "account-a", Some(0));
        let backup = managed_official("backup", "account-b", Some(1));
        let eligible = vec![&main, &backup];
        let mut usage = HashMap::new();
        usage.insert("account-a".to_string(), 50.0);
        usage.insert("account-b".to_string(), 10.0);
        assert!(keep_official_affinity(
            &main,
            &eligible,
            &usage,
            OFFICIAL_POOL_AUTO_SWITCH_THRESHOLD
        ));

        usage.insert("account-a".to_string(), 85.0);
        assert!(!keep_official_affinity(
            &main,
            &eligible,
            &usage,
            OFFICIAL_POOL_AUTO_SWITCH_THRESHOLD
        ));
    }

    #[test]
    fn unknown_bound_yields_to_known_headroom() {
        let main = managed_official("main", "account-a", Some(0));
        let backup = managed_official("backup", "account-b", Some(1));
        let eligible = vec![&main, &backup];
        let mut usage = HashMap::new();
        usage.insert("account-b".to_string(), 20.0);
        assert!(!keep_official_affinity(
            &main,
            &eligible,
            &usage,
            OFFICIAL_POOL_AUTO_SWITCH_THRESHOLD
        ));
    }

    #[test]
    fn hottest_quota_uses_max_window_and_ignores_failures() {
        use crate::services::subscription::{CredentialStatus, QuotaTier, SubscriptionQuota};

        let quota = SubscriptionQuota {
            tool: "codex_oauth".into(),
            credential_status: CredentialStatus::Valid,
            credential_message: None,
            success: true,
            tiers: vec![
                QuotaTier {
                    name: "seven_day".into(),
                    utilization: 31.0,
                    resets_at: None,
                    used_value_usd: None,
                    max_value_usd: None,
                },
                QuotaTier {
                    name: "30_day".into(),
                    utilization: 72.5,
                    resets_at: None,
                    used_value_usd: None,
                    max_value_usd: None,
                },
            ],
            extra_usage: None,
            error: None,
            queried_at: None,
        };
        assert_eq!(hottest_quota_utilization(&quota), Some(72.5));

        let failed = SubscriptionQuota {
            success: false,
            ..quota.clone()
        };
        assert_eq!(hottest_quota_utilization(&failed), None);
    }

    #[test]
    fn hottest_quota_from_headers_takes_max() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-primary-used-percent",
            HeaderValue::from_static("12"),
        );
        headers.insert(
            "x-codex-secondary-used-percent",
            HeaderValue::from_static("44.5"),
        );
        assert_eq!(hottest_quota_from_response_headers(&headers), Some(44.5));
    }

    #[test]
    fn wham_poll_is_throttled() {
        let mut state = OfficialPoolState::default();
        let now = Instant::now();
        assert!(state.try_begin_wham_poll("account-a", now));
        assert!(!state.try_begin_wham_poll("account-a", now + Duration::from_secs(10)));
        assert!(state.try_begin_wham_poll("account-a", now + Duration::from_secs(61)));
    }

    fn anthropic_oauth_card(id: &str, token: &str, sort_index: Option<usize>) -> Provider {
        let mut provider = Provider::with_id(
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
        );
        provider.sort_index = sort_index;
        provider
    }

    fn kimi_coding_card(id: &str, key: &str, sort_index: Option<usize>) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            format!("Kimi {id}"),
            json!({
                "auth": { "OPENAI_API_KEY": key },
                "config": format!(
                    "model = \"kimi-for-coding\"\n\n[model_providers.{id}]\nbase_url = \"https://api.kimi.com/coding/v1\"\n"
                ),
                "modelCatalog": { "models": [{ "model": "kimi-for-coding" }, { "model": "k2" }] }
            }),
            None,
        );
        provider.sort_index = sort_index;
        provider
    }

    #[test]
    fn anthropic_and_kimi_family_detection() {
        let oat = anthropic_oauth_card("claude-a", "sk-ant-oat01-test", Some(0));
        let api_key = anthropic_oauth_card("claude-api", "sk-ant-api03-test", Some(1));
        let kimi = kimi_coding_card("kimi-a", "kimi-key-a", Some(0));
        assert!(is_anthropic_oauth_provider(&oat));
        assert!(!is_anthropic_oauth_provider(&api_key));
        assert!(is_kimi_coding_provider(&kimi));
        assert!(!is_kimi_coding_provider(&oat));
        assert_eq!(quota_identity(&oat).as_deref(), Some("claude-a"));
        assert_eq!(quota_identity(&kimi).as_deref(), Some("kimi-a"));
    }

    fn managed_kimi_card(id: &str, account_id: &str) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            format!("Kimi {id}"),
            json!({
                "auth": { "OPENAI_API_KEY": "PROXY_MANAGED" },
                "config": "[model_providers.kimi]\nbase_url = \"https://api.kimi.com/coding/v1\"\n",
                "modelCatalog": { "models": [{ "model": "kimi-for-coding" }] }
            }),
            None,
        );
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

    fn managed_anthropic_card(id: &str, account_id: &str) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            format!("Claude {id}"),
            json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": "PROXY_MANAGED",
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                    "ANTHROPIC_MODEL": "claude-sonnet-4-6"
                }
            }),
            None,
        );
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

    #[test]
    fn managed_kimi_and_anthropic_use_account_id_as_quota_identity() {
        let kimi = managed_kimi_card("kimi-card", "kimi-acct");
        let claude = managed_anthropic_card("claude-card", "claude-acct");
        assert!(is_kimi_coding_provider(&kimi));
        assert!(is_anthropic_oauth_provider(&claude));
        assert_eq!(quota_identity(&kimi).as_deref(), Some("kimi-acct"));
        assert_eq!(quota_identity(&claude).as_deref(), Some("claude-acct"));
    }

    #[test]
    fn kimi_pool_picks_lowest_usage_by_provider_id() {
        let main = kimi_coding_card("kimi-a", "key-a", Some(0));
        let backup = kimi_coding_card("kimi-b", "key-b", Some(1));
        let providers = [main.clone(), backup.clone()];
        let candidates = subscription_pool_candidates(
            &providers,
            "kimi-for-coding",
            SubscriptionPoolFamily::KimiCoding,
        );
        assert_eq!(candidates.len(), 2);
        let mut usage = HashMap::new();
        usage.insert("kimi-a".to_string(), 91.0);
        usage.insert("kimi-b".to_string(), 14.0);
        let picked = pick_official_pool(
            Some("kimi-a"),
            &candidates,
            &HashMap::new(),
            Instant::now(),
            &usage,
        )
        .expect("kimi pool");
        assert_eq!(picked.id, "kimi-b");
    }

    #[test]
    fn anthropic_pool_unknown_usage_sorts_after_known() {
        let main = anthropic_oauth_card("claude-a", "sk-ant-oat01-a", Some(0));
        let backup = anthropic_oauth_card("claude-b", "sk-ant-oat01-b", Some(1));
        let providers = [main.clone(), backup.clone()];
        let candidates = subscription_pool_candidates(
            &providers,
            "claude-sonnet-4-6",
            SubscriptionPoolFamily::AnthropicOauth,
        );
        let mut usage = HashMap::new();
        usage.insert("claude-b".to_string(), 20.0);
        let picked = pick_official_pool(
            Some("claude-a"),
            &candidates,
            &HashMap::new(),
            Instant::now(),
            &usage,
        )
        .expect("anthropic pool");
        assert_eq!(picked.id, "claude-b");
    }
}
