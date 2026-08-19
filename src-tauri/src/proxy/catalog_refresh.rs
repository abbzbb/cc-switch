//! Refresh the merged Codex picker catalog from each card's upstream `/v1/models`.
//!
//! The on-disk `cc-switch-model-catalog.json` is still a static projection. This
//! module fills a sidecar discovery cache so the next projection includes every
//! model the vendor advertised, not only the card's current `model` / mapping
//! table. Failures stay local: the last good cache (or mapping table) remains.

use crate::codex_config::{extract_codex_api_key, extract_codex_base_url};
use crate::provider::Provider;
use crate::proxy::model_routing::{
    load_routing_discovery_cache, save_routing_discovery_cache, MAX_DISCOVERED_MODELS_PER_CARD,
};
use crate::proxy::providers::is_codex_official_provider;
use crate::services::model_fetch::{self, FetchedModel};
use std::collections::HashMap;

const PROXY_AUTH_PLACEHOLDER: &str = "PROXY_MANAGED";

pub fn catalog_fetch_disabled() -> bool {
    std::env::var("CC_SWITCH_TEST_HOME").is_ok()
        && std::env::var("CC_SWITCH_CATALOG_FETCH").ok().as_deref() != Some("1")
}

pub fn is_loopback_or_proxy_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    let after_scheme = lower.split("://").nth(1).unwrap_or(lower.as_str());
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "0.0.0.0")
}

pub fn provider_upstream_base_url(provider: &Provider) -> Option<String> {
    let config = provider
        .settings_config
        .get("config")
        .and_then(|value| value.as_str())?;
    let url = extract_codex_base_url(config)?;
    let trimmed = url.trim();
    if trimmed.is_empty() || is_loopback_or_proxy_url(trimmed) {
        return None;
    }
    Some(trimmed.to_string())
}

pub fn provider_fetch_api_key(provider: &Provider) -> Option<String> {
    let auth = provider.settings_config.get("auth");
    let config = provider
        .settings_config
        .get("config")
        .and_then(|value| value.as_str());
    let key = extract_codex_api_key(auth, config)?;
    if key == PROXY_AUTH_PLACEHOLDER {
        return None;
    }
    Some(key)
}

pub fn fetched_ids(models: &[FetchedModel]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for model in models {
        let id = model.id.trim();
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        ids.push(id.to_string());
        if ids.len() >= MAX_DISCOVERED_MODELS_PER_CARD {
            break;
        }
    }
    ids
}

pub async fn discover_models_for_provider(provider: &Provider) -> Result<Vec<String>, String> {
    if is_codex_official_provider(provider) {
        return Err("official catalog uses the local seed; live ChatGPT fetch is skipped".into());
    }
    let base_url =
        provider_upstream_base_url(provider).ok_or_else(|| "no upstream base_url".to_string())?;
    let api_key = provider_fetch_api_key(provider).ok_or_else(|| "no API key".to_string())?;
    let is_full_url = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.is_full_url)
        .unwrap_or(false);
    let api_format = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.api_format.as_deref());
    let user_agent = crate::provider::parse_custom_user_agent(
        provider
            .meta
            .as_ref()
            .and_then(|meta| meta.custom_user_agent.as_deref()),
    )
    .ok()
    .flatten();
    let models = model_fetch::fetch_models(
        &base_url,
        &api_key,
        is_full_url,
        None,
        user_agent,
        api_format,
        None,
    )
    .await?;
    Ok(fetched_ids(&models))
}

pub async fn refresh_discovery_cache(providers: &[Provider]) -> HashMap<String, Vec<String>> {
    let mut cache = load_routing_discovery_cache();
    if catalog_fetch_disabled() {
        return cache;
    }
    for provider in providers {
        if provider.meta.as_ref().and_then(|meta| meta.routing_catalog) == Some(false) {
            continue;
        }
        match discover_models_for_provider(provider).await {
            Ok(ids) if !ids.is_empty() => {
                cache.insert(provider.id.clone(), ids);
            }
            Ok(_) => {}
            Err(error) => {
                log::debug!(
                    "[catalog-refresh] skip {} ({}): {error}",
                    provider.id,
                    provider.name
                );
            }
        }
    }
    save_routing_discovery_cache(&cache);
    cache
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use serde_json::json;

    #[test]
    fn loopback_urls_are_skipped() {
        assert!(is_loopback_or_proxy_url("http://127.0.0.1:15721/v1"));
        assert!(is_loopback_or_proxy_url("http://localhost:15721/v1"));
        assert!(is_loopback_or_proxy_url("http://[::1]:15721/v1"));
        assert!(!is_loopback_or_proxy_url("https://api.x.ai/v1"));
    }

    #[test]
    fn proxy_managed_key_is_not_used_for_fetch() {
        let provider = Provider::with_id(
            "p".into(),
            "P".into(),
            json!({
                "auth": { "OPENAI_API_KEY": "PROXY_MANAGED" },
                "config": "model_provider = \"x\"\n[model_providers.x]\nbase_url = \"https://api.x.ai/v1\"\n"
            }),
            None,
        );
        assert!(provider_fetch_api_key(&provider).is_none());
        assert_eq!(
            provider_upstream_base_url(&provider).as_deref(),
            Some("https://api.x.ai/v1")
        );
    }

    #[test]
    fn fetched_ids_cap_and_dedup() {
        let models: Vec<FetchedModel> = (0..3)
            .map(|i| FetchedModel {
                id: format!("m-{i}"),
                owned_by: None,
            })
            .chain([FetchedModel {
                id: "m-0".into(),
                owned_by: None,
            }])
            .collect();
        assert_eq!(fetched_ids(&models), vec!["m-0", "m-1", "m-2"]);
    }
}
