//! OpenCodex-style `provider/model` routing.
//!
//! Request `model` values whose first path segment matches a provider routing
//! slug pin that card (no cross-provider failover). Catalog injection writes
//! the same ids so Codex's model picker can target any configured card.

use crate::codex_config::{
    get_codex_config_dir, get_codex_model_catalog_path, CodexCatalogToolProfile,
};
use crate::config::write_json_file;
use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::providers::{is_codex_official_provider, resolve_codex_catalog_tool_profile};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Cap on models cached from one card's upstream `/v1/models`.
pub const MAX_DISCOVERED_MODELS_PER_CARD: usize = 200;

const ROUTING_DISCOVERY_FILENAME: &str = "cc-switch-model-discovery.json";

/// Official Codex models the picker should keep even when the card only
/// stored a single current `model =`. Live ChatGPT `/models` is not mirrored
/// here; this is a local seed so unprefixed `gpt-*` rows do not collapse to
/// just `gpt-5.5`.
const OFFICIAL_CATALOG_SEED: &[&str] = &[
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6",
    "gpt-5.6-sol",
    "gpt-5.6-luna",
    "gpt-5.6-terra",
    "gpt-5.3-codex-spark",
    "codex-auto-review",
];

pub fn routing_discovery_cache_path() -> PathBuf {
    get_codex_config_dir().join(ROUTING_DISCOVERY_FILENAME)
}

pub fn load_routing_discovery_cache() -> HashMap<String, Vec<String>> {
    let path = routing_discovery_cache_path();
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save_routing_discovery_cache(cache: &HashMap<String, Vec<String>>) {
    let _ = write_json_file(&routing_discovery_cache_path(), cache);
}

/// Result of inspecting a request model id against the app's provider cards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRouteDecision {
    /// Keep current-provider / failover selection.
    Default,
    /// Pin a single card and strip the routing prefix before upstream mapping.
    Pinned {
        provider_id: String,
        upstream_model: String,
    },
}

/// Normalize a user-supplied or derived routing slug.
#[cfg(test)]
pub fn normalize_routing_slug(raw: &str) -> Option<String> {
    if raw.trim().is_empty() {
        return None;
    }
    let slug = slugify(raw);
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

/// Routing slug for one provider, ignoring collisions with siblings.
pub fn preferred_routing_slug(provider: &Provider) -> String {
    if let Some(override_slug) = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.routing_slug.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return slugify(override_slug);
    }

    let id = provider.id.trim();
    if is_valid_slug(id) && !is_uuid_like(id) {
        return id.to_ascii_lowercase();
    }

    let from_name = slugify(&provider.name);
    if from_name.is_empty() {
        slugify(&provider.id)
    } else {
        from_name
    }
}

/// Assign unique slugs across a provider set. Explicit `routingSlug` wins;
/// collisions get a sanitized id suffix.
pub fn assign_routing_slugs(providers: &[Provider]) -> HashMap<String, String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut assigned = HashMap::new();

    let mut with_override = Vec::new();
    let mut without_override = Vec::new();
    for provider in providers {
        if provider
            .meta
            .as_ref()
            .and_then(|meta| meta.routing_slug.as_deref())
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        {
            with_override.push(provider);
        } else {
            without_override.push(provider);
        }
    }

    for provider in with_override.into_iter().chain(without_override) {
        let mut slug = preferred_routing_slug(provider);
        if slug.is_empty() {
            slug = "provider".to_string();
        }
        if used.contains(&slug) {
            let suffix = sanitize_id_suffix(&provider.id);
            slug = format!("{slug}-{suffix}");
            let mut n = 2u32;
            while used.contains(&slug) {
                slug = format!("{}-{suffix}-{n}", preferred_routing_slug(provider));
                n += 1;
            }
        }
        used.insert(slug.clone());
        assigned.insert(provider.id.clone(), slug);
    }

    assigned
}

/// Whether this card should contribute rows to the merged Codex/Claude catalog.
pub fn participates_in_routing_catalog(provider: &Provider) -> bool {
    if provider.meta.as_ref().and_then(|meta| meta.routing_catalog) == Some(false) {
        return false;
    }
    is_codex_official_provider(provider) || !provider_upstream_model_ids(provider).is_empty()
}

/// Persist the catalog opt-in on a card. `true` restores the default
/// (omit the flag); `false` writes an explicit opt-out. Other meta stays.
pub fn set_routing_catalog_enabled(provider: &mut Provider, enabled: bool) {
    let mut meta = provider.meta.take().unwrap_or_default();
    meta.routing_catalog = if enabled { None } else { Some(false) };
    provider.meta = Some(meta);
}

/// Alias inner `/` in an upstream model id to `-` for catalog slugs.
pub fn alias_inner_slashes(model: &str) -> String {
    model.replace('/', "-")
}

pub fn models_match(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    left.eq_ignore_ascii_case(right)
        || alias_inner_slashes(left).eq_ignore_ascii_case(&alias_inner_slashes(right))
}

/// Claude Code gateway discovery keeps `/v1/models` rows whose `id` contains
/// `claude` or `anthropic` (and, before v2.1.223, required that prefix). Catalog
/// ids are therefore `anthropic/{routing_slug}/{model}` so `kimi/k2` can appear
/// in `/model`. Request routing peels this prefix only when the next segment is
/// a known slug, leaving Desktop ids such as `anthropic/claude-sonnet-5` alone.
pub const CLAUDE_GATEWAY_MODEL_PREFIX: &str = "anthropic/";

pub fn claude_gateway_routed_model_id(slug: &str, upstream: &str) -> String {
    format!(
        "{CLAUDE_GATEWAY_MODEL_PREFIX}{slug}/{}",
        alias_inner_slashes(upstream)
    )
}

/// If `model` is `anthropic/{known_slug}/...`, return `{known_slug}/...`.
pub fn peel_claude_gateway_prefix<'a>(
    model: &'a str,
    known_slugs: &HashSet<String>,
) -> Option<&'a str> {
    let rest = model.strip_prefix(CLAUDE_GATEWAY_MODEL_PREFIX)?;
    let (slug, after) = rest.split_once('/')?;
    if after.trim().is_empty() {
        return None;
    }
    if known_slugs.contains(&slug.to_ascii_lowercase()) {
        Some(rest)
    } else {
        None
    }
}

/// Anthropic-format model list for Claude Code `GET /v1/models` discovery.
#[cfg(test)]
pub fn build_claude_gateway_model_list(providers: &[Provider]) -> Value {
    build_claude_gateway_model_list_with_combos(providers, &[])
}

pub fn build_claude_gateway_model_list_with_combos(
    providers: &[Provider],
    combos: &[crate::proxy::combo::ModelCombo],
) -> Value {
    let slugs = assign_routing_slugs(providers);
    let mut data = Vec::new();
    let mut seen = HashSet::new();
    for provider in providers {
        if !participates_in_routing_catalog(provider) {
            continue;
        }
        let Some(slug) = slugs.get(&provider.id) else {
            continue;
        };
        for model in provider_upstream_model_ids(provider) {
            let id = claude_gateway_routed_model_id(slug, &model);
            if !seen.insert(id.clone()) {
                continue;
            }
            data.push(json!({
                "type": "model",
                "id": id,
                "display_name": format!("{} / {model}", provider.name),
            }));
        }
    }
    for item in crate::proxy::combo::combo_claude_gateway_items(combos, providers) {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if seen.insert(id.to_string()) {
            data.push(item);
        }
    }
    let first_id = data
        .first()
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let last_id = data
        .last()
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    json!({
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    })
}

pub fn providers_current_first(
    current: &[Provider],
    all: impl IntoIterator<Item = Provider>,
) -> Vec<Provider> {
    let mut providers: Vec<Provider> = Vec::new();
    for provider in current {
        if providers.iter().any(|existing| existing.id == provider.id) {
            continue;
        }
        providers.push(provider.clone());
    }
    for provider in all {
        if providers.iter().any(|existing| existing.id == provider.id) {
            continue;
        }
        providers.push(provider);
    }
    providers
}

/// Parse `slug/model` only when `slug` is one of the assigned routing slugs.
pub fn parse_routed_model<'a>(
    request_model: &'a str,
    known_slugs: &HashSet<String>,
) -> Option<(&'a str, &'a str)> {
    let trimmed = request_model.trim();
    let (slug, rest) = trimmed.split_once('/')?;
    if rest.trim().is_empty() {
        return None;
    }
    let slug_norm = slug.to_ascii_lowercase();
    if known_slugs.contains(&slug_norm) {
        Some((slug, rest.trim()))
    } else {
        None
    }
}

/// Strip `{slug}/` from a request body when it targets this provider.
pub fn strip_routing_prefix_from_body(mut body: Value, provider: &Provider) -> Value {
    let Some(model) = body.get("model").and_then(Value::as_str) else {
        return body;
    };
    let owned = model.to_string();
    let candidate = if let Some(rest) = owned.strip_prefix(CLAUDE_GATEWAY_MODEL_PREFIX) {
        if let Some((slug, after)) = rest.split_once('/') {
            if !after.trim().is_empty() && routing_slug_belongs_to_provider(slug, provider) {
                rest
            } else {
                owned.as_str()
            }
        } else {
            owned.as_str()
        }
    } else {
        owned.as_str()
    };
    let Some((slug, rest)) = candidate.split_once('/') else {
        return body;
    };
    let rest = rest.trim();
    if rest.is_empty() || !routing_slug_belongs_to_provider(slug, provider) {
        return body;
    }
    body["model"] = json!(rest);
    body
}

fn routing_slug_belongs_to_provider(slug: &str, provider: &Provider) -> bool {
    let slug_norm = slug.trim().to_ascii_lowercase();
    if slug_norm.is_empty() {
        return false;
    }
    let preferred = preferred_routing_slug(provider);
    if slug_norm == preferred {
        return true;
    }
    let id_lower = provider.id.trim().to_ascii_lowercase();
    if slug_norm == id_lower {
        return true;
    }
    let suffix = sanitize_id_suffix(&provider.id);
    let collision = format!("{preferred}-{suffix}");
    slug_norm == collision || slug_norm.starts_with(&format!("{collision}-"))
}

/// Decide how to select providers for this request model.
pub fn decide_model_route(providers: &[Provider], request_model: &str) -> ModelRouteDecision {
    let slugs = assign_routing_slugs(providers);
    let known: HashSet<String> = slugs.values().cloned().collect();
    let trimmed = request_model.trim();
    if trimmed.is_empty() || trimmed == "unknown" {
        return ModelRouteDecision::Default;
    }

    let routed_source = peel_claude_gateway_prefix(trimmed, &known).unwrap_or(trimmed);
    if let Some((slug, rest)) = parse_routed_model(routed_source, &known) {
        let slug_norm = slug.to_ascii_lowercase();
        if let Some(provider) = providers.iter().find(|provider| {
            slugs
                .get(&provider.id)
                .is_some_and(|assigned| assigned == &slug_norm)
        }) {
            return ModelRouteDecision::Pinned {
                provider_id: provider.id.clone(),
                upstream_model: rest.to_string(),
            };
        }
    }

    let matches: Vec<&Provider> = providers
        .iter()
        .filter(|provider| {
            provider_upstream_model_ids(provider)
                .iter()
                .any(|model| models_match(model, trimmed))
        })
        .collect();
    if matches.len() == 1 {
        return ModelRouteDecision::Pinned {
            provider_id: matches[0].id.clone(),
            upstream_model: trimmed.to_string(),
        };
    }

    ModelRouteDecision::Default
}

/// Upstream model ids advertised by a card (mapping table + env + toml).
///
/// Last `/v1/models` discovery is **not** included: that cache is only
/// unioned when projecting `cc-switch-model-catalog.json`, so request
/// routing does not read the sidecar file on the hot path.
pub fn provider_upstream_model_ids(provider: &Provider) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |value: &str| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        if seen.insert(trimmed.to_string()) {
            ids.push(trimmed.to_string());
        }
    };

    if let Some(models) = provider
        .settings_config
        .get("modelCatalog")
        .and_then(|catalog| catalog.get("models"))
        .and_then(Value::as_array)
    {
        for entry in models {
            if let Some(model) = entry.get("model").and_then(Value::as_str) {
                push(model);
            }
        }
    }

    if let Some(model) = provider
        .settings_config
        .get("model")
        .and_then(Value::as_str)
    {
        push(model);
    }

    if let Some(env) = provider.settings_config.get("env") {
        for key in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
        ] {
            if let Some(model) = env.get(key).and_then(Value::as_str) {
                push(model);
            }
        }
    }

    if let Some(model) = crate::proxy::providers::codex_provider_upstream_model(provider) {
        push(&model);
    }
    if let Some(config_text) = provider
        .settings_config
        .get("config")
        .and_then(Value::as_str)
    {
        if let Some(model) = extract_toml_model_line(config_text) {
            push(&model);
        }
        for model in extract_toml_advertised_models(config_text) {
            push(&model);
        }
    }

    if let Some(routes) = provider
        .meta
        .as_ref()
        .map(|meta| &meta.claude_desktop_model_routes)
    {
        for route in routes.values() {
            push(&route.model);
        }
    }

    if is_codex_official_provider(provider) {
        for entry in official_native_fallback_entries() {
            if let Some(slug) = entry.get("slug").and_then(Value::as_str) {
                push(slug);
            }
        }
    }

    ids
}

/// Build the merged Codex catalog written during takeover.
#[cfg(test)]
pub fn build_merged_codex_routing_catalog(providers: &[Provider]) -> Result<Value, AppError> {
    build_merged_codex_routing_catalog_with_combos(providers, &[])
}

pub fn build_merged_codex_routing_catalog_with_combos(
    providers: &[Provider],
    combos: &[crate::proxy::combo::ModelCombo],
) -> Result<Value, AppError> {
    let slugs = assign_routing_slugs(providers);
    let discovery = load_routing_discovery_cache();
    let mut models = Vec::new();
    let mut seen_slugs = HashSet::new();
    let mut priority = 0usize;

    for provider in providers {
        if !participates_in_routing_catalog(provider) {
            continue;
        }
        let Some(slug) = slugs.get(&provider.id) else {
            continue;
        };
        let keep_unprefixed = is_codex_official_provider(provider);
        let profile = resolve_codex_catalog_tool_profile(provider);
        let config_text = provider
            .settings_config
            .get("config")
            .and_then(Value::as_str)
            .unwrap_or("");

        let built = crate::codex_config::codex_model_catalog_from_settings(
            &provider.settings_config,
            config_text,
            profile,
        )?;

        let mut entries = match built {
            Some(catalog) => catalog
                .get("models")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            None if keep_unprefixed => official_native_fallback_entries(),
            None => Vec::new(),
        };
        if entries.is_empty() {
            entries = catalog_entries_from_provider(
                provider,
                profile,
                discovery.get(&provider.id).map(Vec::as_slice),
            );
        } else {
            let have: HashSet<String> = entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("slug")
                        .and_then(Value::as_str)
                        .map(|slug| slug.trim().to_string())
                })
                .filter(|slug| !slug.is_empty())
                .collect();
            let extra: Vec<String> = advertised_model_ids_for_catalog(
                provider,
                discovery.get(&provider.id).map(Vec::as_slice),
            )
            .into_iter()
            .filter(|id| !have.contains(id.as_str()) && !have.contains(&alias_inner_slashes(id)))
            .collect();
            if !extra.is_empty() {
                entries.extend(catalog_entries_from_ids(&extra, profile));
            }
        }

        for mut entry in entries {
            let original_slug = entry
                .get("slug")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("")
                .to_string();
            if original_slug.is_empty() {
                continue;
            }

            if keep_unprefixed {
                push_catalog_entry(&mut models, &mut seen_slugs, &mut priority, entry.clone());
            }

            let aliased = alias_inner_slashes(&original_slug);
            let routed_slug = format!("{slug}/{aliased}");
            if let Some(obj) = entry.as_object_mut() {
                let display = obj
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or(&original_slug);
                let display = format!("{} / {display}", provider.name);
                obj.insert("slug".to_string(), json!(routed_slug));
                obj.insert("display_name".to_string(), json!(display));
                obj.insert("description".to_string(), json!(display));
            }
            push_catalog_entry(&mut models, &mut seen_slugs, &mut priority, entry);
        }
    }

    append_combo_catalog_entries(
        &mut models,
        &mut seen_slugs,
        &mut priority,
        providers,
        combos,
    );

    Ok(json!({ "models": models }))
}

/// Overwrite `cc-switch-model-catalog.json` with the merged routing catalog.
#[cfg(test)]
pub fn write_merged_codex_routing_catalog(providers: &[Provider]) -> Result<(), AppError> {
    write_merged_codex_routing_catalog_with_combos(providers, &[])
}

pub fn write_merged_codex_routing_catalog_with_combos(
    providers: &[Provider],
    combos: &[crate::proxy::combo::ModelCombo],
) -> Result<(), AppError> {
    let catalog = build_merged_codex_routing_catalog_with_combos(providers, combos)?;
    let path = get_codex_model_catalog_path();
    write_json_file(&path, &catalog)?;
    let cache_path = get_codex_config_dir().join("models_cache.json");
    let _ = std::fs::remove_file(cache_path);
    Ok(())
}

fn append_combo_catalog_entries(
    models: &mut Vec<Value>,
    seen_slugs: &mut HashSet<String>,
    priority: &mut usize,
    providers: &[Provider],
    combos: &[crate::proxy::combo::ModelCombo],
) {
    let slugs = assign_routing_slugs(providers);
    for combo in combos {
        let resolved = crate::proxy::combo::resolve_combo_targets(combo, providers);
        let template = resolved.first().and_then(|first| {
            let provider_slug = slugs.get(&first.provider.id)?;
            let want = format!(
                "{provider_slug}/{}",
                alias_inner_slashes(&first.upstream_model)
            );
            models
                .iter()
                .find(|entry| entry.get("slug").and_then(Value::as_str) == Some(want.as_str()))
        });
        if let Some(entry) = crate::proxy::combo::combo_catalog_entry(combo, providers, template) {
            push_catalog_entry(models, seen_slugs, priority, entry);
        }
    }
}

fn push_catalog_entry(
    models: &mut Vec<Value>,
    seen_slugs: &mut HashSet<String>,
    priority: &mut usize,
    mut entry: Value,
) {
    let slug = entry
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if slug.is_empty() || !seen_slugs.insert(slug) {
        return;
    }
    if let Some(obj) = entry.as_object_mut() {
        obj.insert("priority".to_string(), json!(1000 + *priority));
    }
    *priority += 1;
    models.push(entry);
}

fn advertised_model_ids_for_catalog(
    provider: &Provider,
    discovered: Option<&[String]>,
) -> Vec<String> {
    let mut ids = provider_upstream_model_ids(provider);
    let mut seen: HashSet<String> = ids.iter().cloned().collect();
    if let Some(extra) = discovered {
        for id in extra {
            let trimmed = id.trim();
            if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
                continue;
            }
            ids.push(trimmed.to_string());
        }
    }
    ids
}

fn catalog_entries_from_ids(ids: &[String], profile: CodexCatalogToolProfile) -> Vec<Value> {
    if ids.is_empty() {
        return Vec::new();
    }
    let models: Vec<Value> = ids.iter().map(|model| json!({ "model": model })).collect();
    let settings = json!({ "modelCatalog": { "models": models } });
    crate::codex_config::codex_model_catalog_from_settings(&settings, "", profile)
        .ok()
        .flatten()
        .and_then(|catalog| catalog.get("models").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn catalog_entries_from_provider(
    provider: &Provider,
    profile: CodexCatalogToolProfile,
    discovered: Option<&[String]>,
) -> Vec<Value> {
    catalog_entries_from_ids(
        &advertised_model_ids_for_catalog(provider, discovered),
        profile,
    )
}

fn official_native_fallback_entries() -> Vec<Value> {
    let models: Vec<Value> = OFFICIAL_CATALOG_SEED
        .iter()
        .map(|model| json!({ "model": *model }))
        .collect();
    let settings = json!({ "modelCatalog": { "models": models } });
    crate::codex_config::codex_model_catalog_from_settings(
        &settings,
        "",
        CodexCatalogToolProfile::NativeResponses,
    )
    .ok()
    .flatten()
    .and_then(|catalog| catalog.get("models").and_then(Value::as_array).cloned())
    .unwrap_or_else(|| {
        OFFICIAL_CATALOG_SEED
            .iter()
            .map(|slug| {
                json!({
                    "slug": slug,
                    "display_name": slug,
                    "description": slug,
                    "priority": 1000
                })
            })
            .collect()
    })
}

fn extract_toml_model_line(config_text: &str) -> Option<String> {
    for line in config_text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("model") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let value = rest.trim().trim_matches('"').trim_matches('\'').trim();
                if !value.is_empty() && !value.starts_with('[') {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn extract_toml_advertised_models(config_text: &str) -> Vec<String> {
    let Ok(value) = config_text.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(providers) = value.get("model_providers").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for table in providers.values() {
        if let Some(models) = table.get("models") {
            collect_toml_model_ids(models, &mut out, &mut seen);
        }
    }
    out
}

fn collect_toml_model_ids(value: &toml::Value, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    match value {
        toml::Value::String(id) => {
            let trimmed = id.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                out.push(trimmed.to_string());
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                collect_toml_model_ids(item, out, seen);
            }
        }
        toml::Value::Table(table) => {
            for key in ["model", "slug", "id"] {
                if let Some(id) = table.get(key).and_then(toml::Value::as_str) {
                    let trimmed = id.trim();
                    if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                        out.push(trimmed.to_string());
                    }
                }
            }
        }
        _ => {}
    }
}

fn slugify(raw: &str) -> String {
    let mut out = String::new();
    let mut last_hyphen = false;
    for ch in raw.chars() {
        let mapped = match ch {
            'A'..='Z' => ch.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '.' | '_' => ch,
            _ => '-',
        };
        if mapped == '-' {
            if !out.is_empty() && !last_hyphen {
                out.push('-');
                last_hyphen = true;
            }
        } else {
            out.push(mapped);
            last_hyphen = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_ascii_alphanumeric())
    {
        out.insert(0, 'p');
    }
    if out.is_empty() {
        "provider".to_string()
    } else {
        out
    }
}

fn is_valid_slug(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn is_uuid_like(value: &str) -> bool {
    let value = value.trim();
    value.len() == 36
        && value.as_bytes().get(8) == Some(&b'-')
        && value.as_bytes().get(13) == Some(&b'-')
        && value.as_bytes().get(18) == Some(&b'-')
        && value.as_bytes().get(23) == Some(&b'-')
        && value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn sanitize_id_suffix(id: &str) -> String {
    let compact: String = id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if compact.is_empty() {
        "id".to_string()
    } else {
        compact.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AuthBinding, AuthBindingSource, ProviderMeta};
    use serde_json::json;
    use serial_test::serial;

    fn provider(id: &str, name: &str, catalog_models: &[&str]) -> Provider {
        let models: Vec<Value> = catalog_models
            .iter()
            .map(|model| json!({ "model": *model }))
            .collect();
        Provider::with_id(
            id.to_string(),
            name.to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "sk-test" },
                "config": "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://example.com/v1\"\nwire_api = \"chat\"\n",
                "modelCatalog": { "models": models }
            }),
            None,
        )
    }

    fn official(id: &str) -> Provider {
        let mut p = Provider::with_id(
            id.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        p.category = Some("official".to_string());
        p.meta = Some(ProviderMeta {
            provider_type: Some("codex_oauth".to_string()),
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct".to_string()),
            }),
            ..Default::default()
        });
        p
    }

    #[test]
    fn slugify_name_and_uuid_id() {
        let p = provider(
            "2c0f1a6e-9b11-4d22-8c33-abcdef123456",
            "Kimi Coding",
            &["k2"],
        );
        assert_eq!(preferred_routing_slug(&p), "kimi-coding");
        assert_eq!(
            normalize_routing_slug("Kimi Coding").as_deref(),
            Some("kimi-coding")
        );
        assert_eq!(normalize_routing_slug("   "), None);
    }

    #[test]
    fn stable_id_used_when_slug_safe() {
        let p = provider("deepseek", "DeepSeek", &["deepseek-v4"]);
        assert_eq!(preferred_routing_slug(&p), "deepseek");
    }

    #[test]
    fn parse_only_known_slugs() {
        let known = HashSet::from(["kimi".to_string()]);
        assert_eq!(parse_routed_model("kimi/k2", &known), Some(("kimi", "k2")));
        assert_eq!(
            parse_routed_model("anthropic/claude-sonnet-5", &known),
            None
        );
        assert_eq!(parse_routed_model("kimi/", &known), None);
        assert_eq!(
            parse_routed_model("kimi/org/model", &known),
            Some(("kimi", "org/model"))
        );
    }

    #[test]
    fn pin_by_prefix_and_unique_unprefixed() {
        let kimi = provider("kimi", "Kimi", &["k2", "org/model"]);
        let ds = provider("deepseek", "DeepSeek", &["deepseek-v4"]);
        let providers = vec![kimi, ds];

        match decide_model_route(&providers, "kimi/k2") {
            ModelRouteDecision::Pinned {
                provider_id,
                upstream_model,
            } => {
                assert_eq!(provider_id, "kimi");
                assert_eq!(upstream_model, "k2");
            }
            other => panic!("expected pin, got {other:?}"),
        }

        match decide_model_route(&providers, "deepseek-v4") {
            ModelRouteDecision::Pinned { provider_id, .. } => {
                assert_eq!(provider_id, "deepseek");
            }
            other => panic!("expected unique pin, got {other:?}"),
        }

        assert_eq!(
            decide_model_route(&providers, "claude-sonnet-4-6"),
            ModelRouteDecision::Default
        );
        assert_eq!(
            decide_model_route(&providers, "anthropic/claude-sonnet-5"),
            ModelRouteDecision::Default
        );

        match decide_model_route(&providers, "anthropic/kimi/k2") {
            ModelRouteDecision::Pinned {
                provider_id,
                upstream_model,
            } => {
                assert_eq!(provider_id, "kimi");
                assert_eq!(upstream_model, "k2");
            }
            other => panic!("expected gateway-prefixed pin, got {other:?}"),
        }
        match decide_model_route(&providers, "anthropic/deepseek/deepseek-v4") {
            ModelRouteDecision::Pinned { provider_id, .. } => {
                assert_eq!(provider_id, "deepseek");
            }
            other => panic!("expected gateway-prefixed pin, got {other:?}"),
        }
    }

    #[test]
    fn inner_slash_alias_matches() {
        let kimi = provider("kimi", "Kimi", &["org/model"]);
        match decide_model_route(&[kimi], "org-model") {
            ModelRouteDecision::Pinned { provider_id, .. } => assert_eq!(provider_id, "kimi"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn strip_claude_gateway_prefix_only_for_known_slug() {
        let p = provider("kimi", "Kimi", &["k2"]);
        let stripped = strip_routing_prefix_from_body(json!({ "model": "anthropic/kimi/k2" }), &p);
        assert_eq!(stripped["model"], "k2");
        let desktop =
            strip_routing_prefix_from_body(json!({ "model": "anthropic/claude-sonnet-5" }), &p);
        assert_eq!(desktop["model"], "anthropic/claude-sonnet-5");
    }

    #[test]
    fn claude_gateway_list_uses_anthropic_prefixed_ids() {
        let kimi = provider("kimi", "Kimi", &["k2"]);
        let ds = provider("deepseek", "DeepSeek", &["deepseek-v4"]);
        let list = build_claude_gateway_model_list(&[kimi, ds]);
        let ids: Vec<&str> = list
            .get("data")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| entry.get("id").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        assert!(ids.contains(&"anthropic/kimi/k2"), "{ids:?}");
        assert!(ids.contains(&"anthropic/deepseek/deepseek-v4"), "{ids:?}");
        assert!(ids.iter().all(|id| id.starts_with("anthropic/")), "{ids:?}");
    }

    #[test]
    fn opt_out_of_catalog() {
        let mut p = provider("kimi", "Kimi", &["k2"]);
        p.meta = Some(ProviderMeta {
            routing_catalog: Some(false),
            ..Default::default()
        });
        assert!(!participates_in_routing_catalog(&p));
    }

    #[test]
    fn set_routing_catalog_enabled_preserves_other_meta() {
        let mut p = provider("kimi", "Kimi", &["k2"]);
        p.meta = Some(ProviderMeta {
            routing_slug: Some("kimi".into()),
            routing_catalog: Some(false),
            ..Default::default()
        });
        set_routing_catalog_enabled(&mut p, true);
        assert!(participates_in_routing_catalog(&p));
        assert_eq!(
            p.meta
                .as_ref()
                .and_then(|meta| meta.routing_slug.as_deref()),
            Some("kimi")
        );
        set_routing_catalog_enabled(&mut p, false);
        assert!(!participates_in_routing_catalog(&p));
    }

    #[test]
    fn strip_prefix_is_case_insensitive_on_slug() {
        let p = provider("kimi", "Kimi", &["k2"]);
        let body = json!({ "model": "Kimi/k2" });
        let stripped = strip_routing_prefix_from_body(body, &p);
        assert_eq!(stripped["model"], "k2");
    }

    #[test]
    fn collision_suffix() {
        let a = provider("id-aaaa1111", "Same Name", &["a"]);
        let b = provider("id-bbbb2222", "Same Name", &["b"]);
        let map = assign_routing_slugs(&[a, b]);
        let slugs: HashSet<_> = map.values().cloned().collect();
        assert_eq!(slugs.len(), 2);
    }

    #[test]
    fn official_participates_without_catalog() {
        assert!(participates_in_routing_catalog(&official("codex-official")));
    }

    fn with_test_home<T>(f: impl FnOnce() -> T) -> T {
        let dir = tempfile::TempDir::new().expect("temp home");
        let original = std::env::var("CC_SWITCH_TEST_HOME").ok();
        std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
        let result = f();
        match original {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
        result
    }

    #[test]
    #[serial]
    fn write_merged_catalog_invalidates_models_cache() {
        with_test_home(|| {
            let cache = get_codex_config_dir().join("models_cache.json");
            std::fs::create_dir_all(cache.parent().unwrap()).expect("codex dir");
            std::fs::write(
                &cache,
                serde_json::to_string(&json!({
                    "models": [{
                        "slug": "gpt-5.5",
                        "display_name": "GPT-5.5",
                        "model_messages": { "instructions_template": "t" },
                        "additional_speed_tiers": [],
                        "context_window": 128000
                    }]
                }))
                .expect("serialize cache"),
            )
            .expect("seed cache");
            write_merged_codex_routing_catalog(&[provider("kimi", "Kimi", &["k2"])])
                .expect("write merged catalog");
            assert!(
                !cache.exists(),
                "Codex models_cache.json should be dropped after catalog rewrite"
            );
            let text =
                std::fs::read_to_string(get_codex_model_catalog_path()).expect("read catalog");
            assert!(text.contains("kimi/k2"), "{text}");
        });
    }

    #[test]
    fn collision_assigned_slug_is_stripped() {
        let a = provider("id-aaaa1111", "Same Name", &["a"]);
        let b = provider("id-bbbb2222", "Same Name", &["b"]);
        let map = assign_routing_slugs(&[a.clone(), b.clone()]);
        let slug_b = map.get("id-bbbb2222").expect("assigned slug");
        assert_ne!(slug_b, "same-name");
        let stripped =
            strip_routing_prefix_from_body(json!({ "model": format!("{slug_b}/b") }), &b);
        assert_eq!(stripped["model"], "b");
        match decide_model_route(&[a, b], &format!("{slug_b}/b")) {
            ModelRouteDecision::Pinned { provider_id, .. } => {
                assert_eq!(provider_id, "id-bbbb2222");
            }
            other => panic!("expected pin by collision slug, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn merged_catalog_prefixes_and_keeps_per_card_tool_profile() {
        with_test_home(|| {
            let mut chat = provider("kimi", "Kimi", &["k2", "org/model"]);
            chat.meta = Some(ProviderMeta {
                api_format: Some("openai_chat".to_string()),
                ..Default::default()
            });
            let mut anthropic = provider("anthropic-gw", "Anthropic GW", &["claude-sonnet-4-6"]);
            anthropic.meta = Some(ProviderMeta {
                api_format: Some("anthropic".to_string()),
                ..Default::default()
            });
            let mut skipped = provider("hidden", "Hidden", &["secret-model"]);
            skipped.meta = Some(ProviderMeta {
                routing_catalog: Some(false),
                ..Default::default()
            });
            let official = official("codex-official");

            let catalog = build_merged_codex_routing_catalog(&[chat, anthropic, skipped, official])
                .expect("merged catalog");
            let models = catalog["models"].as_array().expect("models");
            let slugs: Vec<&str> = models
                .iter()
                .filter_map(|entry| entry.get("slug").and_then(Value::as_str))
                .collect();

            assert!(slugs.contains(&"kimi/k2"), "{slugs:?}");
            assert!(slugs.contains(&"kimi/org-model"), "{slugs:?}");
            assert!(
                slugs.contains(&"anthropic-gw/claude-sonnet-4-6"),
                "{slugs:?}"
            );
            assert!(
                slugs
                    .iter()
                    .any(|slug| *slug == "gpt-5.5" || slug.starts_with("codex-official/")),
                "official native rows should remain selectable: {slugs:?}"
            );
            assert!(
                !slugs.iter().any(|slug| slug.contains("secret-model")),
                "opted-out card must not appear: {slugs:?}"
            );

            let kimi_k2 = models
                .iter()
                .find(|entry| entry.get("slug").and_then(Value::as_str) == Some("kimi/k2"))
                .expect("kimi/k2");
            let anthropic_row = models
                .iter()
                .find(|entry| {
                    entry.get("slug").and_then(Value::as_str)
                        == Some("anthropic-gw/claude-sonnet-4-6")
                })
                .expect("anthropic routed row");
            assert!(
                kimi_k2.get("tools").is_some() || kimi_k2.get("model_messages").is_some(),
                "ProxyChat card should keep the chat tool template: {kimi_k2}"
            );
            assert!(
                anthropic_row.get("tools").is_none(),
                "Anthropic card must not share the chat apply_patch tools: {anthropic_row}"
            );
        });
    }

    #[test]
    fn merged_catalog_includes_combo_slug() {
        let kimi = provider("kimi", "Kimi", &["k2"]);
        let ds = provider("deepseek", "DeepSeek", &["deepseek-v4"]);
        let combo = crate::proxy::combo::ModelCombo {
            id: "main".into(),
            targets: vec![
                crate::proxy::combo::parse_combo_target_spec("kimi/k2").unwrap(),
                crate::proxy::combo::parse_combo_target_spec("deepseek/deepseek-v4").unwrap(),
            ],
            strategy: crate::proxy::combo::ComboStrategy::Failover,
            sticky_limit: 1,
        };
        let catalog = build_merged_codex_routing_catalog_with_combos(
            &[kimi.clone(), ds.clone()],
            &[combo.clone()],
        )
        .expect("catalog");
        let slugs: Vec<&str> = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry.get("slug").and_then(Value::as_str))
            .collect();
        assert!(slugs.contains(&"combo/main"), "{slugs:?}");

        let list = build_claude_gateway_model_list_with_combos(&[kimi, ds], &[combo]);
        let ids: Vec<&str> = list["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .collect();
        assert!(ids.contains(&"anthropic/combo/main"), "{ids:?}");
    }

    fn catalog_slugs(providers: &[Provider]) -> Vec<String> {
        let catalog = build_merged_codex_routing_catalog(providers).expect("merged catalog");
        catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| {
                entry
                    .get("slug")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    #[test]
    fn toml_models_array_is_unioned_with_mapping_table() {
        let mut p = provider("packy", "Packy", &["gpt-5.6-sol"]);
        p.settings_config["config"] = json!(
            "model_provider = \"custom\"\nmodel = \"gpt-5.6-sol\"\n[model_providers.custom]\nbase_url = \"https://example.com/v1\"\nmodels = [\"gpt-5.4\", { model = \"kimi-k2\" }]\n"
        );
        let slugs = catalog_slugs(&[p]);
        assert!(
            slugs.contains(&"packy/gpt-5.6-sol".to_string()),
            "{slugs:?}"
        );
        assert!(slugs.contains(&"packy/gpt-5.4".to_string()), "{slugs:?}");
        assert!(slugs.contains(&"packy/kimi-k2".to_string()), "{slugs:?}");
    }

    #[test]
    fn official_seed_includes_current_gpt_family() {
        let slugs = catalog_slugs(&[official("codex-official")]);
        for expected in [
            "gpt-5.5",
            "gpt-5.6",
            "gpt-5.6-sol",
            "codex-official/gpt-5.5",
        ] {
            assert!(
                slugs.iter().any(|slug| slug == expected),
                "missing {expected} in {slugs:?}"
            );
        }
    }

    #[test]
    #[serial]
    fn discovery_cache_is_unioned_into_merged_catalog() {
        with_test_home(|| {
            let mut cache = HashMap::new();
            cache.insert(
                "grok".to_string(),
                vec!["grok-4.5".into(), "grok-4.6".into()],
            );
            save_routing_discovery_cache(&cache);
            let slugs = catalog_slugs(&[provider("grok", "Grok", &["grok-4.5"])]);
            assert!(slugs.contains(&"grok/grok-4.5".to_string()), "{slugs:?}");
            assert!(slugs.contains(&"grok/grok-4.6".to_string()), "{slugs:?}");
        });
    }
}
