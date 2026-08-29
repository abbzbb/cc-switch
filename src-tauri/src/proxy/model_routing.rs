//! OpenCodex-style `provider/model` routing.
//!
//! Request `model` values whose first path segment matches a provider routing
//! slug pin that card (no cross-provider failover). Catalog injection writes
//! the same ids so Codex's model picker can target any configured card.

use crate::{
    codex_config::{
        ensure_cc_switch_model_catalog_pointer, get_codex_config_dir, get_codex_model_catalog_path,
        inferred_catalog_context_window, CodexCatalogToolProfile,
        CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME,
    },
    config::write_json_file,
    error::AppError,
    provider::Provider,
    proxy::providers::{
        codex_provider_upstream_model, is_codex_official_provider,
        resolve_codex_catalog_tool_profile,
    },
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};
use toml_edit::DocumentMut;

/// Cap on models cached from one card's upstream `/v1/models`.
pub const MAX_DISCOVERED_MODELS_PER_CARD: usize = 200;

const ROUTING_DISCOVERY_FILENAME: &str = "cc-switch-model-discovery.json";

/// Official Codex models the picker should keep even when the card only
/// stored a single current `model =`. Live ChatGPT `/models` is not mirrored
/// here; this is a local seed so unprefixed `gpt-*` rows do not collapse to
/// just `gpt-5.5`. Unprefixed copies are omitted from the merged picker when
/// a non-Official card also participates, so Codex does not treat
/// `gpt-5.6-sol` as a sibling of `grok/grok-4.6`. Typed unprefixed official
/// ids still unique-pin Official.
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

fn discovery_cache_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

fn load_routing_discovery_cache_unlocked() -> HashMap<String, Vec<String>> {
    let path = routing_discovery_cache_path();
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_routing_discovery_cache_unlocked(cache: &HashMap<String, Vec<String>>) {
    let _ = write_json_file(&routing_discovery_cache_path(), cache);
}

pub fn load_routing_discovery_cache() -> HashMap<String, Vec<String>> {
    let _guard = discovery_cache_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    load_routing_discovery_cache_unlocked()
}

#[cfg(test)]
pub fn save_routing_discovery_cache(cache: &HashMap<String, Vec<String>>) {
    let _guard = discovery_cache_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    save_routing_discovery_cache_unlocked(cache);
}

/// Load-modify-save under one lock so concurrent refreshes cannot drop inserts.
pub fn mutate_routing_discovery_cache(update: impl FnOnce(&mut HashMap<String, Vec<String>>)) {
    let _guard = discovery_cache_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut cache = load_routing_discovery_cache_unlocked();
    update(&mut cache);
    save_routing_discovery_cache_unlocked(&cache);
}

pub fn drop_routing_discovery_cache_entry(provider_id: &str) {
    let id = provider_id.trim();
    if id.is_empty() {
        return;
    }
    mutate_routing_discovery_cache(|cache| {
        cache.remove(id);
    });
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

fn compare_providers_for_assign(left: &Provider, right: &Provider) -> std::cmp::Ordering {
    left.sort_index
        .unwrap_or(usize::MAX)
        .cmp(&right.sort_index.unwrap_or(usize::MAX))
        .then_with(|| {
            left.created_at
                .unwrap_or(0)
                .cmp(&right.created_at.unwrap_or(0))
        })
        .then_with(|| left.id.cmp(&right.id))
}

/// Assign unique slugs across a provider set. Input order is ignored: cards
/// are sorted like the provider DAO / TS assigner (`sort_index`, `created_at`,
/// `id`). Explicit `routingSlug` wins; collisions get a sanitized id suffix.
pub fn assign_routing_slugs(providers: &[Provider]) -> HashMap<String, String> {
    let mut ordered: Vec<&Provider> = providers.iter().collect();
    ordered.sort_by(|left, right| compare_providers_for_assign(left, right));

    let mut used: HashSet<String> = HashSet::new();
    let mut assigned = HashMap::new();

    let mut with_override = Vec::new();
    let mut without_override = Vec::new();
    for provider in ordered {
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

/// Generic provider updates must not overwrite catalog membership.
/// `set_provider_routing_catalog` is the only writer for this flag.
pub fn preserve_routing_catalog(existing: &Provider, incoming: &mut Provider) {
    let existing_flag = existing.meta.as_ref().and_then(|meta| meta.routing_catalog);
    let mut meta = incoming.meta.take().unwrap_or_default();
    meta.routing_catalog = existing_flag;
    incoming.meta = Some(meta);
}

/// Alias inner `/` in an upstream model id to `-` for catalog slugs.
pub fn alias_inner_slashes(model: &str) -> String {
    model.replace('/', "-")
}

/// Flattened leftover copies of `{sibling_slug}/{model}` (`default-gpt-5.6-sol`).
/// Native ids that merely share a hyphen prefix with a sibling (`kimi-k2` on
/// a `packy` card) are not included.
fn flattened_namespace_copies(
    providers: &[Provider],
    slugs: &HashMap<String, String>,
) -> HashMap<String, HashSet<String>> {
    let mut by_slug: HashMap<String, HashSet<String>> = HashMap::new();
    for provider in providers {
        let Some(slug) = slugs.get(&provider.id) else {
            continue;
        };
        let slug_l = slug.to_ascii_lowercase();
        let entry = by_slug.entry(slug_l.clone()).or_default();
        let mut push = |model: &str| {
            let aliased = alias_inner_slashes(model.trim());
            if aliased.is_empty() {
                return;
            }
            entry.insert(format!("{slug_l}-{aliased}"));
        };
        for id in provider_upstream_model_ids(provider) {
            push(&id);
        }
        if is_codex_official_provider(provider) {
            for seed in OFFICIAL_CATALOG_SEED {
                push(seed);
            }
        }
    }
    by_slug
}

fn foreign_flattened_ids(
    this_slug: &str,
    copies_by_slug: &HashMap<String, HashSet<String>>,
) -> HashSet<String> {
    let this = this_slug.trim().to_ascii_lowercase();
    let mut out = HashSet::new();
    for (slug, copies) in copies_by_slug {
        if slug != &this {
            out.extend(copies.iter().cloned());
        }
    }
    out
}

/// Bare upstream id to advertise under `this_slug`, or `None` if `raw` belongs
/// to another routing namespace.
///
/// A second catalog pass used to treat `grok/grok-4.6` as a new model id,
/// flatten the slash, and prefix again (`grok/grok-grok-4.6`). The same pass
/// copied `default/gpt-5.6-sol` into the Grok namespace as
/// `grok/default-gpt-5.6-sol`. Peel this card's own prefix (including a
/// doubled leftover) and drop slugs that already belong to a sibling card.
pub(crate) fn bare_catalog_model_id(
    raw: &str,
    this_slug: &str,
    known_slugs: &HashSet<String>,
    foreign_flattened: &HashSet<String>,
) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let this = this_slug.trim().to_ascii_lowercase();
    if this.is_empty() {
        return None;
    }

    if let Some((prefix, rest)) = raw.split_once('/') {
        let prefix = prefix.trim().to_ascii_lowercase();
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        if prefix == this {
            return bare_catalog_model_id(rest, this_slug, known_slugs, foreign_flattened);
        }
        if known_slugs
            .iter()
            .any(|slug| slug.eq_ignore_ascii_case(&prefix))
        {
            return None;
        }
        return Some(alias_inner_slashes(raw));
    }

    if foreign_flattened.contains(&raw.to_ascii_lowercase()) {
        return None;
    }

    let doubled = format!("{this}-{this}-");
    let peeled = if raw.len() > doubled.len()
        && raw.is_char_boundary(doubled.len())
        && raw[..doubled.len()].eq_ignore_ascii_case(&doubled)
        && raw.is_char_boundary(this.len() + 1)
    {
        &raw[this.len() + 1..]
    } else {
        raw
    };
    if peeled.is_empty() {
        return None;
    }
    if foreign_flattened.contains(&peeled.to_ascii_lowercase()) {
        return None;
    }
    Some(alias_inner_slashes(peeled))
}

fn catalog_display_name(provider_name: &str, display: &str, bare: &str) -> String {
    let name = provider_name.trim();
    let mut display = display.trim().to_string();
    loop {
        let prefix = format!("{name} / ");
        if display.len() >= prefix.len()
            && display.is_char_boundary(prefix.len())
            && display[..prefix.len()].eq_ignore_ascii_case(&prefix)
        {
            display = display[prefix.len()..].trim().to_string();
            continue;
        }
        break;
    }
    if display.is_empty() || display.eq_ignore_ascii_case(bare) {
        format!("{name} / {bare}")
    } else {
        format!("{name} / {display}")
    }
}

/// Discovery/`/v1/models` extras that Codex should not offer as coding models.
fn is_non_coding_discovery_model(id: &str) -> bool {
    let id = id
        .rsplit('/')
        .next()
        .unwrap_or(id)
        .trim()
        .replace('_', "-")
        .to_ascii_lowercase();
    id.starts_with("gpt-image-")
        || id.starts_with("dall-e")
        || id.starts_with("tts-")
        || id.starts_with("whisper-")
        || id.starts_with("sora-")
        || id.contains("-audio-")
        || id.contains("-realtime-")
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
    body["model"] = json!(resolve_advertised_upstream(provider, rest));
    body
}

/// Map a catalog alias (`org-model`) back to the card's advertised id (`org/model`).
pub fn resolve_advertised_upstream(provider: &Provider, requested: &str) -> String {
    let requested = requested.trim();
    if requested.is_empty() {
        return requested.to_string();
    }
    let ids = provider_upstream_model_ids(provider);
    if let Some(id) = ids.iter().find(|id| id.eq_ignore_ascii_case(requested)) {
        return id.clone();
    }
    if let Some(id) = ids.iter().find(|id| models_match(id, requested)) {
        return id.clone();
    }
    requested.to_string()
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
                upstream_model: resolve_advertised_upstream(provider, rest),
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
            upstream_model: resolve_advertised_upstream(matches[0], trimmed),
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
    let known_slugs: HashSet<String> = slugs
        .values()
        .map(|slug| slug.to_ascii_lowercase())
        .collect();
    let copies_by_slug = flattened_namespace_copies(providers, &slugs);
    let discovery = load_routing_discovery_cache();
    let hide_unprefixed_official = catalog_has_non_official_participant(providers);
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
        let foreign_flattened = foreign_flattened_ids(slug, &copies_by_slug);
        let is_official = is_codex_official_provider(provider);
        let keep_unprefixed = is_official && !hide_unprefixed_official;
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
            None if is_official => official_native_fallback_entries(),
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
                .filter_map(|entry| entry.get("slug").and_then(Value::as_str))
                .filter_map(|id| bare_catalog_model_id(id, slug, &known_slugs, &foreign_flattened))
                .flat_map(|id| [id.clone(), alias_inner_slashes(&id)])
                .collect();
            let extra: Vec<String> = advertised_model_ids_for_catalog(
                provider,
                discovery.get(&provider.id).map(Vec::as_slice),
            )
            .into_iter()
            .filter_map(|id| bare_catalog_model_id(&id, slug, &known_slugs, &foreign_flattened))
            .filter(|id| !have.contains(id) && !have.contains(&alias_inner_slashes(id)))
            .filter(|id| !is_non_coding_discovery_model(id))
            .collect();
            if !extra.is_empty() {
                entries.extend(catalog_entries_from_ids(&extra, profile, config_text));
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
            let Some(bare) =
                bare_catalog_model_id(&original_slug, slug, &known_slugs, &foreign_flattened)
            else {
                continue;
            };

            if let Some(obj) = entry.as_object_mut() {
                obj.insert("slug".to_string(), json!(bare.clone()));
            }

            if keep_unprefixed {
                push_catalog_entry(&mut models, &mut seen_slugs, &mut priority, entry.clone());
            }

            let aliased = alias_inner_slashes(&bare);
            let routed_slug = format!("{slug}/{aliased}");
            if let Some(obj) = entry.as_object_mut() {
                let display = obj
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or(bare.as_str())
                    .to_string();
                let display = catalog_display_name(&provider.name, &display, &bare);
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

fn catalog_has_non_official_participant(providers: &[Provider]) -> bool {
    providers.iter().any(|provider| {
        participates_in_routing_catalog(provider) && !is_codex_official_provider(provider)
    })
}

/// Display name Codex uses for `supports_remote_compaction()` (`name == "OpenAI"`
/// or Azure). A mixed Official+Grok picker shares one live provider, so that
/// gate must be off or Codex posts `/responses/compact` to Grok/xAI.
const REMOTE_COMPACT_PROVIDER_NAMES: &[&str] = &["OpenAI", "Azure"];
const LOCAL_COMPACT_PROVIDER_NAME: &str = "CC Switch";

/// Rewrite the live takeover `config.toml` so Codex compact/default-model
/// paths stay consistent with the merged routing catalog.
///
/// - When `current_provider_id` is set, write that card's prefixed default
///   (`grok/grok-4.6`) instead of leaving a leftover union-catalog first
///   row (`default/gpt-5.6-sol`). A live id already on that card is kept.
/// - Namespace a unique unprefixed Official `model` (`gpt-5.6-sol` →
///   `{slug}/gpt-5.6-sol`) and drop an unprefixed `review_model`. Already
///   namespaced ids are left alone so a session pick of `grok/grok-4.6` is
///   not clobbered on catalog refresh when no current card is supplied.
/// - Restore `model_catalog_json` when oh-my-codex/`omx setup` deleted it.
/// - Strip inline `[model_providers.*].models` dumps so Official 258k rows
///   cannot sit beside `grok/grok-4.6` after the catalog pointer is gone.
/// - Rename `name = "OpenAI"` (or Azure) to `CC Switch` when a third-party
///   card participates, so Codex uses local compact instead of posting
///   `/responses/compact` to an upstream that does not implement it.
/// - Drop an undersized top-level `model_context_window` /
///   `model_auto_compact_token_limit` so Grok is not compacted at 120k.
pub fn rewrite_live_codex_toml_for_shared_catalog(
    providers: &[Provider],
    current_provider_id: Option<&str>,
) -> Result<(), AppError> {
    crate::codex_config::with_live_codex_toml_lock(|| {
        let config = crate::codex_config::read_codex_config_text()?;
        if config.trim().is_empty() {
            return Ok(());
        }
        let next =
            rewrite_codex_toml_text_for_routing_takeover(&config, providers, current_provider_id)?;
        if next == config {
            return Ok(());
        }
        crate::codex_config::write_codex_live_config_atomic_unlocked(Some(&next))
    })
}

fn rewrite_codex_toml_text_for_routing_takeover(
    config: &str,
    providers: &[Provider],
    current_provider_id: Option<&str>,
) -> Result<String, AppError> {
    let mut next = config.to_string();
    if !catalog_pointer_is_cc_switch(&next) {
        next = ensure_cc_switch_model_catalog_pointer(&next)?;
    }

    let current_model = extract_toml_string_field(&next, "model");
    let review_model = extract_toml_string_field(&next, "review_model");
    let rewrite = shared_catalog_live_rewrite(
        providers,
        current_model.as_deref(),
        review_model.as_deref(),
        current_provider_id,
    );
    if let Some(model) = rewrite.model.as_deref() {
        next = crate::codex_config::update_codex_toml_field(&next, "model", model)
            .map_err(AppError::Config)?;
    }
    if rewrite.clear_review_model {
        next = crate::codex_config::update_codex_toml_field(&next, "review_model", "")
            .map_err(AppError::Config)?;
    }

    sanitize_live_provider_for_routing_takeover(&next, providers)
}

fn catalog_pointer_is_cc_switch(config_text: &str) -> bool {
    extract_toml_string_field(config_text, "model_catalog_json")
        .and_then(|path| {
            Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name == CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)
        })
        .unwrap_or(false)
}

fn sanitize_live_provider_for_routing_takeover(
    config: &str,
    providers: &[Provider],
) -> Result<String, AppError> {
    let mut doc = config
        .parse::<DocumentMut>()
        .map_err(|error| AppError::Config(format!("Invalid Codex config.toml: {error}")))?;
    let mut dirty = false;

    if let Some(model_providers) = doc
        .get_mut("model_providers")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        let keys: Vec<String> = model_providers
            .iter()
            .map(|(key, _)| key.to_string())
            .collect();
        for key in keys {
            let Some(table) = model_providers
                .get_mut(key.as_str())
                .and_then(toml_edit::Item::as_table_like_mut)
            else {
                continue;
            };
            if table.remove("models").is_some() {
                dirty = true;
            }
        }
    }

    let live_model = extract_toml_string_field(config, "model");
    let disable_remote_compact = should_disable_remote_compact(providers, live_model.as_deref());
    if let Some(provider_key) = doc
        .get("model_provider")
        .and_then(|item| item.as_str())
        .map(str::to_string)
    {
        if let Some(table) = doc
            .get_mut("model_providers")
            .and_then(toml_edit::Item::as_table_like_mut)
            .and_then(|model_providers| model_providers.get_mut(&provider_key))
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            let current_name = table
                .get("name")
                .and_then(|item| item.as_str())
                .unwrap_or("");
            if disable_remote_compact
                && REMOTE_COMPACT_PROVIDER_NAMES
                    .iter()
                    .any(|name| current_name.eq_ignore_ascii_case(name))
            {
                table.insert("name", toml_edit::value(LOCAL_COMPACT_PROVIDER_NAME));
                dirty = true;
            } else if !disable_remote_compact && current_name == LOCAL_COMPACT_PROVIDER_NAME {
                table.insert("name", toml_edit::value("OpenAI"));
                dirty = true;
            }
        }
    }

    let windows = participating_inferred_windows(providers);
    let max_window = windows.iter().copied().max();
    let min_window = windows.iter().copied().min();
    if let (Some(current), Some(max)) = (
        top_level_positive_u64(&doc, "model_context_window"),
        max_window,
    ) {
        if current < max {
            doc.as_table_mut().remove("model_context_window");
            dirty = true;
        }
    }
    if let (Some(current), Some(min)) = (
        top_level_positive_u64(&doc, "model_auto_compact_token_limit"),
        min_window,
    ) {
        if current < min {
            doc.as_table_mut().remove("model_auto_compact_token_limit");
            dirty = true;
        }
    }

    if dirty {
        Ok(doc.to_string())
    } else {
        Ok(config.to_string())
    }
}

fn should_disable_remote_compact(providers: &[Provider], live_model: Option<&str>) -> bool {
    if catalog_has_non_official_participant(providers) {
        return true;
    }
    let Some(model) = live_model.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    if catalog_model_leaf(model).starts_with("grok-") {
        return true;
    }
    let slugs = assign_routing_slugs(providers);
    let known: HashSet<String> = slugs.values().cloned().collect();
    let Some((slug, _)) = parse_routed_model(model, &known) else {
        return false;
    };
    providers.iter().any(|provider| {
        slugs
            .get(&provider.id)
            .is_some_and(|assigned| assigned.eq_ignore_ascii_case(slug))
            && !is_codex_official_provider(provider)
    })
}

fn catalog_model_leaf(model: &str) -> String {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .to_ascii_lowercase()
}

fn participating_inferred_windows(providers: &[Provider]) -> Vec<u64> {
    let mut windows = Vec::new();
    for provider in providers
        .iter()
        .filter(|provider| participates_in_routing_catalog(provider))
    {
        for model in provider_upstream_model_ids(provider) {
            if let Some(window) = inferred_catalog_context_window(&model) {
                windows.push(window);
            }
        }
        if is_codex_official_provider(provider) {
            for seed in OFFICIAL_CATALOG_SEED {
                if let Some(window) = inferred_catalog_context_window(seed) {
                    windows.push(window);
                }
            }
        }
    }
    windows
}

fn top_level_positive_u64(doc: &DocumentMut, field: &str) -> Option<u64> {
    doc.get(field)
        .and_then(|item| item.as_integer())
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SharedCatalogLiveRewrite {
    model: Option<String>,
    clear_review_model: bool,
}

fn shared_catalog_live_rewrite(
    providers: &[Provider],
    current_model: Option<&str>,
    review_model: Option<&str>,
    current_provider_id: Option<&str>,
) -> SharedCatalogLiveRewrite {
    if !catalog_has_non_official_participant(providers) {
        return SharedCatalogLiveRewrite {
            model: None,
            clear_review_model: false,
        };
    }
    let model = if let Some(current_id) = current_provider_id {
        live_model_for_current_provider(providers, current_model, current_id)
    } else {
        current_model.and_then(|value| namespaced_live_model(providers, value))
    };
    // Mixed catalogs must not keep a leftover Official `review_model`
    // (`gpt-5.6-sol` or `default/gpt-5.6-sol`). Codex review then follows
    // `model` instead of silently calling Official beside Grok.
    let clear_review_model = review_model
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    SharedCatalogLiveRewrite {
        model,
        clear_review_model,
    }
}

fn live_model_for_current_provider(
    providers: &[Provider],
    current_model: Option<&str>,
    current_provider_id: &str,
) -> Option<String> {
    let current_default = provider_default_routed_model(providers, current_provider_id)?;
    let Some(current_model) = current_model
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Some(current_default);
    };
    if live_model_belongs_to_provider(providers, current_model, current_provider_id) {
        return namespaced_live_model(providers, current_model);
    }
    // A session pick of `grok/grok-4.6` must survive catalog-only refresh even
    // when the logical current card is still Official.
    if is_third_party_routed_pin(providers, current_model) {
        return None;
    }
    Some(current_default)
}

fn is_third_party_routed_pin(providers: &[Provider], model: &str) -> bool {
    match decide_model_route(providers, model) {
        ModelRouteDecision::Pinned { provider_id, .. } => providers
            .iter()
            .any(|provider| provider.id == provider_id && !is_codex_official_provider(provider)),
        ModelRouteDecision::Default => false,
    }
}

fn live_model_belongs_to_provider(providers: &[Provider], model: &str, provider_id: &str) -> bool {
    matches!(
        decide_model_route(providers, model),
        ModelRouteDecision::Pinned { provider_id: pinned, .. } if pinned == provider_id
    )
}

fn provider_default_upstream_model(provider: &Provider) -> Option<String> {
    codex_provider_upstream_model(provider)
        .or_else(|| {
            provider
                .settings_config
                .get("modelCatalog")
                .and_then(|catalog| catalog.get("models"))
                .and_then(Value::as_array)
                .and_then(|models| models.first())
                .and_then(|entry| entry.get("model"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(ToString::to_string)
        })
        .or_else(|| provider_upstream_model_ids(provider).into_iter().next())
}

/// Prefixed catalog slug for a card's default model (`grok/grok-4.6`).
/// Official-only catalogs keep the unprefixed pool id.
pub fn provider_default_routed_model(providers: &[Provider], provider_id: &str) -> Option<String> {
    let provider = providers
        .iter()
        .find(|provider| provider.id == provider_id)?;
    if !participates_in_routing_catalog(provider) {
        return None;
    }
    let upstream = provider_default_upstream_model(provider)?;
    if !catalog_has_non_official_participant(providers) && is_codex_official_provider(provider) {
        return Some(upstream);
    }
    let slugs = assign_routing_slugs(providers);
    let slug = slugs.get(&provider.id)?;
    Some(format!("{slug}/{}", alias_inner_slashes(&upstream)))
}

fn is_implicit_default_card(provider: &Provider) -> bool {
    is_codex_official_provider(provider)
        || provider.id.eq_ignore_ascii_case("default")
        || preferred_routing_slug(provider).eq_ignore_ascii_case("default")
}

fn is_official_auxiliary_model(model: &str) -> bool {
    matches!(
        catalog_model_leaf(model).as_str(),
        "codex-auto-review" | "gpt-5.3-codex-spark"
    )
}

fn request_matches_provider_default(
    providers: &[Provider],
    request_model: &str,
    provider: &Provider,
) -> bool {
    let Some(upstream) = provider_default_upstream_model(provider) else {
        return false;
    };
    if models_match(request_model, &upstream) {
        return true;
    }
    let slugs = assign_routing_slugs(providers);
    let known: HashSet<String> = slugs.values().cloned().collect();
    if let Some((slug, rest)) = parse_routed_model(request_model, &known) {
        return slugs
            .get(&provider.id)
            .is_some_and(|assigned| assigned.eq_ignore_ascii_case(slug))
            && models_match(rest, &upstream);
    }
    request_model.split_once('/').is_some_and(|(prefix, rest)| {
        !prefix.is_empty()
            && !known.contains(&prefix.to_ascii_lowercase())
            && models_match(rest, &upstream)
    })
}

/// Follow Guardian / leftover Official defaults onto the current third-party
/// card. Explicit picks of another provider's non-default slug stay pinned.
pub fn request_should_follow_current_provider(
    providers: &[Provider],
    request_model: &str,
    current_provider_id: &str,
) -> Option<String> {
    let trimmed = request_model.trim();
    if trimmed.is_empty() {
        return None;
    }
    let current = providers
        .iter()
        .find(|provider| provider.id == current_provider_id)?;
    if is_implicit_default_card(current) {
        return None;
    }
    let current_default = provider_default_routed_model(providers, current_provider_id)?;
    if models_match(trimmed, &current_default)
        || live_model_belongs_to_provider(providers, trimmed, current_provider_id)
    {
        return None;
    }
    if is_official_auxiliary_model(trimmed) {
        return Some(current_default);
    }
    let leftover = providers.iter().any(|provider| {
        provider.id != current_provider_id
            && is_implicit_default_card(provider)
            && request_matches_provider_default(providers, trimmed, provider)
    });
    leftover.then_some(current_default)
}

/// Routed `{slug}/{model}` for a third-party card. Official pins return
/// `None` so default-model / review auxiliaries are not persisted as the
/// live Codex default.
pub fn routed_session_pick(providers: &[Provider], request_model: &str) -> Option<String> {
    let ModelRouteDecision::Pinned {
        provider_id,
        upstream_model,
    } = decide_model_route(providers, request_model)
    else {
        return None;
    };
    let provider = providers
        .iter()
        .find(|provider| provider.id == provider_id)?;
    if is_codex_official_provider(provider) {
        return None;
    }
    let slugs = assign_routing_slugs(providers);
    let slug = slugs.get(&provider_id)?;
    Some(format!("{slug}/{}", alias_inner_slashes(&upstream_model)))
}

/// Session memory for "Codex picked Grok, then sent Official as the file default".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModelFollow {
    pub model: String,
    pub displaced_default: Option<String>,
}

/// Follow a prior third-party pick when this request is the live file default
/// or an Official pin (compact / review / default-model path).
pub fn request_should_follow_session(
    providers: &[Provider],
    request_model: &str,
    follow: &SessionModelFollow,
) -> bool {
    let trimmed = request_model.trim();
    if trimmed.is_empty() || trimmed == follow.model {
        return false;
    }
    if follow
        .displaced_default
        .as_deref()
        .is_some_and(|value| value.trim() == trimmed)
    {
        return true;
    }
    if is_official_auxiliary_model(trimmed) {
        return true;
    }
    match decide_model_route(providers, trimmed) {
        ModelRouteDecision::Pinned { provider_id, .. } => providers
            .iter()
            .any(|provider| provider.id == provider_id && is_codex_official_provider(provider)),
        ModelRouteDecision::Default => {
            let leaf = catalog_model_leaf(trimmed);
            leaf.starts_with("gpt-") || leaf.starts_with("codex-")
        }
    }
}

/// Upstream id after routing prefix strip, used so failed rows can show
/// `grok/grok-4.6 → grok-4.6` before `forward` runs.
pub fn expected_outbound_model(providers: &[Provider], request_model: &str) -> Option<String> {
    match decide_model_route(providers, request_model) {
        ModelRouteDecision::Pinned { upstream_model, .. } => Some(upstream_model),
        ModelRouteDecision::Default => None,
    }
}

/// ChatGPT Official implements `/responses/compact`. Grok / niuma / xAI do not;
/// forwarding that path is what produced the 4× 502 HTML rows.
pub fn provider_rejects_remote_compact(provider: &Provider) -> bool {
    !is_codex_official_provider(provider)
}

/// Write a third-party session pick into live `config.toml` `model` so the
/// next Codex process (and any helper that rereads the file) stops calling
/// Official. Official ids are never persisted here.
pub fn persist_third_party_live_codex_model(model: &str) {
    // Takeover-owned live files must keep the catalog pointer / official model.
    // Skip when a switch already holds the toml lock; the next request retries.
    let Some(()) = crate::codex_config::try_with_live_codex_toml_lock(|| {
        let Some(next) = live_codex_model_persist_update(model) else {
            return;
        };
        match crate::codex_config::write_codex_live_config_atomic_unlocked(Some(&next)) {
            Ok(()) => log::info!("[Codex] persisted live model = {model}"),
            Err(error) => log::warn!("[Codex] failed to persist live model {model}: {error}"),
        }
    }) else {
        log::debug!("[Codex] skip live model persist; config.toml lock busy");
        return;
    };
}

pub fn current_live_codex_model() -> Option<String> {
    crate::codex_config::read_codex_config_text()
        .ok()
        .and_then(|text| extract_toml_string_field(&text, "model"))
}

fn live_codex_model_persist_update(model: &str) -> Option<String> {
    let trimmed = model.trim();
    if trimmed.is_empty() || !trimmed.contains('/') {
        return None;
    }
    let config = crate::codex_config::read_codex_config_text().ok()?;
    if config.trim().is_empty() {
        return None;
    }
    if config.contains(crate::codex_config::CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME) {
        return None;
    }
    if extract_toml_string_field(&config, "model").as_deref() == Some(trimmed) {
        return None;
    }
    crate::codex_config::update_codex_toml_field(&config, "model", trimmed).ok()
}

fn namespaced_live_model(providers: &[Provider], current_model: &str) -> Option<String> {
    let trimmed = current_model.trim();
    if trimmed.is_empty() {
        return None;
    }
    let slugs = assign_routing_slugs(providers);
    let known: HashSet<String> = slugs.values().cloned().collect();
    if parse_routed_model(trimmed, &known).is_some() {
        return None;
    }
    let matches: Vec<&Provider> = providers
        .iter()
        .filter(|provider| participates_in_routing_catalog(provider))
        .filter(|provider| {
            provider_upstream_model_ids(provider)
                .iter()
                .any(|model| models_match(model, trimmed))
        })
        .collect();
    if matches.len() != 1 {
        return None;
    }
    let slug = slugs.get(&matches[0].id)?;
    Some(format!("{slug}/{}", alias_inner_slashes(trimmed)))
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

fn catalog_entries_from_ids(
    ids: &[String],
    profile: CodexCatalogToolProfile,
    config_text: &str,
) -> Vec<Value> {
    if ids.is_empty() {
        return Vec::new();
    }
    let models: Vec<Value> = ids.iter().map(|model| json!({ "model": model })).collect();
    let settings = json!({ "modelCatalog": { "models": models } });
    crate::codex_config::codex_model_catalog_from_settings(&settings, config_text, profile)
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
    let config_text = provider
        .settings_config
        .get("config")
        .and_then(Value::as_str)
        .unwrap_or("");
    catalog_entries_from_ids(
        &advertised_model_ids_for_catalog(provider, discovered),
        profile,
        config_text,
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
    extract_toml_string_field(config_text, "model")
}

fn extract_toml_string_field(config_text: &str, field: &str) -> Option<String> {
    for line in config_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix(field) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_matches('"').trim_matches('\'').trim();
        if !value.is_empty() && !value.starts_with('[') {
            return Some(value.to_string());
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
        match decide_model_route(std::slice::from_ref(&kimi), "org-model") {
            ModelRouteDecision::Pinned {
                provider_id,
                upstream_model,
            } => {
                assert_eq!(provider_id, "kimi");
                assert_eq!(upstream_model, "org/model");
            }
            other => panic!("{other:?}"),
        }
        match decide_model_route(std::slice::from_ref(&kimi), "kimi/org-model") {
            ModelRouteDecision::Pinned {
                provider_id,
                upstream_model,
            } => {
                assert_eq!(provider_id, "kimi");
                assert_eq!(upstream_model, "org/model");
            }
            other => panic!("{other:?}"),
        }
        match decide_model_route(std::slice::from_ref(&kimi), "kimi/org/model") {
            ModelRouteDecision::Pinned { upstream_model, .. } => {
                assert_eq!(upstream_model, "org/model");
            }
            other => panic!("{other:?}"),
        }
        let stripped = strip_routing_prefix_from_body(json!({ "model": "kimi/org-model" }), &kimi);
        assert_eq!(stripped["model"], "org/model");
    }

    #[test]
    fn opt_out_card_still_pins_by_slug() {
        let mut hidden = provider("hidden", "Hidden", &["secret-model"]);
        hidden.meta = Some(ProviderMeta {
            routing_catalog: Some(false),
            ..Default::default()
        });
        let visible = provider("kimi", "Kimi", &["k2"]);
        match decide_model_route(&[hidden, visible], "hidden/secret-model") {
            ModelRouteDecision::Pinned {
                provider_id,
                upstream_model,
            } => {
                assert_eq!(provider_id, "hidden");
                assert_eq!(upstream_model, "secret-model");
            }
            other => panic!("opt-out must still pin by slug, got {other:?}"),
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
    fn collision_winner_follows_sort_index_not_input_order() {
        let mut first = provider("bbbbbbbb-9b11-4d22-8c33-abcdef123456", "Same Name", &["z"]);
        first.sort_index = Some(1);
        let mut second = provider("aaaaaaaa-9b11-4d22-8c33-abcdef123456", "Same Name", &["a"]);
        second.sort_index = Some(0);
        let map = assign_routing_slugs(&[first, second]);
        assert_eq!(
            map.get("aaaaaaaa-9b11-4d22-8c33-abcdef123456")
                .map(String::as_str),
            Some("same-name")
        );
        assert_eq!(
            map.get("bbbbbbbb-9b11-4d22-8c33-abcdef123456")
                .map(String::as_str),
            Some("same-name-bbbbbbbb")
        );
    }

    #[test]
    fn preserve_routing_catalog_keeps_existing_opt_out() {
        let mut existing = provider("kimi", "Kimi", &["k2"]);
        existing.meta = Some(ProviderMeta {
            routing_catalog: Some(false),
            routing_slug: Some("kimi".to_string()),
            ..Default::default()
        });
        let mut incoming = provider("kimi", "Kimi", &["k2"]);
        incoming.meta = Some(ProviderMeta {
            routing_slug: Some("kimi".to_string()),
            ..Default::default()
        });
        preserve_routing_catalog(&existing, &mut incoming);
        assert_eq!(
            incoming.meta.as_ref().and_then(|meta| meta.routing_catalog),
            Some(false)
        );
        assert_eq!(
            incoming
                .meta
                .as_ref()
                .and_then(|meta| meta.routing_slug.as_deref()),
            Some("kimi")
        );
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
                slugs.iter().any(|slug| slug.starts_with("codex-official/")),
                "official native rows should remain selectable: {slugs:?}"
            );
            assert!(
                !slugs
                    .iter()
                    .any(|slug| *slug == "gpt-5.5" || *slug == "gpt-5.6-sol"),
                "unprefixed Official ids must not sit next to third-party rows: {slugs:?}"
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
            std::slice::from_ref(&combo),
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
        let catalog =
            build_merged_codex_routing_catalog(&[official("codex-official")]).expect("catalog");
        let models = catalog["models"].as_array().expect("models");
        let slugs: Vec<&str> = models
            .iter()
            .filter_map(|entry| entry.get("slug").and_then(Value::as_str))
            .collect();
        for expected in [
            "gpt-5.5",
            "gpt-5.6",
            "gpt-5.6-sol",
            "codex-official/gpt-5.5",
        ] {
            assert!(slugs.contains(&expected), "missing {expected} in {slugs:?}");
        }
        let window = |slug: &str| {
            models
                .iter()
                .find(|entry| entry.get("slug").and_then(Value::as_str) == Some(slug))
                .and_then(|entry| entry.get("context_window").and_then(Value::as_u64))
        };
        assert_eq!(window("gpt-5.6-sol"), Some(372_000));
        assert_eq!(window("gpt-5.5"), Some(272_000));
        assert_eq!(window("codex-official/gpt-5.6-sol"), Some(372_000));
    }

    #[test]
    fn official_only_pool_keeps_unprefixed_seed() {
        let primary = official("codex-official");
        let mut backup = official("chatgpt-backup");
        backup.name = "ChatGPT Backup".to_string();
        let slugs = catalog_slugs(&[primary, backup]);
        assert!(
            slugs.iter().any(|slug| slug == "gpt-5.6-sol"),
            "Official-only catalogs keep unprefixed pool ids: {slugs:?}"
        );
    }

    #[test]
    fn official_plus_grok_hides_unprefixed_official_ids() {
        let slugs = catalog_slugs(&[
            official("codex-official"),
            provider("grok", "Grok", &["grok-4.6"]),
        ]);
        assert!(slugs.contains(&"grok/grok-4.6".to_string()), "{slugs:?}");
        assert!(
            slugs.contains(&"codex-official/gpt-5.6-sol".to_string()),
            "{slugs:?}"
        );
        assert!(
            !slugs
                .iter()
                .any(|slug| { slug == "gpt-5.6-sol" || slug == "gpt-5.6" || slug == "gpt-5.5" }),
            "bare Official ids must not appear beside Grok: {slugs:?}"
        );
    }

    #[test]
    fn official_plus_grok_unprefixed_sol_still_pins_official() {
        let providers = vec![
            official("codex-official"),
            provider("grok", "Grok", &["grok-4.6"]),
        ];
        match decide_model_route(&providers, "gpt-5.6-sol") {
            ModelRouteDecision::Pinned { provider_id, .. } => {
                assert_eq!(provider_id, "codex-official");
            }
            other => panic!("typed gpt-5.6-sol must still pin Official, got {other:?}"),
        }
        match decide_model_route(&providers, "grok/grok-4.6") {
            ModelRouteDecision::Pinned {
                provider_id,
                upstream_model,
            } => {
                assert_eq!(provider_id, "grok");
                assert_eq!(upstream_model, "grok-4.6");
            }
            other => panic!("grok/grok-4.6 must pin Grok only, got {other:?}"),
        }
    }

    #[test]
    fn shared_catalog_namespaces_official_default_and_clears_review_model() {
        let official = official("codex-official");
        let grok = provider("grok", "Grok", &["grok-4.6"]);
        assert_eq!(
            shared_catalog_live_rewrite(
                std::slice::from_ref(&official),
                Some("gpt-5.6-sol"),
                Some("gpt-5.6-sol"),
                None,
            ),
            SharedCatalogLiveRewrite {
                model: None,
                clear_review_model: false,
            }
        );
        assert_eq!(
            shared_catalog_live_rewrite(
                &[official.clone(), grok.clone()],
                Some("gpt-5.6-sol"),
                Some("gpt-5.6-sol"),
                None,
            ),
            SharedCatalogLiveRewrite {
                model: Some("codex-official/gpt-5.6-sol".into()),
                clear_review_model: true,
            }
        );
        assert_eq!(
            shared_catalog_live_rewrite(
                &[official.clone(), grok.clone()],
                Some("grok/grok-4.6"),
                Some("grok/grok-4.6"),
                None,
            ),
            SharedCatalogLiveRewrite {
                model: None,
                clear_review_model: true,
            }
        );
        assert_eq!(
            shared_catalog_live_rewrite(
                &[official, grok],
                Some("grok/grok-4.6"),
                Some("default/gpt-5.6-sol"),
                None,
            ),
            SharedCatalogLiveRewrite {
                model: None,
                clear_review_model: true,
            }
        );
    }

    #[test]
    fn shared_catalog_rewrites_leftover_default_to_current_provider() {
        let official = official("default");
        let grok = provider("grok", "Grok", &["grok-4.6"]);
        let providers = [official, grok];
        assert_eq!(
            shared_catalog_live_rewrite(
                &providers,
                Some("default/gpt-5.6-sol"),
                Some("default/gpt-5.6-sol"),
                Some("grok"),
            ),
            SharedCatalogLiveRewrite {
                model: Some("grok/grok-4.6".into()),
                clear_review_model: true,
            }
        );
        assert_eq!(
            shared_catalog_live_rewrite(&providers, Some("grok/grok-4.5"), None, Some("grok"),),
            SharedCatalogLiveRewrite {
                model: None,
                clear_review_model: false,
            }
        );
        assert_eq!(
            shared_catalog_live_rewrite(&providers, Some("grok/grok-4.6"), None, Some("default"),),
            SharedCatalogLiveRewrite {
                model: None,
                clear_review_model: false,
            }
        );
    }

    #[test]
    fn current_provider_leads_merged_catalog_priority() {
        let official = official("default");
        let grok = provider("grok", "Grok", &["grok-4.6"]);
        let official_first = catalog_slugs(&[official.clone(), grok.clone()]);
        assert!(
            official_first
                .first()
                .is_some_and(|slug| slug.starts_with("default/")),
            "union catalog without current-first starts on the Official card: {official_first:?}"
        );

        let current_first =
            providers_current_first(std::slice::from_ref(&grok), [official, grok.clone()]);
        let slugs = catalog_slugs(&current_first);
        assert_eq!(
            slugs.first().map(String::as_str),
            Some("grok/grok-4.6"),
            "{slugs:?}"
        );
        let catalog = build_merged_codex_routing_catalog(&current_first).expect("catalog");
        assert_eq!(
            catalog["models"][0].get("priority").and_then(Value::as_u64),
            Some(1000)
        );
    }

    #[test]
    fn leftover_official_default_follows_current_third_party() {
        let mut official = official("codex-official");
        official.settings_config["config"] = json!("model = \"gpt-5.6-sol\"\n");
        let grok = provider("grok", "Grok", &["grok-4.6"]);
        let kimi = provider("kimi", "Kimi", &["k2"]);
        let providers = [official, grok, kimi];
        assert_eq!(
            request_should_follow_current_provider(&providers, "default/gpt-5.6-sol", "grok")
                .as_deref(),
            Some("grok/grok-4.6")
        );
        assert_eq!(
            request_should_follow_current_provider(&providers, "gpt-5.6-sol", "grok").as_deref(),
            Some("grok/grok-4.6")
        );
        assert_eq!(
            request_should_follow_current_provider(&providers, "codex-auto-review", "grok")
                .as_deref(),
            Some("grok/grok-4.6")
        );
        assert_eq!(
            request_should_follow_current_provider(&providers, "kimi/k2", "grok"),
            None
        );
        assert_eq!(
            request_should_follow_current_provider(&providers, "codex-official/gpt-5.5", "grok"),
            None
        );
        assert_eq!(
            request_should_follow_current_provider(
                &providers,
                "default/gpt-5.6-sol",
                "codex-official"
            ),
            None
        );
    }

    #[test]
    fn routed_session_pick_keeps_grok_and_skips_official() {
        let official = official("codex-official");
        let grok = provider("grok", "Grok", &["grok-4.6"]);
        let providers = [official, grok];
        assert_eq!(
            routed_session_pick(&providers, "grok/grok-4.6").as_deref(),
            Some("grok/grok-4.6")
        );
        assert_eq!(
            routed_session_pick(&providers, "grok-4.6").as_deref(),
            Some("grok/grok-4.6")
        );
        assert_eq!(routed_session_pick(&providers, "default/gpt-5.6-sol"), None);
        assert_eq!(routed_session_pick(&providers, "gpt-5.6-sol"), None);
    }

    #[test]
    fn session_follow_rewrites_live_default_and_official_pin() {
        let official = official("codex-official");
        let grok = provider("grok", "Grok", &["grok-4.6"]);
        let codes = provider("codes", "Niuma", &["gpt-5.4"]);
        let providers = [official, grok, codes];
        let follow = SessionModelFollow {
            model: "grok/grok-4.6".into(),
            displaced_default: Some("default/gpt-5.6-sol".into()),
        };
        assert!(request_should_follow_session(
            &providers,
            "default/gpt-5.6-sol",
            &follow
        ));
        assert!(request_should_follow_session(
            &providers,
            "codex-official/gpt-5.6-sol",
            &follow
        ));
        assert!(!request_should_follow_session(
            &providers,
            "grok/grok-4.6",
            &follow
        ));
        let follow_codes = SessionModelFollow {
            model: "grok/grok-4.6".into(),
            displaced_default: Some("codes/gpt-5.4".into()),
        };
        assert!(request_should_follow_session(
            &providers,
            "codes/gpt-5.4",
            &follow_codes
        ));
        assert!(
            !request_should_follow_session(&providers, "codes/gpt-5.6-sol", &follow_codes),
            "a known non-Official slug pin must not be stolen by gpt-* leaf follow"
        );
        assert!(!request_should_follow_session(
            &providers,
            "kimi/k2",
            &follow_codes
        ));
        assert!(request_should_follow_session(
            &providers, "gpt-5.5", &follow
        ));
    }

    #[test]
    fn expected_outbound_and_compact_reject_for_grok() {
        let official = official("codex-official");
        let grok = provider("grok", "Grok", &["grok-4.6"]);
        assert_eq!(
            expected_outbound_model(std::slice::from_ref(&grok), "grok/grok-4.6").as_deref(),
            Some("grok-4.6")
        );
        assert!(provider_rejects_remote_compact(&grok));
        assert!(!provider_rejects_remote_compact(&official));
    }

    fn sample_omx_takeover_toml() -> &'static str {
        r#"
model = "gpt-5.6-sol"
review_model = "gpt-5.6-sol"
model_provider = "cm"
model_context_window = 258400
model_auto_compact_token_limit = 120000
model_reasoning_effort = "xhigh"

[model_providers.cm]
name = "OpenAI"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
experimental_bearer_token = "PROXY_MANAGED"
models = [
  { model = "gpt-5.6-sol", slug = "gpt-5.6-sol", context_window = 272000, isDefault = true },
  { model = "gpt-5.5", slug = "gpt-5.5", context_window = 272000 },
]
"#
    }

    #[test]
    fn routing_takeover_rewrites_omx_dump_for_official_plus_grok() {
        let rewritten = rewrite_codex_toml_text_for_routing_takeover(
            sample_omx_takeover_toml(),
            &[
                official("codex-official"),
                provider("grok", "Grok", &["grok-4.6"]),
            ],
            None,
        )
        .expect("rewrite");
        assert!(
            rewritten.contains("model_catalog_json = \"cc-switch-model-catalog.json\""),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("model = \"codex-official/gpt-5.6-sol\""),
            "{rewritten}"
        );
        assert!(
            !rewritten.contains("review_model"),
            "unprefixed review_model must be cleared:\n{rewritten}"
        );
        assert!(
            rewritten.contains("name = \"CC Switch\""),
            "remote compact must be disabled when Grok participates:\n{rewritten}"
        );
        assert!(!rewritten.contains("name = \"OpenAI\""), "{rewritten}");
        assert!(
            !rewritten.contains("models = ["),
            "inline Official dump must be stripped:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("model_context_window"),
            "undersized window must not override Grok 500k:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("model_auto_compact_token_limit"),
            "120k auto-compact must not fire on Grok:\n{rewritten}"
        );
        assert!(
            rewritten.contains("model_reasoning_effort = \"xhigh\""),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("experimental_bearer_token = \"PROXY_MANAGED\""),
            "{rewritten}"
        );
    }

    #[test]
    fn routing_takeover_keeps_openai_name_for_official_only() {
        let rewritten = rewrite_codex_toml_text_for_routing_takeover(
            sample_omx_takeover_toml(),
            &[official("codex-official")],
            None,
        )
        .expect("rewrite");
        assert!(rewritten.contains("name = \"OpenAI\""), "{rewritten}");
        assert!(!rewritten.contains("name = \"CC Switch\""), "{rewritten}");
        assert!(
            rewritten.contains("model_catalog_json = \"cc-switch-model-catalog.json\""),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("model = \"gpt-5.6-sol\""),
            "Official-only keeps the unprefixed pool id:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("model_auto_compact_token_limit"),
            "{rewritten}"
        );
    }

    #[test]
    fn routing_takeover_preserves_user_catalog_pointer() {
        let input = r#"
model = "grok/grok-4.6"
model_provider = "cm"
model_catalog_json = "my-handwritten-catalog.json"

[model_providers.cm]
name = "OpenAI"
base_url = "http://127.0.0.1:15721/v1"
"#;
        let rewritten = rewrite_codex_toml_text_for_routing_takeover(
            input,
            &[
                official("codex-official"),
                provider("grok", "Grok", &["grok-4.6"]),
            ],
            None,
        )
        .expect("rewrite");
        assert!(
            rewritten.contains("model_catalog_json = \"my-handwritten-catalog.json\""),
            "{rewritten}"
        );
        assert!(rewritten.contains("name = \"CC Switch\""), "{rewritten}");
    }

    #[test]
    fn routing_takeover_clears_namespaced_official_review_model() {
        let input = r#"
model = "grok/grok-4.6"
review_model = "default/gpt-5.6-sol"
model_provider = "cm"

[model_providers.cm]
name = "OpenAI"
base_url = "http://127.0.0.1:15721/v1"
"#;
        let rewritten = rewrite_codex_toml_text_for_routing_takeover(
            input,
            &[
                official("codex-official"),
                provider("grok", "Grok", &["grok-4.6"]),
            ],
            None,
        )
        .expect("rewrite");
        assert!(
            !rewritten.contains("review_model"),
            "namespaced Official review_model must be cleared:\n{rewritten}"
        );
        assert!(
            rewritten.contains("model = \"grok/grok-4.6\""),
            "{rewritten}"
        );
    }

    #[test]
    fn routing_takeover_pins_current_provider_default_over_catalog_head() {
        let input = r#"
model = "default/gpt-5.6-sol"
review_model = "default/gpt-5.6-sol"
model_provider = "cm"

[model_providers.cm]
name = "Grok"
base_url = "http://127.0.0.1:15721/v1"
"#;
        let rewritten = rewrite_codex_toml_text_for_routing_takeover(
            input,
            &[official("default"), provider("grok", "Grok", &["grok-4.6"])],
            Some("grok"),
        )
        .expect("rewrite");
        assert!(
            rewritten.contains("model = \"grok/grok-4.6\""),
            "{rewritten}"
        );
        assert!(
            !rewritten.contains("model = \"default/gpt-5.6-sol\""),
            "{rewritten}"
        );
        assert!(!rewritten.contains("review_model"), "{rewritten}");
    }

    #[test]
    fn routing_takeover_disables_remote_compact_for_typed_grok() {
        let input = r#"
model = "grok/grok-4.6"
model_provider = "cm"

[model_providers.cm]
name = "OpenAI"
base_url = "http://127.0.0.1:15721/v1"
"#;
        let rewritten = rewrite_codex_toml_text_for_routing_takeover(
            input,
            &[official("codex-official")],
            None,
        )
        .expect("rewrite");
        assert!(rewritten.contains("name = \"CC Switch\""), "{rewritten}");
    }

    #[test]
    fn routing_takeover_restores_openai_name_when_grok_leaves() {
        let input = r#"
model = "default/gpt-5.6-sol"
model_provider = "cm"
model_catalog_json = "cc-switch-model-catalog.json"

[model_providers.cm]
name = "CC Switch"
base_url = "http://127.0.0.1:15721/v1"
"#;
        let rewritten = rewrite_codex_toml_text_for_routing_takeover(
            input,
            &[official("codex-official")],
            None,
        )
        .expect("rewrite");
        assert!(rewritten.contains("name = \"OpenAI\""), "{rewritten}");
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

    #[test]
    #[serial]
    fn discovery_extra_grok_keeps_real_reasoning_levels() {
        with_test_home(|| {
            let mut cache = HashMap::new();
            cache.insert("grok".to_string(), vec!["grok-4.6".into()]);
            save_routing_discovery_cache(&cache);
            let catalog =
                build_merged_codex_routing_catalog(&[provider("grok", "Grok", &["grok-4.5"])])
                    .expect("merged catalog");
            let entry = catalog["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item.get("slug").and_then(Value::as_str) == Some("grok/grok-4.6"))
                .expect("routed grok-4.6");
            let efforts: Vec<&str> = entry["supported_reasoning_levels"]
                .as_array()
                .expect("supported_reasoning_levels")
                .iter()
                .filter_map(|level| level.get("effort").and_then(Value::as_str))
                .collect();
            assert_eq!(efforts, ["low", "medium", "high", "xhigh"], "{entry}");
            assert_eq!(
                entry.get("context_window").and_then(Value::as_u64),
                Some(500_000),
                "routed grok-4.6 must keep the official 500k window, not 128k: {entry}"
            );
        });
    }

    #[test]
    #[serial]
    fn discovery_extra_gpt56_keeps_max_and_ultra() {
        with_test_home(|| {
            let mut cache = HashMap::new();
            cache.insert("packy".to_string(), vec!["gpt-5.6-sol".into()]);
            save_routing_discovery_cache(&cache);
            let catalog =
                build_merged_codex_routing_catalog(&[provider("packy", "Packy", &["gpt-5.5"])])
                    .expect("merged catalog");
            let entry = catalog["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item.get("slug").and_then(Value::as_str) == Some("packy/gpt-5.6-sol"))
                .expect("routed gpt-5.6-sol");
            let efforts: Vec<&str> = entry["supported_reasoning_levels"]
                .as_array()
                .expect("supported_reasoning_levels")
                .iter()
                .filter_map(|level| level.get("effort").and_then(Value::as_str))
                .collect();
            assert_eq!(
                efforts,
                ["low", "medium", "high", "xhigh", "max", "ultra"],
                "{entry}"
            );
            assert_eq!(
                entry.get("default_reasoning_level").and_then(Value::as_str),
                Some("medium")
            );
            assert_eq!(
                entry.get("context_window").and_then(Value::as_u64),
                Some(372_000),
                "routed gpt-5.6-sol must keep the Codex 372k window, not 128k: {entry}"
            );
        });
    }

    fn known(slugs: &[&str]) -> HashSet<String> {
        slugs.iter().map(|slug| slug.to_ascii_lowercase()).collect()
    }

    #[test]
    fn bare_catalog_id_peels_own_prefix_and_drops_foreign() {
        let slugs = known(&["default", "grok"]);
        let foreign = HashSet::from(["default-gpt-5.6-sol".to_string()]);
        assert_eq!(
            bare_catalog_model_id("grok-4.6", "grok", &slugs, &foreign).as_deref(),
            Some("grok-4.6")
        );
        assert_eq!(
            bare_catalog_model_id("grok/grok-4.6", "grok", &slugs, &foreign).as_deref(),
            Some("grok-4.6")
        );
        assert_eq!(
            bare_catalog_model_id("grok/grok-grok-4.6", "grok", &slugs, &foreign).as_deref(),
            Some("grok-4.6")
        );
        assert_eq!(
            bare_catalog_model_id("default/gpt-5.6-sol", "grok", &slugs, &foreign),
            None
        );
        assert_eq!(
            bare_catalog_model_id("default-gpt-5.6-sol", "grok", &slugs, &foreign),
            None
        );
        assert_eq!(
            bare_catalog_model_id("grok/default-gpt-5.6-sol", "grok", &slugs, &foreign),
            None
        );
        assert_eq!(
            bare_catalog_model_id("org/custom", "grok", &slugs, &foreign).as_deref(),
            Some("org-custom")
        );
        assert_eq!(
            bare_catalog_model_id("kimi-k2", "packy", &slugs, &foreign).as_deref(),
            Some("kimi-k2"),
            "native ids that only share a hyphen prefix with a sibling slug must stay"
        );
    }

    #[test]
    fn sibling_native_hyphen_ids_stay_in_merged_catalog() {
        let kimi = provider("kimi", "Kimi", &["kimi-k2"]);
        let packy = provider("packy", "Packy", &["kimi-k2", "kimi-for-coding"]);
        let slugs = catalog_slugs(&[kimi, packy]);
        assert!(slugs.contains(&"kimi/kimi-k2".to_string()), "{slugs:?}");
        assert!(slugs.contains(&"packy/kimi-k2".to_string()), "{slugs:?}");
        assert!(
            slugs.contains(&"packy/kimi-for-coding".to_string()),
            "{slugs:?}"
        );
    }

    #[test]
    #[serial]
    fn mixed_catalog_does_not_double_prefix_or_cross_copy() {
        let mut openai = official("default");
        openai.name = "OpenAI".to_string();
        let grok = provider(
            "grok",
            "Grok",
            &[
                "grok-4.6",
                "grok/grok-4.6",
                "grok/grok-grok-4.6",
                "default/gpt-5.6-sol",
                "default-gpt-5.6-sol",
                "grok/default-gpt-5.6-sol",
            ],
        );
        let first = catalog_slugs(&[openai.clone(), grok.clone()]);
        assert!(
            first.contains(&"grok/grok-4.6".to_string()),
            "expected single Grok row: {first:?}"
        );
        assert!(
            first.contains(&"default/gpt-5.6-sol".to_string()),
            "expected Official row: {first:?}"
        );
        assert!(
            !first.iter().any(|slug| slug.contains("grok-grok")),
            "doubled Grok prefix leaked: {first:?}"
        );
        assert!(
            !first
                .iter()
                .any(|slug| slug.starts_with("grok/default") || slug.contains("default-gpt")),
            "Official models copied into Grok: {first:?}"
        );
        assert_eq!(
            first
                .iter()
                .filter(|slug| slug.as_str() == "grok/grok-4.6")
                .count(),
            1
        );

        let catalog =
            build_merged_codex_routing_catalog(&[openai.clone(), grok.clone()]).expect("catalog");
        let grok_display = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item.get("slug").and_then(Value::as_str) == Some("grok/grok-4.6"))
            .and_then(|item| item.get("display_name").and_then(Value::as_str))
            .unwrap();
        assert_eq!(grok_display, "Grok / grok-4.6");

        let mut grok_replay = grok;
        let replayed: Vec<Value> = first.iter().map(|slug| json!({ "model": slug })).collect();
        grok_replay.settings_config["modelCatalog"] = json!({ "models": replayed });
        let second = catalog_slugs(&[openai, grok_replay]);
        assert_eq!(
            second
                .iter()
                .filter(|slug| slug.starts_with("grok/"))
                .count(),
            first
                .iter()
                .filter(|slug| slug.starts_with("grok/"))
                .count(),
            "second pass grew the Grok namespace:\nfirst={first:?}\nsecond={second:?}"
        );
        assert!(
            !second.iter().any(|slug| slug.contains("grok-grok")),
            "second pass reintroduced doubled prefix: {second:?}"
        );
        assert!(
            !second
                .iter()
                .any(|slug| slug.starts_with("grok/default") || slug.contains("default-gpt")),
            "second pass copied Official into Grok: {second:?}"
        );
    }

    #[test]
    #[serial]
    fn discovery_skips_image_audio_realtime_unless_mapped() {
        with_test_home(|| {
            let mut cache = HashMap::new();
            cache.insert(
                "default".to_string(),
                vec![
                    "gpt-image-1".into(),
                    "gpt-4o-audio-preview".into(),
                    "gpt-4o-realtime-preview".into(),
                    "gpt-5.6-sol".into(),
                ],
            );
            save_routing_discovery_cache(&cache);
            let mut openai = official("default");
            openai.settings_config["modelCatalog"] =
                json!({ "models": [{ "model": "gpt-5.6-sol" }] });
            let slugs = catalog_slugs(&[openai, provider("grok", "Grok", &["grok-4.6"])]);
            assert!(
                slugs.contains(&"default/gpt-5.6-sol".to_string()),
                "{slugs:?}"
            );
            assert!(
                !slugs.iter().any(|slug| {
                    slug.contains("gpt-image")
                        || slug.contains("audio-preview")
                        || slug.contains("realtime-preview")
                }),
                "non-coding discovery extras leaked: {slugs:?}"
            );
        });
    }
}
