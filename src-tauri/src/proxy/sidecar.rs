//! OpenCodex-style web-search and vision sidecars.
//!
//! Hosted `web_search` on a non-passthrough model is rewritten to a function
//! tool and executed through ChatGPT Official or Anthropic OAuth. Images sent
//! to a text-only model are described first and replaced with text. Sidecar
//! failures become bounded tool/image markers instead of failing the turn.

use crate::database::Database;
use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::error::ProxyError;
use crate::proxy::handler_context::RequestContext;
use crate::proxy::providers::{
    is_codex_official_provider, should_convert_codex_responses_to_anthropic,
    should_convert_codex_responses_to_chat, transform, transform_codex_anthropic,
    transform_codex_chat, transform_codex_responses_namespace, transform_responses,
};
use crate::proxy::response_processor::{read_decoded_body, spawn_log_usage};
use crate::proxy::server::ProxyState;
use crate::proxy::usage::parser::TokenUsage;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use http::Extensions;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

pub const SIDECAR_SETTING_KEY: &str = "sidecar_settings";
const DEFAULT_WEB_SEARCH_MODEL_OPENAI: &str = "gpt-5.6-luna";
const DEFAULT_WEB_SEARCH_MODEL_ANTHROPIC: &str = "claude-sonnet-5";
const DEFAULT_VISION_MODEL_OPENAI: &str = "gpt-5.6-luna";
const DEFAULT_VISION_MODEL_ANTHROPIC: &str = "claude-haiku-4-5";
const DEFAULT_MAX_SEARCHES: u32 = 3;
const DEFAULT_MAX_DESCRIPTIONS: u32 = 8;
const DEFAULT_SEARCH_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_VISION_TIMEOUT_MS: u64 = 45_000;
const SEARCH_RESULT_CHAR_CAP: usize = 8_000;
const VISION_RESULT_CHAR_CAP: usize = 2_000;
const WEB_SEARCH_FUNCTION_NAME: &str = "web_search";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SidecarBackend {
    #[default]
    Auto,
    Openai,
    Anthropic,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchSidecarConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub backend: SidecarBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "default_max_searches")]
    pub max_searches_per_turn: u32,
    #[serde(default = "default_search_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for WebSearchSidecarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: SidecarBackend::Auto,
            model: None,
            max_searches_per_turn: DEFAULT_MAX_SEARCHES,
            timeout_ms: DEFAULT_SEARCH_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VisionSidecarConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub backend: SidecarBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "default_max_descriptions")]
    pub max_descriptions_per_turn: u32,
    #[serde(default = "default_vision_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for VisionSidecarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: SidecarBackend::Auto,
            model: None,
            max_descriptions_per_turn: DEFAULT_MAX_DESCRIPTIONS,
            timeout_ms: DEFAULT_VISION_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SidecarSettings {
    #[serde(default)]
    pub web_search: WebSearchSidecarConfig,
    #[serde(default)]
    pub vision: VisionSidecarConfig,
}

fn default_true() -> bool {
    true
}

fn default_max_searches() -> u32 {
    DEFAULT_MAX_SEARCHES
}

fn default_max_descriptions() -> u32 {
    DEFAULT_MAX_DESCRIPTIONS
}

fn default_search_timeout_ms() -> u64 {
    DEFAULT_SEARCH_TIMEOUT_MS
}

fn default_vision_timeout_ms() -> u64 {
    DEFAULT_VISION_TIMEOUT_MS
}

pub fn load_sidecar_settings(db: &Database) -> SidecarSettings {
    match db.get_setting(SIDECAR_SETTING_KEY) {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        _ => SidecarSettings::default(),
    }
}

pub fn save_sidecar_settings(
    db: &Database,
    settings: &SidecarSettings,
) -> Result<SidecarSettings, AppError> {
    let sanitized = sanitize_settings(settings);
    let json = serde_json::to_string(&sanitized)
        .map_err(|e| AppError::Database(format!("序列化 sidecar 配置失败: {e}")))?;
    db.set_setting(SIDECAR_SETTING_KEY, &json)?;
    Ok(sanitized)
}

fn sanitize_settings(settings: &SidecarSettings) -> SidecarSettings {
    let mut out = settings.clone();
    out.web_search.max_searches_per_turn = out.web_search.max_searches_per_turn.clamp(1, 20);
    out.web_search.timeout_ms = out.web_search.timeout_ms.clamp(1, i32::MAX as u64);
    out.vision.max_descriptions_per_turn = out.vision.max_descriptions_per_turn.min(32);
    out.vision.timeout_ms = out.vision.timeout_ms.clamp(1, i32::MAX as u64);
    if let Some(model) = out.web_search.model.as_mut() {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            out.web_search.model = None;
        } else {
            *model = trimmed.to_string();
        }
    }
    if let Some(model) = out.vision.model.as_mut() {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            out.vision.model = None;
        } else {
            *model = trimmed.to_string();
        }
    }
    out
}

pub fn request_has_hosted_web_search(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(is_hosted_web_search_tool))
}

fn is_hosted_web_search_tool(tool: &Value) -> bool {
    let Some(tool_type) = tool.get("type").and_then(Value::as_str) else {
        return false;
    };
    tool_type == "web_search" || tool_type.starts_with("web_search_")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidecarClientFormat {
    Responses,
    AnthropicMessages,
    ChatCompletions,
}

#[cfg(test)]
pub fn rewrite_hosted_web_search_to_function(body: &mut Value) -> bool {
    rewrite_hosted_web_search_to_format(body, SidecarClientFormat::Responses)
}

fn rewrite_hosted_web_search_to_format(body: &mut Value, format: SidecarClientFormat) -> bool {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for tool in tools.iter_mut() {
        if is_hosted_web_search_tool(tool) {
            *tool = function_tool_for(format);
            changed = true;
        }
    }
    changed
}

fn function_tool_for(format: SidecarClientFormat) -> Value {
    let parameters = json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query" }
        },
        "required": ["query"]
    });
    match format {
        SidecarClientFormat::Responses => json!({
            "type": "function",
            "name": WEB_SEARCH_FUNCTION_NAME,
            "description": "Search the web. Use for current events, facts, or URLs.",
            "parameters": parameters
        }),
        SidecarClientFormat::AnthropicMessages => json!({
            "name": WEB_SEARCH_FUNCTION_NAME,
            "description": "Search the web. Use for current events, facts, or URLs.",
            "input_schema": parameters
        }),
        SidecarClientFormat::ChatCompletions => json!({
            "type": "function",
            "function": {
                "name": WEB_SEARCH_FUNCTION_NAME,
                "description": "Search the web. Use for current events, facts, or URLs.",
                "parameters": parameters
            }
        }),
    }
}

fn strip_web_search_function_tool(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        tools.retain(|tool| {
            tool.get("name").and_then(Value::as_str) != Some(WEB_SEARCH_FUNCTION_NAME)
                && tool
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    != Some(WEB_SEARCH_FUNCTION_NAME)
        });
    }
}

#[derive(Debug, Clone)]
struct WebSearchCall {
    call_id: String,
    query: String,
}

fn extract_web_search_calls(responses_json: &Value) -> Vec<WebSearchCall> {
    let mut calls = Vec::new();
    if let Some(output) = responses_json.get("output").and_then(Value::as_array) {
        for item in output {
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                continue;
            }
            if item.get("name").and_then(Value::as_str) != Some(WEB_SEARCH_FUNCTION_NAME) {
                continue;
            }
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("call_web_search")
                .to_string();
            let query = query_from_arguments(item.get("arguments"));
            calls.push(WebSearchCall { call_id, query });
        }
    }
    if calls.is_empty() {
        if let Some(content) = responses_json.get("content").and_then(Value::as_array) {
            for item in content {
                if item.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                if item.get("name").and_then(Value::as_str) != Some(WEB_SEARCH_FUNCTION_NAME) {
                    continue;
                }
                let call_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_web_search")
                    .to_string();
                let query = item
                    .get("input")
                    .and_then(|input| input.get("query"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                calls.push(WebSearchCall { call_id, query });
            }
        }
    }
    if calls.is_empty() {
        if let Some(choices) = responses_json.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let Some(tool_calls) = choice
                    .pointer("/message/tool_calls")
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                for tool_call in tool_calls {
                    let name = tool_call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if name != WEB_SEARCH_FUNCTION_NAME {
                        continue;
                    }
                    let call_id = tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("call_web_search")
                        .to_string();
                    let query = query_from_arguments(tool_call.pointer("/function/arguments"));
                    calls.push(WebSearchCall { call_id, query });
                }
            }
        }
    }
    calls
}

fn query_from_arguments(arguments: Option<&Value>) -> String {
    match arguments {
        Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|parsed| {
                parsed
                    .get("query")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| raw.trim().to_string()),
        Some(Value::Object(map)) => map
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn append_search_results(
    body: &mut Value,
    calls: &[(WebSearchCall, String)],
    format: SidecarClientFormat,
) {
    match format {
        SidecarClientFormat::Responses => append_search_results_to_input(body, calls),
        SidecarClientFormat::AnthropicMessages => append_search_results_to_messages(body, calls),
        SidecarClientFormat::ChatCompletions => append_search_results_to_chat_messages(body, calls),
    }
}

fn append_search_results_to_input(body: &mut Value, calls: &[(WebSearchCall, String)]) {
    let input = if let Some(existing) = body.get_mut("input") {
        if existing.is_array() {
            existing
        } else {
            let wrapped =
                json!([{ "type": "message", "role": "user", "content": existing.take() }]);
            *existing = wrapped;
            existing
        }
    } else {
        body.as_object_mut()
            .expect("responses body")
            .insert("input".to_string(), json!([]));
        body.get_mut("input").expect("input")
    };
    let Some(items) = input.as_array_mut() else {
        return;
    };
    for (call, result) in calls {
        items.push(json!({
            "type": "function_call",
            "call_id": call.call_id,
            "name": WEB_SEARCH_FUNCTION_NAME,
            "arguments": json!({ "query": call.query }).to_string()
        }));
        items.push(json!({
            "type": "function_call_output",
            "call_id": call.call_id,
            "output": cap_chars(result, SEARCH_RESULT_CHAR_CAP)
        }));
    }
}

fn append_search_results_to_messages(body: &mut Value, calls: &[(WebSearchCall, String)]) {
    let Some(items) = ensure_array_field(body, "messages") else {
        return;
    };
    let mut tool_uses = Vec::new();
    let mut tool_results = Vec::new();
    for (call, result) in calls {
        tool_uses.push(json!({
            "type": "tool_use",
            "id": call.call_id,
            "name": WEB_SEARCH_FUNCTION_NAME,
            "input": { "query": call.query }
        }));
        tool_results.push(json!({
            "type": "tool_result",
            "tool_use_id": call.call_id,
            "content": cap_chars(result, SEARCH_RESULT_CHAR_CAP)
        }));
    }
    items.push(json!({ "role": "assistant", "content": tool_uses }));
    items.push(json!({ "role": "user", "content": tool_results }));
}

fn append_search_results_to_chat_messages(body: &mut Value, calls: &[(WebSearchCall, String)]) {
    let Some(items) = ensure_array_field(body, "messages") else {
        return;
    };
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|(call, _)| {
            json!({
                "id": call.call_id,
                "type": "function",
                "function": {
                    "name": WEB_SEARCH_FUNCTION_NAME,
                    "arguments": json!({ "query": call.query }).to_string()
                }
            })
        })
        .collect();
    items.push(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": tool_calls
    }));
    for (call, result) in calls {
        items.push(json!({
            "role": "tool",
            "tool_call_id": call.call_id,
            "content": cap_chars(result, SEARCH_RESULT_CHAR_CAP)
        }));
    }
}

fn ensure_array_field<'a>(body: &'a mut Value, field: &str) -> Option<&'a mut Vec<Value>> {
    if !body.get(field).is_some_and(Value::is_array) {
        body.as_object_mut()?.insert(field.to_string(), json!([]));
    }
    body.get_mut(field)?.as_array_mut()
}

fn cap_chars(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

pub fn request_has_images(body: &Value) -> bool {
    walk_values(body, &mut |value| is_image_part(value))
}

fn is_image_part(value: &Value) -> bool {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return value.get("image_url").is_some() || value.get("source").is_some();
    };
    matches!(kind, "input_image" | "image" | "image_url")
        || (kind == "tool_result" && value.get("image_url").is_some())
}

fn walk_values(value: &Value, predicate: &mut impl FnMut(&Value) -> bool) -> bool {
    if predicate(value) {
        return true;
    }
    match value {
        Value::Array(items) => items.iter().any(|item| walk_values(item, predicate)),
        Value::Object(map) => map.values().any(|item| walk_values(item, predicate)),
        _ => false,
    }
}

pub fn model_is_text_only(provider: &Provider, request_model: &str) -> bool {
    let explicit = provider
        .meta
        .as_ref()
        .map(|meta| meta.no_vision_models.as_slice())
        .unwrap_or(&[]);
    let candidates = model_ids_to_match(request_model);
    if !explicit.is_empty() {
        return candidates
            .iter()
            .any(|model| explicit.iter().any(|pattern| model_matches(model, pattern)));
    }
    candidates.iter().any(|model| is_builtin_text_only(model))
}

fn model_ids_to_match(request_model: &str) -> Vec<String> {
    let mut ids = vec![request_model.trim().to_string()];
    if let Some((_, rest)) = request_model.split_once('/') {
        ids.push(rest.trim().to_string());
    }
    ids.retain(|id| !id.is_empty());
    ids
}

fn model_matches(model: &str, pattern: &str) -> bool {
    let model = strip_ollama_size(model);
    let pattern = strip_ollama_size(pattern.trim());
    if pattern.is_empty() {
        return false;
    }
    model.eq_ignore_ascii_case(&pattern)
        || model
            .to_ascii_lowercase()
            .starts_with(&pattern.to_ascii_lowercase())
}

fn strip_ollama_size(model: &str) -> String {
    model.split(':').next().unwrap_or(model).trim().to_string()
}

fn is_builtin_text_only(model: &str) -> bool {
    let stem = strip_ollama_size(model).to_ascii_lowercase();
    stem.starts_with("deepseek")
        || stem.starts_with("kimi")
        || stem.starts_with("k2")
        || stem.starts_with("k3")
        || stem.starts_with("glm")
        || stem.starts_with("gpt-oss")
        || stem.contains("qwen3-coder")
}

async fn search_backend_ready(
    backend: SidecarBackend,
    state: &ProxyState,
) -> Option<SidecarBackend> {
    if test_search_url().is_some() {
        return Some(resolve_or_default(backend));
    }
    manager_backend(backend, state).await
}

async fn vision_backend_ready(
    backend: SidecarBackend,
    state: &ProxyState,
) -> Option<SidecarBackend> {
    if test_vision_url().is_some() {
        return Some(resolve_or_default(backend));
    }
    manager_backend(backend, state).await
}

fn resolve_or_default(backend: SidecarBackend) -> SidecarBackend {
    match backend {
        SidecarBackend::Auto => SidecarBackend::Anthropic,
        other => other,
    }
}

async fn manager_backend(backend: SidecarBackend, state: &ProxyState) -> Option<SidecarBackend> {
    // Tests stay inert unless a mock sidecar URL is set, so existing proxy
    // e2e never hits live ChatGPT / Anthropic from this path.
    #[cfg(test)]
    {
        let _ = (backend, state);
        return None;
    }
    #[cfg(not(test))]
    {
        let anthropic_ok = manager_has_usable_account(state.anthropic_oauth_manager.as_ref()).await;
        let openai_ok = manager_has_usable_codex(state.codex_oauth_manager.as_ref()).await;
        match backend {
            SidecarBackend::Auto => {
                if anthropic_ok {
                    Some(SidecarBackend::Anthropic)
                } else if openai_ok {
                    Some(SidecarBackend::Openai)
                } else {
                    None
                }
            }
            SidecarBackend::Anthropic if anthropic_ok => Some(SidecarBackend::Anthropic),
            SidecarBackend::Openai if openai_ok => Some(SidecarBackend::Openai),
            _ => None,
        }
    }
}

#[cfg_attr(test, allow(dead_code))]
async fn manager_has_usable_account(
    manager: Option<
        &std::sync::Arc<crate::proxy::providers::anthropic_oauth_auth::AnthropicOAuthManager>,
    >,
) -> bool {
    let Some(manager) = manager else {
        return false;
    };
    manager
        .list_accounts()
        .await
        .iter()
        .any(|account| !account.requires_reauth)
}

#[cfg_attr(test, allow(dead_code))]
async fn manager_has_usable_codex(
    manager: Option<&std::sync::Arc<crate::proxy::providers::codex_oauth_auth::CodexOAuthManager>>,
) -> bool {
    let Some(manager) = manager else {
        return false;
    };
    manager
        .list_accounts()
        .await
        .iter()
        .any(|account| !account.reauth_required)
}

fn test_search_url() -> Option<String> {
    #[cfg(test)]
    {
        std::env::var("CC_SWITCH_TEST_SIDECAR_SEARCH_URL")
            .ok()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
    }
    #[cfg(not(test))]
    None
}

fn test_vision_url() -> Option<String> {
    #[cfg(test)]
    {
        std::env::var("CC_SWITCH_TEST_SIDECAR_VISION_URL")
            .ok()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
    }
    #[cfg(not(test))]
    None
}

pub async fn should_run_web_search_sidecar(
    provider: &Provider,
    body: &Value,
    settings: &SidecarSettings,
    state: &ProxyState,
) -> bool {
    if !settings.web_search.enabled || !request_has_hosted_web_search(body) {
        return false;
    }
    if is_codex_official_provider(provider) || provider.is_anthropic_oauth() {
        return false;
    }
    search_backend_ready(settings.web_search.backend, state)
        .await
        .is_some()
}

pub async fn rewrite_vision_if_needed(
    body: &Value,
    provider: &Provider,
    request_model: &str,
    settings: &SidecarSettings,
    state: &ProxyState,
) -> Result<Value, ProxyError> {
    if !settings.vision.enabled
        || !request_has_images(body)
        || !model_is_text_only(provider, request_model)
    {
        return Ok(body.clone());
    }
    if vision_backend_ready(settings.vision.backend, state)
        .await
        .is_none()
    {
        return Ok(body.clone());
    }
    let mut rewritten = body.clone();
    let mut remaining = settings.vision.max_descriptions_per_turn;
    replace_images(&mut rewritten, &mut remaining, settings, state).await;
    Ok(rewritten)
}

async fn replace_images(
    body: &mut Value,
    remaining: &mut u32,
    settings: &SidecarSettings,
    state: &ProxyState,
) {
    let timeout = Duration::from_millis(settings.vision.timeout_ms);
    replace_image_nodes_async(body, remaining, settings, state, timeout).await;
}

async fn replace_image_nodes_async(
    value: &mut Value,
    remaining: &mut u32,
    settings: &SidecarSettings,
    state: &ProxyState,
    timeout: Duration,
) {
    match value {
        Value::Array(items) => {
            for item in items.iter_mut() {
                if is_image_node(item) {
                    apply_image_description(item, remaining, settings, state, timeout).await;
                } else {
                    Box::pin(replace_image_nodes_async(
                        item, remaining, settings, state, timeout,
                    ))
                    .await;
                }
            }
        }
        Value::Object(_) => {
            if is_image_node(value) {
                apply_image_description(value, remaining, settings, state, timeout).await;
                return;
            }
            if let Some(map) = value.as_object_mut() {
                for child in map.values_mut() {
                    Box::pin(replace_image_nodes_async(
                        child, remaining, settings, state, timeout,
                    ))
                    .await;
                }
            }
        }
        _ => {}
    }
}

async fn apply_image_description(
    value: &mut Value,
    remaining: &mut u32,
    settings: &SidecarSettings,
    state: &ProxyState,
    timeout: Duration,
) {
    if *remaining == 0 {
        *value = json!({
            "type": "input_text",
            "text": "[image omitted: description budget exhausted]"
        });
        return;
    }
    *remaining = remaining.saturating_sub(1);
    let image = image_ref(value);
    let text = describe_image(&image, settings, state, timeout).await;
    *value = text_part_for(value, &text);
}

#[cfg(test)]
fn replace_image_nodes(value: &mut Value, mut replacer: impl FnMut(&Value) -> Value) {
    replace_image_nodes_inner(value, &mut replacer);
}

#[cfg(test)]
fn replace_image_nodes_inner(value: &mut Value, replacer: &mut impl FnMut(&Value) -> Value) {
    match value {
        Value::Array(items) => {
            for item in items.iter_mut() {
                if is_image_node(item) {
                    *item = replacer(item);
                } else {
                    replace_image_nodes_inner(item, replacer);
                }
            }
        }
        Value::Object(_) => {
            if is_image_node(value) {
                *value = replacer(value);
                return;
            }
            if let Some(map) = value.as_object_mut() {
                for child in map.values_mut() {
                    replace_image_nodes_inner(child, replacer);
                }
            }
        }
        _ => {}
    }
}

fn is_image_node(value: &Value) -> bool {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return value.get("image_url").is_some();
    };
    matches!(kind, "input_image" | "image" | "image_url")
}

struct ImageRef {
    data_url: String,
}

fn image_ref(value: &Value) -> ImageRef {
    let data_url = value
        .get("image_url")
        .and_then(|url| {
            url.as_str().map(ToString::to_string).or_else(|| {
                url.get("url")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
        })
        .or_else(|| {
            value
                .get("url")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| {
            let source = value.get("source")?;
            let data = source.get("data").and_then(Value::as_str)?;
            let media = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            Some(format!("data:{media};base64,{data}"))
        })
        .unwrap_or_default();
    ImageRef { data_url }
}

fn text_part_for(original: &Value, text: &str) -> Value {
    let capped = cap_chars(text, VISION_RESULT_CHAR_CAP);
    match original.get("type").and_then(Value::as_str) {
        Some("image") => {
            json!({ "type": "text", "text": format!("[image description: {capped}]") })
        }
        _ => json!({
            "type": "input_text",
            "text": format!("[image description: {capped}]")
        }),
    }
}

async fn describe_image(
    image: &ImageRef,
    settings: &SidecarSettings,
    state: &ProxyState,
    timeout: Duration,
) -> String {
    if let Some(url) = test_vision_url() {
        return post_json(&url, json!({ "image_url": image.data_url }), timeout)
            .await
            .unwrap_or_else(|error| format!("[image processing error: {error}]"));
    }
    let backend = match vision_backend_ready(settings.vision.backend, state).await {
        Some(backend) => backend,
        None => return "[image processing error: vision sidecar backend unavailable]".to_string(),
    };
    let model = settings
        .vision
        .model
        .clone()
        .unwrap_or_else(|| match backend {
            SidecarBackend::Openai => DEFAULT_VISION_MODEL_OPENAI.to_string(),
            _ => DEFAULT_VISION_MODEL_ANTHROPIC.to_string(),
        });
    let prompt = "Describe this image concisely for a coding assistant.";
    let result = match backend {
        SidecarBackend::Openai => {
            openai_sidecar_complete(
                state,
                &model,
                json!([{
                    "type": "message",
                    "role": "user",
                    "content": [
                        { "type": "input_text", "text": prompt },
                        { "type": "input_image", "image_url": image.data_url }
                    ]
                }]),
                json!([]),
                timeout,
            )
            .await
        }
        _ => {
            anthropic_sidecar_complete(
                state,
                &model,
                json!([{
                    "role": "user",
                    "content": anthropic_image_content(prompt, &image.data_url)
                }]),
                json!([]),
                timeout,
            )
            .await
        }
    };
    result.unwrap_or_else(|error| format!("[image processing error: {error}]"))
}

async fn execute_web_search(query: &str, settings: &SidecarSettings, state: &ProxyState) -> String {
    let timeout = Duration::from_millis(settings.web_search.timeout_ms);
    if let Some(url) = test_search_url() {
        return post_json(&url, json!({ "query": query }), timeout)
            .await
            .unwrap_or_else(|error| format!("[web_search error] {error}"));
    }
    let backend = match search_backend_ready(settings.web_search.backend, state).await {
        Some(backend) => backend,
        None => {
            return format!("[web_search error] sidecar backend unavailable for query: {query}")
        }
    };
    let model = settings
        .web_search
        .model
        .clone()
        .unwrap_or_else(|| match backend {
            SidecarBackend::Openai => DEFAULT_WEB_SEARCH_MODEL_OPENAI.to_string(),
            _ => DEFAULT_WEB_SEARCH_MODEL_ANTHROPIC.to_string(),
        });
    let result = match backend {
        SidecarBackend::Openai => {
            openai_sidecar_complete(
                state,
                &model,
                json!([{
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": query }]
                }]),
                json!([{ "type": "web_search" }]),
                timeout,
            )
            .await
        }
        _ => {
            anthropic_sidecar_complete(
                state,
                &model,
                json!([{
                    "role": "user",
                    "content": query
                }]),
                json!([{ "type": "web_search_20250305", "name": "web_search" }]),
                timeout,
            )
            .await
        }
    };
    result.unwrap_or_else(|error| format!("[web_search error] {error}"))
}

#[cfg_attr(test, allow(dead_code))]
fn anthropic_image_content(prompt: &str, image_url: &str) -> Value {
    let mut content = vec![json!({ "type": "text", "text": prompt })];
    if let Some(rest) = image_url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            let media = meta.split(';').next().unwrap_or("image/png");
            content.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media,
                    "data": data
                }
            }));
            return json!(content);
        }
    }
    content.push(json!({
        "type": "image",
        "source": { "type": "url", "url": image_url }
    }));
    json!(content)
}

#[cfg_attr(test, allow(dead_code))]
fn official_codex_base_url() -> String {
    #[cfg(test)]
    {
        if let Ok(url) = std::env::var("CC_SWITCH_TEST_CODEX_OFFICIAL_BASE_URL") {
            let trimmed = url.trim().trim_end_matches('/');
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    crate::proxy::providers::CHATGPT_CODEX_BASE_URL
        .trim_end_matches('/')
        .to_string()
}

pub(crate) fn official_sidecar_headers(
    account_id: Option<&str>,
    session_id: &str,
) -> Vec<(http::HeaderName, http::HeaderValue)> {
    let mut headers = Vec::new();
    if let Some(account_id) = account_id.map(str::trim).filter(|id| !id.is_empty()) {
        if let Ok(value) = http::HeaderValue::from_str(account_id) {
            headers.push((http::HeaderName::from_static("chatgpt-account-id"), value));
        }
    }
    let session_id = session_id.trim();
    if !session_id.is_empty() {
        if let Ok(value) = http::HeaderValue::from_str(session_id) {
            headers.push((http::HeaderName::from_static("session_id"), value.clone()));
            headers.push((http::HeaderName::from_static("x-client-request-id"), value));
        }
        let window_id = format!("{session_id}:0");
        if let Ok(value) = http::HeaderValue::from_str(&window_id) {
            headers.push((http::HeaderName::from_static("x-codex-window-id"), value));
        }
    }
    if let Ok(value) = http::HeaderValue::from_str("codex_cli_rs") {
        headers.push((http::HeaderName::from_static("originator"), value));
    }
    if let Ok(value) = http::HeaderValue::from_str("0.144.1") {
        headers.push((http::HeaderName::from_static("version"), value));
    }
    headers
}

#[cfg_attr(test, allow(dead_code))]
async fn openai_sidecar_complete(
    state: &ProxyState,
    model: &str,
    input: Value,
    tools: Value,
    timeout: Duration,
) -> Result<String, String> {
    let manager = state
        .codex_oauth_manager
        .as_ref()
        .ok_or_else(|| "ChatGPT Official is not signed in".to_string())?;
    let token = manager
        .get_valid_token()
        .await
        .map_err(|error| error.to_string())?;
    let account_id = manager.default_account_id().await;
    let session_id = uuid::Uuid::new_v4().to_string();
    let url = format!("{}/responses", official_codex_base_url());
    let body = json!({
        "model": model,
        "input": input,
        "tools": tools,
        "stream": false
    });
    let mut request = crate::proxy::http_client::get()
        .post(url)
        .bearer_auth(token)
        .header("OpenAI-Beta", "responses=v1")
        .json(&body)
        .timeout(timeout);
    for (name, value) in official_sidecar_headers(account_id.as_deref(), &session_id) {
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {value}"));
    }
    Ok(extract_assistant_text(&value))
}

#[cfg_attr(test, allow(dead_code))]
async fn anthropic_sidecar_complete(
    state: &ProxyState,
    model: &str,
    messages: Value,
    tools: Value,
    timeout: Duration,
) -> Result<String, String> {
    let manager = state
        .anthropic_oauth_manager
        .as_ref()
        .ok_or_else(|| "Claude Pro/Max is not signed in".to_string())?;
    let token = manager
        .get_valid_token()
        .await
        .map_err(|error| error.to_string())?;
    let mut body = json!({
        "model": model,
        "max_tokens": 1024,
        "messages": messages,
        "stream": false
    });
    if tools.as_array().is_some_and(|items| !items.is_empty()) {
        body["tools"] = tools;
    }
    let response = crate::proxy::http_client::get()
        .post("https://api.anthropic.com/v1/messages")
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .json(&body)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {value}"));
    }
    Ok(extract_assistant_text(&value))
}

fn extract_assistant_text(value: &Value) -> String {
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        let text = join_text_parts(content);
        if !text.is_empty() {
            return text;
        }
    }
    if let Some(output) = value.get("output").and_then(Value::as_array) {
        let mut chunks = Vec::new();
        for item in output {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                let text = join_text_parts(content);
                if !text.is_empty() {
                    chunks.push(text);
                }
            }
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                chunks.push(text.to_string());
            }
        }
        if !chunks.is_empty() {
            return chunks.join("\n");
        }
    }
    if let Some(text) = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
    {
        return text.to_string();
    }
    value.to_string()
}

#[cfg_attr(test, allow(dead_code))]
fn join_text_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn post_json(url: &str, body: Value, timeout: Duration) -> Result<String, String> {
    let response = crate::proxy::http_client::get()
        .post(url)
        .json(&body)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let value = response.json::<Value>().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return Ok(text.to_string());
    }
    Ok(value.to_string())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_responses_web_search_loop(
    state: ProxyState,
    ctx: RequestContext,
    method: http::Method,
    endpoint: &str,
    body: Value,
    headers: HeaderMap,
    extensions: Extensions,
    settings: SidecarSettings,
    is_stream: bool,
) -> Result<axum::response::Response, ProxyError> {
    run_web_search_loop(
        state,
        ctx,
        method,
        endpoint,
        body,
        headers,
        extensions,
        settings,
        is_stream,
        SidecarClientFormat::Responses,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_messages_web_search_loop(
    state: ProxyState,
    ctx: RequestContext,
    method: http::Method,
    endpoint: &str,
    body: Value,
    headers: HeaderMap,
    extensions: Extensions,
    settings: SidecarSettings,
    is_stream: bool,
) -> Result<axum::response::Response, ProxyError> {
    run_web_search_loop(
        state,
        ctx,
        method,
        endpoint,
        body,
        headers,
        extensions,
        settings,
        is_stream,
        SidecarClientFormat::AnthropicMessages,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_chat_web_search_loop(
    state: ProxyState,
    ctx: RequestContext,
    method: http::Method,
    endpoint: &str,
    body: Value,
    headers: HeaderMap,
    extensions: Extensions,
    settings: SidecarSettings,
    is_stream: bool,
) -> Result<axum::response::Response, ProxyError> {
    run_web_search_loop(
        state,
        ctx,
        method,
        endpoint,
        body,
        headers,
        extensions,
        settings,
        is_stream,
        SidecarClientFormat::ChatCompletions,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_web_search_loop(
    state: ProxyState,
    mut ctx: RequestContext,
    method: http::Method,
    endpoint: &str,
    mut body: Value,
    headers: HeaderMap,
    extensions: Extensions,
    settings: SidecarSettings,
    is_stream: bool,
    format: SidecarClientFormat,
) -> Result<axum::response::Response, ProxyError> {
    let namespace_map = transform_codex_responses_namespace::namespace_restore_map(&body);
    rewrite_hosted_web_search_to_format(&mut body, format);
    let tool_context = transform_codex_chat::build_codex_tool_context_from_request(&body);
    let max_searches = settings.web_search.max_searches_per_turn;
    let mut searches = 0u32;
    let providers = ctx.get_providers();

    loop {
        let mut hop_body = body.clone();
        hop_body["stream"] = json!(false);
        if searches >= max_searches {
            strip_web_search_function_tool(&mut hop_body);
        }
        let forwarder = ctx.create_forwarder(&state);
        let mut result = match forwarder
            .forward_with_retry(
                &ctx.app_type,
                method.clone(),
                endpoint,
                hop_body,
                headers.clone(),
                extensions.clone(),
                providers.clone(),
            )
            .await
        {
            Ok(result) => result,
            Err(err) => return Err(err.error),
        };
        let _guard = result.connection_guard.take();
        ctx.provider = result.provider.clone();
        ctx.outbound_model = result.outbound_model.clone();
        let timeout = if ctx.app_config.non_streaming_timeout > 0 {
            Duration::from_secs(ctx.app_config.non_streaming_timeout as u64)
        } else {
            Duration::ZERO
        };
        let (_, status, bytes) = read_decoded_body(result.response, ctx.tag, timeout).await?;
        if !status.is_success() {
            return Err(ProxyError::UpstreamError {
                status: status.as_u16(),
                body: Some(String::from_utf8_lossy(&bytes).into_owned()),
            });
        }
        let upstream: Value = serde_json::from_slice(&bytes)
            .map_err(|e| ProxyError::TransformError(format!("sidecar hop parse failed: {e}")))?;
        let calls = extract_web_search_calls_for_hop(
            &upstream,
            format,
            &ctx.provider,
            endpoint,
            &tool_context,
        )?;
        if calls.is_empty() || searches >= max_searches {
            let client = hop_to_client(
                upstream.clone(),
                format,
                &ctx.provider,
                endpoint,
                &tool_context,
                &namespace_map,
            )?;
            log_sidecar_usage(&state, &ctx, &upstream, &client, is_stream);
            return client_json_to_response(client, format, is_stream);
        }
        let mut executed = Vec::new();
        for call in calls {
            if searches >= max_searches {
                break;
            }
            searches = searches.saturating_add(1);
            let result_text = execute_web_search(&call.query, &settings, &state).await;
            executed.push((call, result_text));
        }
        append_search_results(&mut body, &executed, format);
    }
}

fn upstream_to_responses(
    json: Value,
    provider: &Provider,
    endpoint: &str,
    tool_context: &transform_codex_chat::CodexToolContext,
) -> Result<Value, ProxyError> {
    if should_convert_codex_responses_to_anthropic(provider, endpoint) {
        return transform_codex_anthropic::anthropic_response_to_responses_with_context(
            json,
            tool_context,
        );
    }
    if should_convert_codex_responses_to_chat(provider, endpoint) {
        return transform_codex_chat::chat_completion_to_response_with_context(json, tool_context);
    }
    Ok(json)
}

fn extract_web_search_calls_for_hop(
    hop: &Value,
    format: SidecarClientFormat,
    provider: &Provider,
    endpoint: &str,
    tool_context: &transform_codex_chat::CodexToolContext,
) -> Result<Vec<WebSearchCall>, ProxyError> {
    match format {
        SidecarClientFormat::Responses => {
            let responses = upstream_to_responses(hop.clone(), provider, endpoint, tool_context)?;
            Ok(extract_web_search_calls(&responses))
        }
        _ => {
            let mut calls = extract_web_search_calls(hop);
            if calls.is_empty() {
                if let Ok(responses) =
                    upstream_to_responses(hop.clone(), provider, endpoint, tool_context)
                {
                    calls = extract_web_search_calls(&responses);
                }
            }
            Ok(calls)
        }
    }
}

fn hop_to_client(
    hop: Value,
    format: SidecarClientFormat,
    provider: &Provider,
    endpoint: &str,
    tool_context: &transform_codex_chat::CodexToolContext,
    namespace_map: &std::collections::HashMap<
        String,
        transform_codex_responses_namespace::NamespacedName,
    >,
) -> Result<Value, ProxyError> {
    match format {
        SidecarClientFormat::Responses => {
            let mut responses = upstream_to_responses(hop, provider, endpoint, tool_context)?;
            transform_codex_responses_namespace::restore_response_namespaces(
                &mut responses,
                namespace_map,
            );
            Ok(responses)
        }
        SidecarClientFormat::AnthropicMessages => hop_to_anthropic(hop),
        SidecarClientFormat::ChatCompletions => hop_to_chat(hop),
    }
}

fn hop_to_anthropic(hop: Value) -> Result<Value, ProxyError> {
    if looks_like_anthropic_message(&hop) {
        return Ok(hop);
    }
    if hop.get("choices").is_some() {
        return transform::openai_to_anthropic(hop);
    }
    if hop.get("output").is_some() {
        return transform_responses::responses_to_anthropic(hop);
    }
    Ok(hop)
}

fn hop_to_chat(hop: Value) -> Result<Value, ProxyError> {
    if hop.get("choices").is_some() {
        return Ok(hop);
    }
    if hop.get("output").is_some() || looks_like_anthropic_message(&hop) {
        let text = extract_assistant_text(&hop);
        return Ok(chat_completion_from_text(&hop, &text));
    }
    Ok(hop)
}

fn looks_like_anthropic_message(json: &Value) -> bool {
    json.get("type").and_then(Value::as_str) == Some("message")
        || (json.get("role").and_then(Value::as_str) == Some("assistant")
            && json.get("content").is_some())
}

fn chat_completion_from_text(source: &Value, text: &str) -> Value {
    json!({
        "id": source.get("id").cloned().unwrap_or(json!("chatcmpl_sidecar")),
        "object": "chat.completion",
        "model": source.get("model").cloned().unwrap_or(json!("")),
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": source.get("usage").cloned().unwrap_or(json!({}))
    })
}

fn log_sidecar_usage(
    state: &ProxyState,
    ctx: &RequestContext,
    hop: &Value,
    client: &Value,
    is_stream: bool,
) {
    let Some(usage) = TokenUsage::from_codex_response_auto(hop)
        .or_else(|| TokenUsage::from_claude_response(hop))
        .or_else(|| TokenUsage::from_openai_response(hop))
        .or_else(|| TokenUsage::from_codex_response_auto(client))
        .or_else(|| TokenUsage::from_claude_response(client))
        .or_else(|| TokenUsage::from_openai_response(client))
        .filter(TokenUsage::has_billable_tokens)
    else {
        return;
    };
    let model = usage
        .model
        .clone()
        .or_else(|| {
            client
                .get("model")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| {
            hop.get("model")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| ctx.request_model.clone());
    spawn_log_usage(
        state,
        ctx,
        usage,
        &model,
        &ctx.request_model,
        200,
        is_stream,
    );
}

fn client_json_to_response(
    response: Value,
    format: SidecarClientFormat,
    is_stream: bool,
) -> Result<axum::response::Response, ProxyError> {
    if is_stream {
        let sse = match format {
            SidecarClientFormat::Responses => responses_json_to_sse(&response),
            SidecarClientFormat::AnthropicMessages => anthropic_json_to_sse(&response),
            SidecarClientFormat::ChatCompletions => chat_json_to_sse(&response),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "text/event-stream".parse().expect("sse content type"),
        );
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            "no-cache".parse().expect("cache control"),
        );
        return Ok((headers, axum::body::Body::from(sse)).into_response());
    }
    let bytes = serde_json::to_vec(&response)
        .map_err(|e| ProxyError::TransformError(format!("serialize sidecar response: {e}")))?;
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response())
}

pub fn responses_json_to_sse(response: &Value) -> String {
    let mut created = response.clone();
    if let Some(obj) = created.as_object_mut() {
        obj.insert("status".into(), json!("in_progress"));
    }
    let mut sse = String::new();
    sse.push_str("event: response.created\ndata: ");
    sse.push_str(&json!({"type":"response.created","response": created}).to_string());
    sse.push_str("\n\n");
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for (index, item) in output.iter().enumerate() {
            sse.push_str("event: response.output_item.done\ndata: ");
            sse.push_str(
                &json!({
                    "type": "response.output_item.done",
                    "output_index": index,
                    "item": item
                })
                .to_string(),
            );
            sse.push_str("\n\n");
            if let Some(text) = item
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .or_else(|| item.get("text").and_then(Value::as_str))
            {
                sse.push_str("event: response.output_text.delta\ndata: ");
                sse.push_str(
                    &json!({
                        "type": "response.output_text.delta",
                        "delta": text,
                        "output_index": index,
                        "content_index": 0
                    })
                    .to_string(),
                );
                sse.push_str("\n\n");
            }
        }
    }
    sse.push_str("event: response.completed\ndata: ");
    sse.push_str(&json!({"type":"response.completed","response": response}).to_string());
    sse.push_str("\n\n");
    sse
}

fn anthropic_json_to_sse(response: &Value) -> String {
    let text = extract_assistant_text(response);
    let mut message = response.clone();
    if let Some(obj) = message.as_object_mut() {
        obj.insert("content".into(), json!([]));
        obj.insert("stop_reason".into(), json!(null));
    }
    let mut sse = String::new();
    sse.push_str("event: message_start\ndata: ");
    sse.push_str(&json!({"type":"message_start","message": message}).to_string());
    sse.push_str("\n\n");
    sse.push_str("event: content_block_start\ndata: ");
    sse.push_str(
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        })
        .to_string(),
    );
    sse.push_str("\n\n");
    if !text.is_empty() {
        sse.push_str("event: content_block_delta\ndata: ");
        sse.push_str(
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": text }
            })
            .to_string(),
        );
        sse.push_str("\n\n");
    }
    sse.push_str("event: content_block_stop\ndata: ");
    sse.push_str(&json!({"type":"content_block_stop","index":0}).to_string());
    sse.push_str("\n\n");
    sse.push_str("event: message_delta\ndata: ");
    sse.push_str(
        &json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": response.get("usage").cloned().unwrap_or(json!({}))
        })
        .to_string(),
    );
    sse.push_str("\n\n");
    sse.push_str("event: message_stop\ndata: ");
    sse.push_str(&json!({"type":"message_stop"}).to_string());
    sse.push_str("\n\n");
    sse
}

fn chat_json_to_sse(response: &Value) -> String {
    let text = extract_assistant_text(response);
    let chunk = json!({
        "id": response.get("id").cloned().unwrap_or(json!("chatcmpl_sidecar")),
        "object": "chat.completion.chunk",
        "model": response.get("model").cloned().unwrap_or(json!("")),
        "choices": [{
            "index": 0,
            "delta": { "content": text },
            "finish_reason": "stop"
        }]
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderMeta;

    fn deepseek_card() -> Provider {
        Provider::with_id(
            "deepseek".into(),
            "DeepSeek".into(),
            json!({
                "auth": { "OPENAI_API_KEY": "k" },
                "modelCatalog": { "models": [{ "model": "deepseek-v4" }] }
            }),
            None,
        )
    }

    #[test]
    fn hosted_web_search_rewrites_to_function_tool() {
        let mut body = json!({
            "model": "deepseek-v4",
            "tools": [{ "type": "web_search" }, { "type": "function", "name": "apply_patch" }]
        });
        assert!(request_has_hosted_web_search(&body));
        assert!(rewrite_hosted_web_search_to_function(&mut body));
        assert_eq!(body["tools"][0]["name"], WEB_SEARCH_FUNCTION_NAME);
        assert_eq!(body["tools"][1]["name"], "apply_patch");
        assert!(!request_has_hosted_web_search(&body));
    }

    #[test]
    fn extracts_function_call_query_from_responses_output() {
        let json = json!({
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "web_search",
                "arguments": "{\"query\":\"rust async\"}"
            }]
        });
        let calls = extract_web_search_calls(&json);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].query, "rust async");
    }

    #[test]
    fn hosted_web_search_rewrites_to_anthropic_and_chat_tools() {
        let mut messages = json!({
            "tools": [{ "type": "web_search_20250305", "name": "web_search" }]
        });
        assert!(rewrite_hosted_web_search_to_format(
            &mut messages,
            SidecarClientFormat::AnthropicMessages
        ));
        assert_eq!(messages["tools"][0]["name"], WEB_SEARCH_FUNCTION_NAME);
        assert!(messages["tools"][0].get("input_schema").is_some());
        assert!(messages["tools"][0].get("type").is_none());

        let mut chat = json!({ "tools": [{ "type": "web_search" }] });
        assert!(rewrite_hosted_web_search_to_format(
            &mut chat,
            SidecarClientFormat::ChatCompletions
        ));
        assert_eq!(
            chat["tools"][0]["function"]["name"],
            WEB_SEARCH_FUNCTION_NAME
        );
    }

    #[test]
    fn appends_function_call_output_to_input() {
        let mut body = json!({ "input": "hello", "tools": [] });
        append_search_results_to_input(
            &mut body,
            &[(
                WebSearchCall {
                    call_id: "call_1".into(),
                    query: "rust".into(),
                },
                "docs.rs".into(),
            )],
        );
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["output"], "docs.rs");
    }

    #[test]
    fn appends_tool_use_and_tool_result_to_messages() {
        let mut body = json!({
            "messages": [{ "role": "user", "content": "hello" }]
        });
        append_search_results_to_messages(
            &mut body,
            &[(
                WebSearchCall {
                    call_id: "toolu_1".into(),
                    query: "rust".into(),
                },
                "docs.rs".into(),
            )],
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["content"], "docs.rs");
    }

    #[test]
    fn appends_tool_calls_to_chat_messages() {
        let mut body = json!({
            "messages": [{ "role": "user", "content": "hello" }]
        });
        append_search_results_to_chat_messages(
            &mut body,
            &[(
                WebSearchCall {
                    call_id: "call_1".into(),
                    query: "rust".into(),
                },
                "docs.rs".into(),
            )],
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["content"], "docs.rs");
    }

    #[test]
    fn official_sidecar_headers_include_account_and_session() {
        let headers = official_sidecar_headers(Some("acct-123"), "sess-abc");
        let names: Vec<_> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap().to_string()))
            .collect();
        assert!(names.contains(&("chatgpt-account-id", "acct-123".into())));
        assert!(names.contains(&("session_id", "sess-abc".into())));
        assert!(names.contains(&("x-client-request-id", "sess-abc".into())));
        assert!(names.contains(&("x-codex-window-id", "sess-abc:0".into())));
        assert!(names.contains(&("originator", "codex_cli_rs".into())));
        assert!(names.contains(&("version", "0.144.1".into())));
    }

    #[test]
    fn hop_to_anthropic_converts_chat_and_responses() {
        let chat = hop_to_anthropic(json!({
            "id": "cmpl_1",
            "choices": [{
                "message": { "role": "assistant", "content": "hi from chat" },
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        assert_eq!(chat["content"][0]["text"], "hi from chat");

        let responses = hop_to_anthropic(json!({
            "id": "resp_1",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "hi from responses" }]
            }]
        }))
        .unwrap();
        assert_eq!(responses["content"][0]["text"], "hi from responses");
    }

    #[test]
    fn hop_to_chat_wraps_responses_text() {
        let chat = hop_to_chat(json!({
            "id": "resp_1",
            "model": "gpt-5.5",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "wrapped" }]
            }]
        }))
        .unwrap();
        assert_eq!(chat["choices"][0]["message"]["content"], "wrapped");
    }

    #[test]
    fn builtin_text_only_matches_deepseek_and_kimi() {
        let provider = deepseek_card();
        assert!(model_is_text_only(&provider, "deepseek-v4"));
        assert!(model_is_text_only(&provider, "deepseek/deepseek-v4"));
        assert!(!model_is_text_only(&provider, "gpt-5.5"));
    }

    #[test]
    fn explicit_no_vision_models_override_builtin() {
        let mut provider = deepseek_card();
        provider.meta = Some(ProviderMeta {
            no_vision_models: vec!["only-this".into()],
            ..Default::default()
        });
        assert!(!model_is_text_only(&provider, "deepseek-v4"));
        assert!(model_is_text_only(&provider, "only-this:120b"));
    }

    #[test]
    fn vision_rewrite_replaces_input_image_with_text() {
        let mut body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "what is this" },
                    { "type": "input_image", "image_url": "data:image/png;base64,aaa" }
                ]
            }]
        });
        replace_image_nodes(&mut body, |_| {
            json!({
                "type": "input_text",
                "text": "[image description: a red square]"
            })
        });
        assert_eq!(
            body["input"][0]["content"][1]["text"],
            "[image description: a red square]"
        );
    }

    #[test]
    fn sse_contains_completed_event() {
        let sse = responses_json_to_sse(&json!({
            "id": "resp_1",
            "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "hi" }] }]
        }));
        assert!(sse.contains("event: response.completed"));
        assert!(sse.contains("response.output_text.delta"));
    }

    #[test]
    fn load_sidecar_settings_defaults_when_missing() {
        let db = crate::database::Database::memory().unwrap();
        let settings = load_sidecar_settings(&db);
        assert!(settings.web_search.enabled);
        assert!(settings.vision.enabled);
        assert_eq!(settings.web_search.backend, SidecarBackend::Auto);
    }

    struct RestoreEnv {
        key: &'static str,
        original: Option<String>,
    }

    impl RestoreEnv {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    struct TempHome {
        #[allow(dead_code)]
        dir: tempfile::TempDir,
        original_home: Option<String>,
        original_userprofile: Option<String>,
        original_test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().expect("temp home");
            let original_home = std::env::var("HOME").ok();
            let original_userprofile = std::env::var("USERPROFILE").ok();
            let original_test_home = std::env::var("CC_SWITCH_TEST_HOME").ok();
            std::env::set_var("HOME", dir.path());
            std::env::set_var("USERPROFILE", dir.path());
            std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
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
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.original_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
            match &self.original_test_home {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn hosted_web_search_sidecar_loop_runs_against_function_call() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let codex_dir = crate::codex_config::get_codex_config_dir();
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        std::fs::write(
            codex_dir.join("models_cache.json"),
            serde_json::to_string(&json!({
                "models": [{
                    "slug": "gpt-5.5",
                    "display_name": "GPT-5.5",
                    "model_messages": { "instructions_template": "t" },
                    "additional_speed_tiers": [],
                    "context_window": 128000
                }]
            }))
            .unwrap(),
        )
        .expect("write models cache");

        let search_app = axum::Router::new().route(
            "/",
            axum::routing::post(|| async {
                axum::Json(json!({ "text": "search hit: rust async" }))
            }),
        );
        let search_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind search mock");
        let search_addr = search_listener.local_addr().expect("search addr");
        let search_handle = tokio::spawn(async move {
            axum::serve(search_listener, search_app).await.ok();
        });
        let _search_env = RestoreEnv::set(
            "CC_SWITCH_TEST_SIDECAR_SEARCH_URL",
            &format!("http://{search_addr}/"),
        );

        let upstream_handler = |axum::Json(body): axum::Json<Value>| async move {
            let has_tool_output =
                body.get("input")
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                        })
                    });
            let json = if has_tool_output {
                json!({
                    "id": "resp_final",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "final from search" }]
                    }]
                })
            } else {
                json!({
                    "id": "resp_tool",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "web_search",
                        "arguments": "{\"query\":\"rust async\"}"
                    }]
                })
            };
            axum::Json(json)
        };
        let upstream_app = axum::Router::new()
            .route("/v1/responses", axum::routing::post(upstream_handler))
            .route("/responses", axum::routing::post(upstream_handler));
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream mock");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        let upstream_handle = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.ok();
        });

        let db = std::sync::Arc::new(crate::database::Database::memory().expect("db"));
        let mut proxy_config = db.get_proxy_config().await.expect("proxy config");
        proxy_config.listen_port = 0;
        db.update_proxy_config(proxy_config)
            .await
            .expect("ephemeral port");

        let config = format!(
            "model_provider = \"deepseek\"\n\
             model = \"deepseek-v4\"\n\n\
             [model_providers.deepseek]\n\
             name = \"DeepSeek\"\n\
             base_url = \"http://{upstream_addr}/v1\"\n\
             wire_api = \"responses\"\n\
             requires_openai_auth = true\n"
        );
        let mut provider = Provider::with_id(
            "deepseek".into(),
            "DeepSeek".into(),
            json!({
                "auth": { "OPENAI_API_KEY": "k" },
                "config": config,
                "modelCatalog": { "models": [{ "model": "deepseek-v4" }] }
            }),
            None,
        );
        provider.sort_index = Some(0);
        db.save_provider("codex", &provider).expect("save provider");
        db.set_current_provider("codex", "deepseek")
            .expect("set current");
        crate::settings::set_current_provider(&crate::app_config::AppType::Codex, Some("deepseek"))
            .expect("set local current");
        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": "k" }),
            Some(&config),
        )
        .expect("seed live config");

        let service = crate::services::ProxyService::new(db);
        service
            .set_takeover_for_app("codex", true)
            .await
            .expect("takeover");
        let status = service.get_status().await.expect("status");
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/v1/responses", status.port))
            .header("Authorization", "Bearer PROXY_MANAGED")
            .json(&json!({
                "model": "deepseek-v4",
                "input": "search rust",
                "tools": [{ "type": "web_search" }],
                "stream": false
            }))
            .send()
            .await
            .expect("POST responses");
        let status_code = response.status();
        let body: Value = response.json().await.expect("parse response");
        assert!(
            status_code.is_success(),
            "sidecar loop should succeed, got {status_code}: {body}"
        );
        let text = body
            .pointer("/output/0/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(text, "final from search");

        service.set_takeover_for_app("codex", false).await.ok();
        search_handle.abort();
        upstream_handle.abort();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn hosted_web_search_sidecar_loop_runs_on_messages() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let search_app = axum::Router::new().route(
            "/",
            axum::routing::post(|| async { axum::Json(json!({ "text": "search hit" })) }),
        );
        let search_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind search mock");
        let search_addr = search_listener.local_addr().expect("search addr");
        let search_handle = tokio::spawn(async move {
            axum::serve(search_listener, search_app).await.ok();
        });
        let _search_env = RestoreEnv::set(
            "CC_SWITCH_TEST_SIDECAR_SEARCH_URL",
            &format!("http://{search_addr}/"),
        );

        let upstream_handler = |axum::Json(body): axum::Json<Value>| async move {
            let has_tool_result =
                body.get("messages")
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            item.get("content")
                                .and_then(Value::as_array)
                                .is_some_and(|parts| {
                                    parts.iter().any(|part| {
                                        part.get("type").and_then(Value::as_str)
                                            == Some("tool_result")
                                    })
                                })
                        })
                    });
            let json = if has_tool_result {
                json!({
                    "id": "msg_final",
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "final from search" }],
                    "stop_reason": "end_turn"
                })
            } else {
                json!({
                    "id": "msg_tool",
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "web_search",
                        "input": { "query": "rust async" }
                    }],
                    "stop_reason": "tool_use"
                })
            };
            axum::Json(json)
        };
        let upstream_app =
            axum::Router::new().route("/v1/messages", axum::routing::post(upstream_handler));
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream mock");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        let upstream_handle = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.ok();
        });

        let db = std::sync::Arc::new(crate::database::Database::memory().expect("db"));
        let mut proxy_config = db.get_proxy_config().await.expect("proxy config");
        proxy_config.listen_port = 0;
        db.update_proxy_config(proxy_config)
            .await
            .expect("ephemeral port");

        let mut provider = Provider::with_id(
            "deepseek".into(),
            "DeepSeek".into(),
            json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": "k",
                    "ANTHROPIC_BASE_URL": format!("http://{upstream_addr}")
                }
            }),
            None,
        );
        provider.sort_index = Some(0);
        db.save_provider("claude", &provider)
            .expect("save provider");
        db.set_current_provider("claude", "deepseek")
            .expect("set current");
        crate::settings::set_current_provider(
            &crate::app_config::AppType::Claude,
            Some("deepseek"),
        )
        .expect("set local current");

        let claude_settings = crate::config::get_claude_settings_path();
        if let Some(parent) = claude_settings.parent() {
            std::fs::create_dir_all(parent).expect("create claude dir");
        }
        crate::config::write_json_file(
            &claude_settings,
            &json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": "k",
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
                }
            }),
        )
        .expect("seed live config");

        let service = crate::services::ProxyService::new(db);
        service
            .set_takeover_for_app("claude", true)
            .await
            .expect("takeover");
        let status = service.get_status().await.expect("status");
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/v1/messages", status.port))
            .header("Authorization", "Bearer PROXY_MANAGED")
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": "deepseek-v4",
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "search rust" }],
                "tools": [{ "type": "web_search_20250305", "name": "web_search" }],
                "stream": false
            }))
            .send()
            .await
            .expect("POST messages");
        let status_code = response.status();
        let body: Value = response.json().await.expect("parse response");
        assert!(
            status_code.is_success(),
            "messages sidecar loop should succeed, got {status_code}: {body}"
        );
        let text = body
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(text, "final from search");

        service.set_takeover_for_app("claude", false).await.ok();
        search_handle.abort();
        upstream_handle.abort();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn hosted_web_search_sidecar_loop_runs_on_chat_completions() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let codex_dir = crate::codex_config::get_codex_config_dir();
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        std::fs::write(
            codex_dir.join("models_cache.json"),
            serde_json::to_string(&json!({
                "models": [{
                    "slug": "gpt-5.5",
                    "display_name": "GPT-5.5",
                    "model_messages": { "instructions_template": "t" },
                    "additional_speed_tiers": [],
                    "context_window": 128000
                }]
            }))
            .unwrap(),
        )
        .expect("write models cache");

        let search_app = axum::Router::new().route(
            "/",
            axum::routing::post(|| async { axum::Json(json!({ "text": "search hit" })) }),
        );
        let search_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind search mock");
        let search_addr = search_listener.local_addr().expect("search addr");
        let search_handle = tokio::spawn(async move {
            axum::serve(search_listener, search_app).await.ok();
        });
        let _search_env = RestoreEnv::set(
            "CC_SWITCH_TEST_SIDECAR_SEARCH_URL",
            &format!("http://{search_addr}/"),
        );

        let upstream_handler = |axum::Json(body): axum::Json<Value>| async move {
            let has_tool_result =
                body.get("messages")
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item.get("role").and_then(Value::as_str) == Some("tool"))
                    });
            let json = if has_tool_result {
                json!({
                    "id": "cmpl_final",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "final from search" },
                        "finish_reason": "stop"
                    }]
                })
            } else {
                json!({
                    "id": "cmpl_tool",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "web_search",
                                    "arguments": "{\"query\":\"rust async\"}"
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }]
                })
            };
            axum::Json(json)
        };
        let upstream_app = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(upstream_handler),
            )
            .route("/chat/completions", axum::routing::post(upstream_handler));
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream mock");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        let upstream_handle = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.ok();
        });

        let db = std::sync::Arc::new(crate::database::Database::memory().expect("db"));
        let mut proxy_config = db.get_proxy_config().await.expect("proxy config");
        proxy_config.listen_port = 0;
        db.update_proxy_config(proxy_config)
            .await
            .expect("ephemeral port");

        let config = format!(
            "model_provider = \"deepseek\"\n\
             model = \"deepseek-v4\"\n\n\
             [model_providers.deepseek]\n\
             name = \"DeepSeek\"\n\
             base_url = \"http://{upstream_addr}/v1\"\n\
             wire_api = \"chat\"\n\
             requires_openai_auth = true\n"
        );
        let mut provider = Provider::with_id(
            "deepseek".into(),
            "DeepSeek".into(),
            json!({
                "auth": { "OPENAI_API_KEY": "k" },
                "config": config,
                "modelCatalog": { "models": [{ "model": "deepseek-v4" }] }
            }),
            None,
        );
        provider.sort_index = Some(0);
        db.save_provider("codex", &provider).expect("save provider");
        db.set_current_provider("codex", "deepseek")
            .expect("set current");
        crate::settings::set_current_provider(&crate::app_config::AppType::Codex, Some("deepseek"))
            .expect("set local current");
        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": "k" }),
            Some(&config),
        )
        .expect("seed live config");

        let service = crate::services::ProxyService::new(db);
        service
            .set_takeover_for_app("codex", true)
            .await
            .expect("takeover");
        let status = service.get_status().await.expect("status");
        let response = reqwest::Client::new()
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                status.port
            ))
            .header("Authorization", "Bearer PROXY_MANAGED")
            .json(&json!({
                "model": "deepseek-v4",
                "messages": [{ "role": "user", "content": "search rust" }],
                "tools": [{ "type": "web_search" }],
                "stream": false
            }))
            .send()
            .await
            .expect("POST chat completions");
        let status_code = response.status();
        let body: Value = response.json().await.expect("parse response");
        assert!(
            status_code.is_success(),
            "chat sidecar loop should succeed, got {status_code}: {body}"
        );
        let text = body
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(text, "final from search");

        service.set_takeover_for_app("codex", false).await.ok();
        search_handle.abort();
        upstream_handle.abort();
    }
}
