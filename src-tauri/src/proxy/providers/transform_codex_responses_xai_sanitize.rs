//! xAI (Grok) `Responses` request field sanitization for native Responses
//! upstreams.
//!
//! Codex 0.142+ sends `wire_api="responses"` requests carrying a handful of
//! OpenAI-backend-private fields and tool carriers that xAI's strict
//! `api.x.ai/v1/responses` serde parser rejects (HTTP 400/422). cc-switch's
//! Chat/Anthropic transforms already drop these on the way through, but the
//! *native* Responses passthrough forwards the body verbatim, so we scrub them
//! here.
//!
//! This is a faithful port of sub2api's `patchGrokResponsesBody`
//! (`backend/internal/service/openai_gateway_grok.go`), the production Go
//! gateway that routes Codex → Grok subscriptions. Every transform is a
//! deterministic field removal or structural lift — no semantic rewriting — so
//! the same input always yields the same output and the upstream prompt-cache
//! prefix stays stable across requests. Gated on the xAI OAuth path only (see
//! [`super::codex::provider_needs_responses_namespace_flatten`]), so no other
//! provider is ever touched.
//!
//! Run this *after* namespace flattening: by then Codex's `namespace` tools are
//! already lifted to top-level `function` tools, so the tool-type whitelist
//! below keeps them instead of dropping them.

use std::collections::HashSet;

use serde_json::{json, Value};

/// Codex plugin-private fields removed recursively at any nesting depth.
const RECURSIVE_UNSUPPORTED_FIELDS: &[&str] = &["external_web_access"];

/// Top-level request fields xAI rejects regardless of model.
const TOP_LEVEL_UNSUPPORTED_FIELDS: &[&str] = &["prompt_cache_retention", "safety_identifier"];

/// Top-level sampling fields rejected specifically by grok-4.5.
const GROK_45_UNSUPPORTED_FIELDS: &[&str] = &[
    "presence_penalty",
    "presencePenalty",
    "frequency_penalty",
    "frequencyPenalty",
    "stop",
];

/// Tool `type` values xAI's Responses schema accepts. Sourced from xAI's own
/// serde error enumeration (which is more complete than sub2api's hand-copied
/// list — it includes `image_generation`). Any other `type` is a Codex/OpenAI
/// private carrier (`tool_search`, a stray `namespace`, `custom`, …) that the
/// strict parser would reject, so it is dropped.
const XAI_SUPPORTED_TOOL_TYPES: &[&str] = &[
    "function",
    "web_search",
    "x_search",
    "image_generation",
    "collections_search",
    "file_search",
    "code_execution",
    "code_interpreter",
    "mcp",
    "shell",
];

pub(crate) fn request_contains_tool_search(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("type").and_then(Value::as_str) == Some("tool_search"))
        })
}

fn output_has_tool_search_call(value: &Value) -> bool {
    let output = value.get("output").or_else(|| {
        value
            .get("response")
            .and_then(|response| response.get("output"))
    });
    output.and_then(Value::as_array).is_some_and(|items| {
        items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("tool_search_call"))
    })
}

fn empty_tool_search_call() -> Value {
    json!({
        "type": "tool_search_call",
        "id": "tsc_ccs_placeholder",
        "status": "completed"
    })
}

fn response_status_is_unsuccessful(value: &Value) -> bool {
    let status = value.get("status").and_then(Value::as_str).or_else(|| {
        value
            .get("response")
            .and_then(|response| response.get("status"))
            .and_then(Value::as_str)
    });
    matches!(status, Some("incomplete" | "failed" | "cancelled"))
}

pub(crate) fn inject_empty_tool_search_json(body: &mut Value) -> bool {
    if response_status_is_unsuccessful(body) || output_has_tool_search_call(body) {
        return false;
    }
    let item = empty_tool_search_call();
    if let Some(output) = body.get_mut("output").and_then(Value::as_array_mut) {
        output.push(item);
        return true;
    }
    if let Some(output) = body
        .get_mut("response")
        .and_then(|response| response.get_mut("output"))
        .and_then(Value::as_array_mut)
    {
        output.push(item);
        return true;
    }
    if let Some(object) = body.as_object_mut() {
        object.insert("output".to_string(), json!([item]));
        return true;
    }
    false
}

fn sse_has_tool_search_item(sse: &str) -> bool {
    sse.lines().any(|line| {
        let Some(data) = line.strip_prefix("data:") else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            return false;
        };
        value
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            == Some("tool_search_call")
            || value.get("type").and_then(Value::as_str) == Some("tool_search_call")
    })
}

fn max_sse_output_index(sse: &str) -> i64 {
    let mut max = -1i64;
    for line in sse.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        if let Some(index) = value.get("output_index").and_then(Value::as_i64) {
            max = max.max(index);
        }
    }
    max
}

pub(crate) fn inject_empty_tool_search_sse(sse: &str) -> String {
    if sse.contains("event: response.incomplete")
        || sse.contains("event: response.failed")
        || sse_has_tool_search_item(sse)
    {
        return sse.to_string();
    }
    let output_index = max_sse_output_index(sse).saturating_add(1).max(0);
    let item = empty_tool_search_call();
    let added = format!(
        "event: response.output_item.added\ndata: {}\n\n",
        serde_json::to_string(&json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item
        }))
        .unwrap_or_default()
    );
    let done = format!(
        "event: response.output_item.done\ndata: {}\n\n",
        serde_json::to_string(&json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item
        }))
        .unwrap_or_default()
    );
    let injected = format!("{added}{done}");
    if let Some(index) = sse.find("event: response.completed") {
        let mut out = String::with_capacity(sse.len() + injected.len());
        out.push_str(&sse[..index]);
        out.push_str(&injected);
        out.push_str(&sse[index..]);
        return out;
    }
    format!("{sse}{injected}")
}

/// Strip xAI-unsupported fields and tools from a native Codex Responses request
/// body in place. Returns whether anything changed. Deterministic and
/// idempotent: running it twice on the same body changes nothing the second
/// time.
pub(crate) fn sanitize_xai_responses_request(body: &mut Value) -> bool {
    if !body.is_object() {
        return false;
    }

    let mut changed = false;

    // 1. Top-level fields xAI rejects for every model.
    for field in TOP_LEVEL_UNSUPPORTED_FIELDS {
        changed |= remove_top_level_field(body, field);
    }

    // 2. grok-4.5 additionally rejects these sampling knobs.
    if request_targets_grok_45(body) {
        for field in GROK_45_UNSUPPORTED_FIELDS {
            changed |= remove_top_level_field(body, field);
        }
    }

    // 3. Codex plugin-private flags buried at any depth (e.g. inside tools or
    //    tool parameter schemas).
    for field in RECURSIVE_UNSUPPORTED_FIELDS {
        changed |= remove_field_recursive(body, field);
    }

    // 4. Lift the `additional_tools` input carrier (Responses Lite private
    //    shape) up to top-level `tools` so the supported ones survive.
    changed |= promote_additional_tools(body);

    // 5. Drop `content: null` on reasoning input items — xAI's untagged enum
    //    deserializer refuses a present-but-null content field.
    changed |= strip_null_reasoning_content(body);

    // 6. Whitelist the tool types and clean a now-dangling `tool_choice`.
    changed |= filter_unsupported_tools(body);

    changed
}

/// Whether the request's (possibly provider-prefixed) model resolves to
/// grok-4.5. Mirrors sub2api's suffix match: `foo/grok-4.5` counts.
fn request_targets_grok_45(body: &Value) -> bool {
    let Some(model) = body.get("model").and_then(Value::as_str) else {
        return false;
    };
    let mut model = model.trim();
    if let Some(idx) = model.rfind('/') {
        model = model[idx + 1..].trim();
    }
    model.eq_ignore_ascii_case("grok-4.5")
}

fn remove_top_level_field(body: &mut Value, field: &str) -> bool {
    body.as_object_mut()
        .and_then(|obj| obj.remove(field))
        .is_some()
}

/// Delete every occurrence of `field` in the tree, at any depth.
fn remove_field_recursive(value: &mut Value, field: &str) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = map.remove(field).is_some();
            for child in map.values_mut() {
                changed |= remove_field_recursive(child, field);
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for child in items.iter_mut() {
                changed |= remove_field_recursive(child, field);
            }
            changed
        }
        _ => false,
    }
}

fn is_additional_tools_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str).map(str::trim) == Some("additional_tools")
}

/// Promote any `additional_tools` carrier items from `input` into top-level
/// `tools`, preserving top-level order and appending carrier tools in order,
/// de-duplicated. The carrier items themselves are removed from `input`.
fn promote_additional_tools(body: &mut Value) -> bool {
    // Clone `input` up front so the later mutable write-back to `body` doesn't
    // collide with the read borrow. Only pays the clone on the rare carrier path.
    let input_items: Vec<Value> = match body.get("input").and_then(Value::as_array) {
        Some(arr) if arr.iter().any(is_additional_tools_item) => arr.clone(),
        _ => return false,
    };

    // Seed merged tools + dedup keys from the existing top-level tools.
    let mut merged: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            seen.insert(tool_dedup_key(tool));
            merged.push(tool.clone());
        }
    }

    let mut filtered_input: Vec<Value> = Vec::with_capacity(input_items.len());
    let mut promoted = false;
    for item in input_items {
        if is_additional_tools_item(&item) {
            if let Some(carrier_tools) = item.get("tools").and_then(Value::as_array) {
                for tool in carrier_tools {
                    if seen.insert(tool_dedup_key(tool)) {
                        merged.push(tool.clone());
                        promoted = true;
                    }
                }
            }
            continue; // carrier item dropped regardless of dedup outcome
        }
        filtered_input.push(item);
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert("input".to_string(), Value::Array(filtered_input));
        if promoted {
            obj.insert("tools".to_string(), Value::Array(merged));
        }
    }
    // We reached here only because a carrier existed, so `input` changed.
    true
}

/// Stable dedup key for a tool: `(type, name)`, `(mcp, server_label)`, or the
/// serialized tool as a last resort. Mirrors sub2api's `grokResponsesToolDedupKey`.
fn tool_dedup_key(tool: &Value) -> String {
    let tool_type = tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !tool_type.is_empty() {
        if let Some(name) = tool.get("name").and_then(Value::as_str) {
            let name = name.trim();
            if !name.is_empty() {
                return format!("type:{tool_type}\u{0}name:{name}");
            }
        }
        if tool_type == "mcp" {
            if let Some(label) = tool.get("server_label").and_then(Value::as_str) {
                let label = label.trim();
                if !label.is_empty() {
                    return format!("type:mcp\u{0}server_label:{label}");
                }
            }
        }
    }
    format!("json:{tool}")
}

fn strip_null_reasoning_content(body: &mut Value) -> bool {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for item in input.iter_mut() {
        if item.get("type").and_then(Value::as_str).map(str::trim) != Some("reasoning") {
            continue;
        }
        if let Some(obj) = item.as_object_mut() {
            if matches!(obj.get("content"), Some(Value::Null)) {
                obj.remove("content");
                changed = true;
            }
        }
    }
    changed
}

/// Keep only whitelisted tool types and drop a `tool_choice` that now points at
/// a removed or unsupported tool.
fn filter_unsupported_tools(body: &mut Value) -> bool {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return false;
    };
    let original_len = tools.len();
    let filtered: Vec<Value> = tools
        .iter()
        .filter(|tool| {
            let t = tool
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            XAI_SUPPORTED_TOOL_TYPES.contains(&t)
        })
        .cloned()
        .collect();

    let mut changed = false;
    if filtered.len() != original_len {
        if let Some(obj) = body.as_object_mut() {
            if filtered.is_empty() {
                obj.remove("tools");
            } else {
                obj.insert("tools".to_string(), Value::Array(filtered.clone()));
            }
        }
        changed = true;
    }

    if body.get("tool_choice").is_some() && should_drop_tool_choice(body, &filtered) {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tool_choice");
        }
        changed = true;
    }

    changed
}

/// Whether `tool_choice` should be dropped given the surviving `tools`. String
/// choices (`"auto"`, `"none"`, `"required"`) are always kept; object choices
/// are dropped when they reference an unsupported type or a function name that
/// no longer exists.
fn should_drop_tool_choice(body: &Value, tools: &[Value]) -> bool {
    let Some(tool_choice) = body.get("tool_choice") else {
        return false;
    };
    if tools.is_empty() {
        return true;
    }
    let Some(choice) = tool_choice.as_object() else {
        return false; // "auto"/"none"/"required" string choices stay
    };
    let choice_type = choice
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if choice_type.is_empty() {
        return false;
    }
    if !XAI_SUPPORTED_TOOL_TYPES.contains(&choice_type) {
        return true;
    }
    if choice_type == "function" {
        let choice_name = choice
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| {
                choice
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("")
            .trim();
        if choice_name.is_empty() {
            return false;
        }
        let exists = tools.iter().any(|tool| {
            let t = tool
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| {
                    tool.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("")
                .trim();
            t == "function" && name == choice_name
        });
        return !exists;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_external_web_access_recursively() {
        let mut body = json!({
            "model": "grok-4.5",
            "external_web_access": true,
            "tools": [
                {"type": "function", "name": "f", "external_web_access": true,
                 "parameters": {"type": "object", "q": {"external_web_access": true}}}
            ],
            "metadata": {"external_web_access": false}
        });
        assert!(sanitize_xai_responses_request(&mut body));
        let s = body.to_string();
        assert!(!s.contains("external_web_access"), "left over: {s}");
    }

    #[test]
    fn strips_top_level_unsupported_fields() {
        let mut body = json!({
            "model": "grok-4.5",
            "prompt_cache_retention": "24h",
            "safety_identifier": "abc"
        });
        assert!(sanitize_xai_responses_request(&mut body));
        assert!(body.get("prompt_cache_retention").is_none());
        assert!(body.get("safety_identifier").is_none());
    }

    #[test]
    fn strips_grok_45_only_sampling_fields() {
        let mut body = json!({
            "model": "grok-4.5",
            "presence_penalty": 0.1,
            "frequency_penalty": 0.2,
            "stop": ["x"]
        });
        assert!(sanitize_xai_responses_request(&mut body));
        assert!(body.get("presence_penalty").is_none());
        assert!(body.get("frequency_penalty").is_none());
        assert!(body.get("stop").is_none());
    }

    #[test]
    fn keeps_sampling_fields_for_non_grok_45() {
        let mut body = json!({
            "model": "grok-4-fast",
            "presence_penalty": 0.1,
            "stop": ["x"]
        });
        // No unsupported fields present, so no change and knobs preserved.
        assert!(!sanitize_xai_responses_request(&mut body));
        assert_eq!(body.get("presence_penalty"), Some(&json!(0.1)));
        assert_eq!(body.get("stop"), Some(&json!(["x"])));
    }

    #[test]
    fn matches_grok_45_with_provider_prefix() {
        let mut body = json!({"model": "xai/grok-4.5", "stop": ["x"]});
        assert!(sanitize_xai_responses_request(&mut body));
        assert!(body.get("stop").is_none());
    }

    #[test]
    fn promotes_additional_tools_dedup() {
        let mut body = json!({
            "model": "grok-4.5",
            "tools": [{"type": "function", "name": "kept"}],
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "additional_tools", "tools": [
                    {"type": "function", "name": "kept"},
                    {"type": "function", "name": "extra"}
                ]}
            ]
        });
        assert!(sanitize_xai_responses_request(&mut body));
        // carrier removed from input
        let input = body.get("input").unwrap().as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert!(input.iter().all(|i| !is_additional_tools_item(i)));
        // extra promoted, kept not duplicated
        let tools = body.get("tools").unwrap().as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(names, vec!["kept", "extra"]);
    }

    #[test]
    fn strips_null_reasoning_content() {
        let mut body = json!({
            "model": "grok-4.5",
            "input": [
                {"type": "reasoning", "content": null, "id": "r1"},
                {"type": "reasoning", "content": [{"text": "keep"}], "id": "r2"}
            ]
        });
        assert!(sanitize_xai_responses_request(&mut body));
        let input = body.get("input").unwrap().as_array().unwrap();
        assert!(input[0].get("content").is_none());
        assert!(input[1].get("content").is_some());
    }

    #[test]
    fn filters_unsupported_tool_types() {
        let mut body = json!({
            "model": "grok-4.5",
            "tools": [
                {"type": "function", "name": "f"},
                {"type": "tool_search"},
                {"type": "custom", "name": "c"},
                {"type": "mcp", "server_label": "s"}
            ]
        });
        assert!(sanitize_xai_responses_request(&mut body));
        let types: Vec<&str> = body
            .get("tools")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.get("type").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(types, vec!["function", "mcp"]);
    }

    #[test]
    fn drops_dangling_function_tool_choice() {
        let mut body = json!({
            "model": "grok-4.5",
            "tools": [{"type": "tool_search"}],
            "tool_choice": {"type": "function", "name": "gone"}
        });
        assert!(sanitize_xai_responses_request(&mut body));
        // tool_search filtered → no tools → tool_choice dropped
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn keeps_valid_function_tool_choice() {
        let mut body = json!({
            "model": "grok-4.5",
            "tools": [{"type": "function", "name": "run"}],
            "tool_choice": {"type": "function", "name": "run"}
        });
        assert!(!sanitize_xai_responses_request(&mut body));
        assert_eq!(
            body.get("tool_choice").unwrap(),
            &json!({"type": "function", "name": "run"})
        );
    }

    #[test]
    fn keeps_string_tool_choice() {
        let mut body = json!({
            "model": "grok-4.5",
            "tools": [{"type": "function", "name": "run"}],
            "tool_choice": "auto"
        });
        assert!(!sanitize_xai_responses_request(&mut body));
        assert_eq!(body.get("tool_choice").unwrap(), &json!("auto"));
    }

    #[test]
    fn noop_on_clean_request() {
        let mut body = json!({
            "model": "grok-4.5",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "tools": [{"type": "function", "name": "f"}]
        });
        assert!(!sanitize_xai_responses_request(&mut body));
    }

    #[test]
    fn idempotent_second_pass() {
        let mut body = json!({
            "model": "grok-4.5",
            "external_web_access": true,
            "prompt_cache_retention": "24h",
            "tools": [{"type": "function", "name": "f"}, {"type": "tool_search"}]
        });
        assert!(sanitize_xai_responses_request(&mut body));
        // second pass finds nothing left to change
        assert!(!sanitize_xai_responses_request(&mut body));
    }

    #[test]
    fn injects_placeholder_tool_search_call_when_missing() {
        assert!(request_contains_tool_search(&json!({
            "tools": [{"type": "tool_search"}]
        })));
        let mut body = json!({ "output": [{"type": "message"}] });
        assert!(inject_empty_tool_search_json(&mut body));
        assert_eq!(body["output"][1]["type"], "tool_search_call");
        assert!(!inject_empty_tool_search_json(&mut body));

        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\"}\n\n"
        );
        let rewritten = inject_empty_tool_search_sse(sse);
        assert!(rewritten.contains("tool_search_call"));
        assert!(
            rewritten.find("tool_search_call").unwrap()
                < rewritten.find("response.completed").unwrap()
        );
    }

    #[test]
    fn does_not_inject_tool_search_on_incomplete() {
        let mut body = json!({
            "status": "incomplete",
            "output": [{"type": "message"}]
        });
        assert!(!inject_empty_tool_search_json(&mut body));

        let sse = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.incomplete\n",
            "data: {\"type\":\"response.incomplete\"}\n\n"
        );
        let rewritten = inject_empty_tool_search_sse(sse);
        assert_eq!(rewritten, sse);
    }
}
