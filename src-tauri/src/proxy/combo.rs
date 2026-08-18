//! OpenCodex-style virtual `combo/{id}` models.
//!
//! A combo fronts an ordered list of real `{routing_slug}/{model}` targets.
//! `combo/main` (and Claude discovery's `anthropic/combo/main`) expand to that
//! list. Failover keeps configuration order; round-robin uses smooth weighted
//! selection for the first hop, then the remaining targets in config order.

use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::model_routing::{
    alias_inner_slashes, assign_routing_slugs, CLAUDE_GATEWAY_MODEL_PREFIX,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub const COMBO_NAMESPACE: &str = "combo";
pub const COMBO_PREFIX: &str = "combo/";
pub const CLAUDE_COMBO_PREFIX: &str = "anthropic/combo/";
pub const MODEL_COMBOS_SETTING_KEY: &str = "model_combos";

const MAX_ID_LEN: usize = 64;
const MIN_WEIGHT: u32 = 1;
const MAX_WEIGHT: u32 = 10_000;
const MIN_STICKY: u32 = 1;
const MAX_STICKY: u32 = 100;

fn default_weight() -> u32 {
    1
}

fn default_sticky() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ComboTarget {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ComboStrategy {
    #[default]
    Failover,
    #[serde(alias = "roundRobin", alias = "round_robin")]
    RoundRobin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCombo {
    pub id: String,
    pub targets: Vec<ComboTarget>,
    #[serde(default)]
    pub strategy: ComboStrategy,
    #[serde(default = "default_sticky")]
    pub sticky_limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ComboRecord {
    targets: Vec<ComboTarget>,
    #[serde(default)]
    strategy: ComboStrategy,
    #[serde(default = "default_sticky")]
    sticky_limit: u32,
}

#[derive(Debug, Clone)]
pub struct ResolvedComboTarget {
    pub provider: Provider,
    pub upstream_model: String,
    pub weight: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ComboRoundRobinState {
    current_weights: Vec<i32>,
    sticky_index: usize,
    sticky_remaining: u32,
}

impl ModelCombo {
    pub fn canonical_model_id(&self) -> String {
        format!("{COMBO_NAMESPACE}/{}", self.id)
    }

    pub fn claude_gateway_model_id(&self) -> String {
        format!("{CLAUDE_GATEWAY_MODEL_PREFIX}{COMBO_NAMESPACE}/{}", self.id)
    }
}

/// Parse `combo/{id}` or `anthropic/combo/{id}` when at least one combo exists.
/// The `combo/` namespace is reserved only while combos are configured, so a
/// physical provider whose slug is `combo` still works when the map is empty.
pub fn combo_id_from_request_model(request_model: &str, combos_configured: bool) -> Option<&str> {
    if !combos_configured {
        return None;
    }
    let trimmed = request_model.trim();
    let rest = trimmed
        .strip_prefix(CLAUDE_COMBO_PREFIX)
        .or_else(|| trimmed.strip_prefix(COMBO_PREFIX))?;
    let id = rest.trim();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

pub fn is_valid_combo_id(id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() || id.len() > MAX_ID_LEN {
        return false;
    }
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

#[cfg(test)]
pub fn parse_combo_target_spec(spec: &str) -> Result<ComboTarget, AppError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(AppError::InvalidInput("empty combo target".into()));
    }
    let (route, weight) = split_optional_weight(spec);
    let (provider, model) = route.split_once('/').ok_or_else(|| {
        AppError::InvalidInput(format!("combo target '{spec}' must be provider/model"))
    })?;
    let provider = provider.trim();
    let model = model.trim();
    if provider.is_empty() || model.is_empty() {
        return Err(AppError::InvalidInput(format!(
            "combo target '{spec}' must be provider/model"
        )));
    }
    if provider.eq_ignore_ascii_case(COMBO_NAMESPACE) {
        return Err(AppError::InvalidInput(
            "combo targets cannot nest another combo".into(),
        ));
    }
    Ok(ComboTarget {
        provider: provider.to_string(),
        model: model.to_string(),
        weight,
    })
}

#[cfg(test)]
fn split_optional_weight(spec: &str) -> (&str, u32) {
    if let Some((left, right)) = spec.rsplit_once(':') {
        if let Ok(weight) = right.parse::<u32>() {
            if (MIN_WEIGHT..=MAX_WEIGHT).contains(&weight) && !left.is_empty() {
                return (left, weight);
            }
        }
    }
    (spec, default_weight())
}

pub fn validate_combo(
    combo: &ModelCombo,
    other_ids: &HashSet<String>,
    reserved_slugs: &HashSet<String>,
) -> Result<(), AppError> {
    let id = combo.id.trim();
    if !is_valid_combo_id(id) {
        return Err(AppError::InvalidInput(format!(
            "combo id '{id}' must start with a letter or number and may contain . _ - (max {MAX_ID_LEN})"
        )));
    }
    if id.eq_ignore_ascii_case(COMBO_NAMESPACE) {
        return Err(AppError::InvalidInput(
            "combo id 'combo' is reserved".into(),
        ));
    }
    let id_lower = id.to_ascii_lowercase();
    if other_ids
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(id))
    {
        return Err(AppError::InvalidInput(format!(
            "combo id '{id}' already exists"
        )));
    }
    if reserved_slugs.contains(&id_lower) {
        return Err(AppError::InvalidInput(format!(
            "combo id '{id}' collides with a provider routing slug"
        )));
    }
    if combo.targets.is_empty() {
        return Err(AppError::InvalidInput(
            "combo must have at least one target".into(),
        ));
    }
    if !(MIN_STICKY..=MAX_STICKY).contains(&combo.sticky_limit) {
        return Err(AppError::InvalidInput(format!(
            "stickyLimit must be {MIN_STICKY}-{MAX_STICKY}"
        )));
    }

    let mut seen = HashSet::new();
    for target in &combo.targets {
        if target.provider.trim().is_empty() || target.model.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "combo target needs provider and model".into(),
            ));
        }
        if target.provider.eq_ignore_ascii_case(COMBO_NAMESPACE) {
            return Err(AppError::InvalidInput(
                "combo targets cannot nest another combo".into(),
            ));
        }
        if !(MIN_WEIGHT..=MAX_WEIGHT).contains(&target.weight) {
            return Err(AppError::InvalidInput(format!(
                "combo target weight must be {MIN_WEIGHT}-{MAX_WEIGHT}"
            )));
        }
        let key = (
            target.provider.trim().to_ascii_lowercase(),
            alias_inner_slashes(target.model.trim()).to_ascii_lowercase(),
        );
        if !seen.insert(key) {
            return Err(AppError::InvalidInput(format!(
                "duplicate combo target {}/{}",
                target.provider, target.model
            )));
        }
    }
    Ok(())
}

pub fn combos_from_setting_json(raw: Option<&str>) -> Result<Vec<ModelCombo>, AppError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let map: HashMap<String, ComboRecord> =
        serde_json::from_str(raw).map_err(|error| AppError::InvalidInput(error.to_string()))?;
    let mut combos: Vec<ModelCombo> = map
        .into_iter()
        .map(|(id, record)| ModelCombo {
            id,
            targets: record.targets,
            strategy: record.strategy,
            sticky_limit: record.sticky_limit,
        })
        .collect();
    combos.sort_by(|left, right| {
        left.id
            .to_ascii_lowercase()
            .cmp(&right.id.to_ascii_lowercase())
    });
    Ok(combos)
}

pub fn combos_to_setting_json(combos: &[ModelCombo]) -> Result<String, AppError> {
    let mut map = serde_json::Map::new();
    for combo in combos {
        let record = ComboRecord {
            targets: combo.targets.clone(),
            strategy: combo.strategy,
            sticky_limit: combo.sticky_limit,
        };
        map.insert(
            combo.id.clone(),
            serde_json::to_value(record)
                .map_err(|error| AppError::JsonSerialize { source: error })?,
        );
    }
    serde_json::to_string(&Value::Object(map))
        .map_err(|error| AppError::JsonSerialize { source: error })
}

pub fn find_combo<'a>(combos: &'a [ModelCombo], combo_id: &str) -> Option<&'a ModelCombo> {
    combos
        .iter()
        .find(|combo| combo.id.eq_ignore_ascii_case(combo_id.trim()))
}

/// Resolve configured targets against this app's provider cards.
/// Unknown slugs are skipped (ineligible for this app).
pub fn resolve_combo_targets(
    combo: &ModelCombo,
    providers: &[Provider],
) -> Vec<ResolvedComboTarget> {
    let slugs = assign_routing_slugs(providers);
    let mut resolved = Vec::new();
    for target in &combo.targets {
        let want = target.provider.trim().to_ascii_lowercase();
        let Some(provider) = providers.iter().find(|provider| {
            slugs.get(&provider.id).is_some_and(|slug| slug == &want)
                || provider.id.eq_ignore_ascii_case(&want)
        }) else {
            continue;
        };
        resolved.push(ResolvedComboTarget {
            provider: provider.clone(),
            upstream_model: target.model.trim().to_string(),
            weight: target.weight.max(MIN_WEIGHT),
        });
    }
    resolved
}

pub fn order_failover(targets: Vec<ResolvedComboTarget>) -> Vec<ResolvedComboTarget> {
    targets
}

/// Smooth weighted round-robin for the first hop; remaining targets stay in
/// configuration order so a retryable failure can still walk the rest.
pub fn order_round_robin(
    combo_id: &str,
    targets: Vec<ResolvedComboTarget>,
    sticky_limit: u32,
    state: &mut HashMap<String, ComboRoundRobinState>,
) -> Vec<ResolvedComboTarget> {
    if targets.is_empty() {
        return targets;
    }
    let sticky = sticky_limit.clamp(MIN_STICKY, MAX_STICKY);
    let key = combo_id.to_ascii_lowercase();
    let entry = state.entry(key).or_default();
    if entry.current_weights.len() != targets.len() {
        *entry = ComboRoundRobinState {
            current_weights: vec![0; targets.len()],
            sticky_index: 0,
            sticky_remaining: 0,
        };
    }

    let pick = if entry.sticky_remaining > 0 && entry.sticky_index < targets.len() {
        entry.sticky_index
    } else {
        let weights: Vec<u32> = targets.iter().map(|target| target.weight).collect();
        let index = swrr_pick(&weights, &mut entry.current_weights);
        entry.sticky_index = index;
        entry.sticky_remaining = sticky;
        index
    };
    entry.sticky_remaining = entry.sticky_remaining.saturating_sub(1);

    let mut ordered = Vec::with_capacity(targets.len());
    ordered.push(targets[pick].clone());
    for (index, target) in targets.into_iter().enumerate() {
        if index != pick {
            ordered.push(target);
        }
    }
    ordered
}

fn swrr_pick(weights: &[u32], current: &mut [i32]) -> usize {
    let mut best = 0usize;
    let mut best_value = i32::MIN;
    let total: i32 = weights.iter().map(|weight| *weight as i32).sum();
    for (index, weight) in weights.iter().enumerate() {
        current[index] = current[index].saturating_add(*weight as i32);
        if current[index] > best_value {
            best_value = current[index];
            best = index;
        }
    }
    if total > 0 {
        current[best] = current[best].saturating_sub(total);
    }
    best
}

pub fn catalog_slug(combo: &ModelCombo) -> String {
    combo.canonical_model_id()
}

/// Clone the first resolved target's catalog row (tool profile included) and
/// rewrite it to `combo/{id}`. Falls back to a text-only native-responses stub.
pub fn combo_catalog_entry(
    combo: &ModelCombo,
    providers: &[Provider],
    template: Option<&Value>,
) -> Option<Value> {
    if resolve_combo_targets(combo, providers).is_empty() {
        return None;
    }
    let slug = catalog_slug(combo);
    let display = format!("Combo / {}", combo.id);
    if let Some(mut entry) = template.cloned() {
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("slug".to_string(), json!(slug));
            obj.insert("display_name".to_string(), json!(display));
            obj.insert("description".to_string(), json!(display));
        }
        return Some(entry);
    }
    Some(json!({
        "slug": slug,
        "display_name": display,
        "description": display,
        "model_messages": { "instructions_template": "" },
        "additional_speed_tiers": [],
        "context_window": 128000,
        "input_modalities": ["text"],
    }))
}

pub fn combo_claude_gateway_items(combos: &[ModelCombo], providers: &[Provider]) -> Vec<Value> {
    let mut items = Vec::new();
    for combo in combos {
        if resolve_combo_targets(combo, providers).is_empty() {
            continue;
        }
        items.push(json!({
            "type": "model",
            "id": combo.claude_gateway_model_id(),
            "display_name": format!("Combo / {}", combo.id),
        }));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use serde_json::json;

    fn provider(id: &str, name: &str, model: &str) -> Provider {
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

    fn combo(id: &str, specs: &[&str]) -> ModelCombo {
        ModelCombo {
            id: id.to_string(),
            targets: specs
                .iter()
                .map(|spec| parse_combo_target_spec(spec).expect("target"))
                .collect(),
            strategy: ComboStrategy::Failover,
            sticky_limit: 1,
        }
    }

    #[test]
    fn id_grammar() {
        assert!(is_valid_combo_id("main"));
        assert!(is_valid_combo_id("daily-fast"));
        assert!(is_valid_combo_id("v1.2_ok"));
        assert!(!is_valid_combo_id(""));
        assert!(!is_valid_combo_id("-bad"));
        assert!(!is_valid_combo_id("has/slash"));
        assert!(!is_valid_combo_id(&"x".repeat(65)));
    }

    #[test]
    fn parse_target_with_weight_and_inner_slash() {
        let target = parse_combo_target_spec("kimi/k2:2").unwrap();
        assert_eq!(target.provider, "kimi");
        assert_eq!(target.model, "k2");
        assert_eq!(target.weight, 2);

        let nested = parse_combo_target_spec("kimi/org/model").unwrap();
        assert_eq!(nested.model, "org/model");
        assert_eq!(nested.weight, 1);

        assert!(parse_combo_target_spec("combo/main").is_err());
        assert!(parse_combo_target_spec("kimi").is_err());
    }

    #[test]
    fn request_peel_only_when_configured() {
        assert_eq!(combo_id_from_request_model("combo/main", false), None);
        assert_eq!(
            combo_id_from_request_model("combo/main", true),
            Some("main")
        );
        assert_eq!(
            combo_id_from_request_model("anthropic/combo/main", true),
            Some("main")
        );
        assert_eq!(combo_id_from_request_model("kimi/k2", true), None);
        assert_eq!(combo_id_from_request_model("combo/", true), None);
    }

    #[test]
    fn validate_rejects_collision_and_duplicates() {
        let main = combo("main", &["kimi/k2", "deepseek/deepseek-v4"]);
        validate_combo(&main, &HashSet::new(), &HashSet::new()).unwrap();

        let mut others = HashSet::new();
        others.insert("MAIN".to_string());
        assert!(validate_combo(&main, &others, &HashSet::new()).is_err());

        let mut slugs = HashSet::new();
        slugs.insert("main".to_string());
        assert!(validate_combo(&main, &HashSet::new(), &slugs).is_err());

        let dup = combo("dup", &["kimi/k2", "kimi/k2"]);
        assert!(validate_combo(&dup, &HashSet::new(), &HashSet::new()).is_err());
    }

    #[test]
    fn resolve_skips_unknown_slug() {
        let kimi = provider("kimi", "Kimi", "k2");
        let combo = combo("main", &["kimi/k2", "missing/x"]);
        let resolved = resolve_combo_targets(&combo, &[kimi]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].provider.id, "kimi");
        assert_eq!(resolved[0].upstream_model, "k2");
    }

    #[test]
    fn swrr_two_to_one_first_picks() {
        let kimi = provider("kimi", "Kimi", "k2");
        let ds = provider("deepseek", "DeepSeek", "deepseek-v4");
        let combo = ModelCombo {
            id: "balanced".into(),
            targets: vec![
                parse_combo_target_spec("kimi/k2:2").unwrap(),
                parse_combo_target_spec("deepseek/deepseek-v4:1").unwrap(),
            ],
            strategy: ComboStrategy::RoundRobin,
            sticky_limit: 1,
        };
        let resolved = resolve_combo_targets(&combo, &[kimi, ds]);
        let mut state = HashMap::new();
        let first_picks: Vec<String> = (0..6)
            .map(|_| {
                order_round_robin("balanced", resolved.clone(), 1, &mut state)[0]
                    .provider
                    .id
                    .clone()
            })
            .collect();
        // Smooth weighted 2:1 → A, B, A, A, B, A for the first six selections.
        assert_eq!(
            first_picks,
            vec!["kimi", "deepseek", "kimi", "kimi", "deepseek", "kimi"]
        );
    }

    #[test]
    fn setting_json_roundtrip() {
        let combos = vec![combo("main", &["kimi/k2"])];
        let json = combos_to_setting_json(&combos).unwrap();
        let loaded = combos_from_setting_json(Some(&json)).unwrap();
        assert_eq!(loaded[0].id, "main");
        assert_eq!(loaded[0].targets[0].model, "k2");
        assert_eq!(combos_from_setting_json(None).unwrap().len(), 0);
    }
}
