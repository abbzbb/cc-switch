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
//! gateway that routes Codex → Grok subscriptions, plus a deterministic
//! function-tool parameter rewrite for Grok's sampling grammar. Field removals
//! and schema rewrites are pure functions of the request body so the prompt-
//! cache prefix stays stable across requests. Gated on xAI-style hosts or Grok
//! model slugs (see [`super::codex::needs_strict_xai_responses_compat`]).
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

    // 7. Grok's grammar compiler requires each function tool's parameter root
    //    to be an object or a union of objects. Codex App Tools such as
    //    `automation_update` ship root `oneOf`/`anyOf` with a `null` (or other
    //    non-object) branch, which 400s the entire request before sampling.
    changed |= normalize_function_tool_schemas(body);

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

const MAX_UNION_VARIANTS: usize = 24;
const MAX_UNION_DEPTH: usize = 8;

fn normalize_function_tool_schemas(body: &mut Value) -> bool {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return false;
    };
    let original = tools.clone();
    let mut kept: Vec<Value> = Vec::with_capacity(original.len());
    let mut changed = false;
    for mut tool in original {
        if tool.get("type").and_then(Value::as_str).map(str::trim) != Some("function") {
            kept.push(tool);
            continue;
        }
        match rewrite_function_tool_parameters(&mut tool) {
            SchemaRewrite::Unchanged => kept.push(tool),
            SchemaRewrite::Rewritten => {
                changed = true;
                kept.push(tool);
            }
            SchemaRewrite::Drop { name, reason } => {
                changed = true;
                log::warn!(
                    "Dropped incompatible tool schema for Grok Responses upstream: {name} reason: {reason}"
                );
            }
        }
    }
    if !changed {
        return false;
    }
    if let Some(obj) = body.as_object_mut() {
        if kept.is_empty() {
            obj.remove("tools");
        } else {
            obj.insert("tools".to_string(), Value::Array(kept.clone()));
        }
    }
    if body.get("tool_choice").is_some() && should_drop_tool_choice(body, &kept) {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tool_choice");
        }
    }
    true
}

enum SchemaRewrite {
    Unchanged,
    Rewritten,
    Drop { name: String, reason: &'static str },
}

fn function_tool_name(tool: &Value) -> String {
    tool.get("name")
        .and_then(Value::as_str)
        .or_else(|| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
        })
        .unwrap_or("<unnamed>")
        .to_string()
}

fn function_parameters(tool: &Value) -> Option<&Value> {
    tool.get("function")
        .and_then(|function| function.get("parameters"))
        .or_else(|| tool.get("parameters"))
}

fn set_function_parameters(tool: &mut Value, parameters: Value) {
    if let Some(function) = tool
        .get_mut("function")
        .and_then(|value| value.as_object_mut())
    {
        function.insert("parameters".to_string(), parameters);
        return;
    }
    if let Some(obj) = tool.as_object_mut() {
        obj.insert("parameters".to_string(), parameters);
    }
}

fn rewrite_function_tool_parameters(tool: &mut Value) -> SchemaRewrite {
    let name = function_tool_name(tool);
    let Some(params) = function_parameters(tool) else {
        return SchemaRewrite::Unchanged;
    };
    match normalize_tool_parameters(params) {
        Some(normalized) if &normalized == params => SchemaRewrite::Unchanged,
        Some(normalized) => {
            set_function_parameters(tool, normalized);
            SchemaRewrite::Rewritten
        }
        None => SchemaRewrite::Drop {
            name,
            reason: "root anyOf/oneOf is not supported",
        },
    }
}

fn normalize_tool_parameters(params: &Value) -> Option<Value> {
    match params {
        Value::Null => Some(json!({"type": "object", "properties": {}})),
        Value::Object(_) => normalize_root_schema(params),
        _ => None,
    }
}

fn normalize_root_schema(schema: &Value) -> Option<Value> {
    if root_schema_already_legal(schema) {
        return Some(schema.clone());
    }

    let defs = schema
        .get("$defs")
        .cloned()
        .or_else(|| schema.get("definitions").cloned());
    let mut variants = Vec::new();
    if !collect_object_variants(schema, defs.as_ref(), &mut variants, 0)
        || variants.len() > MAX_UNION_VARIANTS
    {
        return None;
    }
    if variants.is_empty() {
        return None;
    }
    for variant in &mut variants {
        ensure_object_type(variant);
    }

    if variants.len() == 1 {
        let mut object = variants.pop().unwrap();
        attach_root_metadata(&mut object, schema, defs);
        return Some(object);
    }

    let mut out = serde_json::Map::new();
    if let Some(defs) = defs {
        out.insert("$defs".to_string(), defs);
    }
    if let Some(description) = schema.get("description").cloned() {
        out.insert("description".to_string(), description);
    }
    out.insert("oneOf".to_string(), Value::Array(variants));
    Some(Value::Object(out))
}

fn root_schema_already_legal(schema: &Value) -> bool {
    if let Some(branches) = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array)
    {
        return !branches.is_empty() && branches.iter().all(is_direct_object_branch);
    }
    is_object_schema(schema) && schema.get("type").and_then(Value::as_str) == Some("object")
}

fn is_direct_object_branch(schema: &Value) -> bool {
    is_object_schema(schema)
        && !is_null_schema(schema)
        && schema.get("oneOf").is_none()
        && schema.get("anyOf").is_none()
}

fn is_null_schema(schema: &Value) -> bool {
    if schema.is_null() {
        return true;
    }
    match schema.get("type") {
        Some(Value::String(kind)) => kind == "null",
        Some(Value::Array(kinds)) => {
            !kinds.is_empty() && kinds.iter().all(|kind| kind.as_str() == Some("null"))
        }
        _ => false,
    }
}

fn is_object_schema(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("object")),
        None => {
            schema.get("properties").is_some()
                || schema.get("required").is_some()
                || schema.get("additionalProperties").is_some()
        }
        _ => false,
    }
}

fn ensure_object_type(schema: &mut Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    match obj.get("type") {
        Some(Value::String(kind)) if kind == "object" => {}
        Some(Value::Array(kinds)) => {
            let filtered: Vec<Value> = kinds
                .iter()
                .filter(|kind| kind.as_str() != Some("null"))
                .cloned()
                .collect();
            if filtered.iter().any(|kind| kind.as_str() == Some("object")) {
                obj.insert("type".to_string(), json!("object"));
            }
        }
        _ => {
            obj.insert("type".to_string(), json!("object"));
        }
    }
}

fn attach_root_metadata(target: &mut Value, original: &Value, defs: Option<Value>) {
    let Some(obj) = target.as_object_mut() else {
        return;
    };
    if let Some(defs) = defs {
        obj.entry("$defs".to_string()).or_insert(defs);
    }
    if let Some(description) = original.get("description") {
        obj.entry("description".to_string())
            .or_insert(description.clone());
    }
}

fn collect_object_variants(
    schema: &Value,
    defs: Option<&Value>,
    out: &mut Vec<Value>,
    depth: usize,
) -> bool {
    if depth > MAX_UNION_DEPTH || out.len() > MAX_UNION_VARIANTS {
        return false;
    }
    let resolved = resolve_local_ref(schema, defs).unwrap_or_else(|| schema.clone());
    if is_null_schema(&resolved) {
        return true;
    }

    if let Some(branches) = resolved
        .get("oneOf")
        .or_else(|| resolved.get("anyOf"))
        .and_then(Value::as_array)
    {
        for branch in branches {
            if !collect_object_variants(branch, defs, out, depth + 1) {
                return false;
            }
        }
        return true;
    }

    if let Some(parts) = resolved.get("allOf").and_then(Value::as_array) {
        let mut groups: Vec<Vec<Value>> = Vec::new();
        for part in parts {
            let mut group = Vec::new();
            if !collect_object_variants(part, defs, &mut group, depth + 1) {
                return false;
            }
            if !group.is_empty() {
                groups.push(group);
            }
        }
        if groups.is_empty() {
            return true;
        }
        let merged = cartesian_merge_objects(&groups);
        if out.len() + merged.len() > MAX_UNION_VARIANTS {
            return false;
        }
        out.extend(merged);
        return true;
    }

    if is_object_schema(&resolved) {
        let mut object = resolved;
        strip_union_keys(&mut object);
        ensure_object_type(&mut object);
        out.push(object);
        return true;
    }

    true
}

fn strip_union_keys(schema: &mut Value) {
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("oneOf");
        obj.remove("anyOf");
        obj.remove("allOf");
    }
}

fn resolve_local_ref(schema: &Value, defs: Option<&Value>) -> Option<Value> {
    let reference = schema.get("$ref").and_then(Value::as_str)?;
    let name = reference
        .strip_prefix("#/$defs/")
        .or_else(|| reference.strip_prefix("#/definitions/"))?;
    defs.and_then(Value::as_object)
        .and_then(|map| map.get(name))
        .cloned()
}

fn cartesian_merge_objects(groups: &[Vec<Value>]) -> Vec<Value> {
    let Some((first, rest)) = groups.split_first() else {
        return Vec::new();
    };
    let mut acc = first.clone();
    for group in rest {
        let mut next = Vec::new();
        for left in &acc {
            for right in group {
                next.push(merge_object_schemas(left, right));
                if next.len() > MAX_UNION_VARIANTS {
                    return next;
                }
            }
        }
        acc = next;
    }
    acc
}

fn merge_object_schemas(left: &Value, right: &Value) -> Value {
    let mut merged = left.clone();
    let Some(obj) = merged.as_object_mut() else {
        return right.clone();
    };
    if let Some(right_props) = right.get("properties").and_then(Value::as_object) {
        let left_props = obj
            .entry("properties".to_string())
            .or_insert_with(|| json!({}));
        if let Some(map) = left_props.as_object_mut() {
            for (key, value) in right_props {
                map.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(right_required) = right.get("required").and_then(Value::as_array) {
        let mut required = obj
            .get("required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for item in right_required {
            if !required.contains(item) {
                required.push(item.clone());
            }
        }
        obj.insert("required".to_string(), Value::Array(required));
    }
    if right.get("additionalProperties") == Some(&json!(false)) {
        obj.insert("additionalProperties".to_string(), json!(false));
    }
    ensure_object_type(&mut merged);
    merged
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

    #[test]
    fn keeps_root_oneof_when_every_branch_is_an_object() {
        let mut body = json!({
            "model": "grok-4.6",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "mode": { "const": "view" },
                                "id": { "type": "string" }
                            },
                            "required": ["mode", "id"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "mode": { "const": "delete" },
                                "id": { "type": "string" }
                            },
                            "required": ["mode", "id"]
                        }
                    ]
                }
            }]
        });
        assert!(!sanitize_xai_responses_request(&mut body));
        assert!(body["tools"][0]["parameters"].get("type").is_none());
        assert_eq!(
            body["tools"][0]["parameters"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn drops_null_branch_from_root_anyof() {
        let mut body = json!({
            "model": "grok-4.6",
            "tools": [{
                "type": "function",
                "name": "mcp__codex_app__automation_update",
                "parameters": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "action": { "type": "string" }
                            },
                            "required": ["action"]
                        },
                        { "type": "null" }
                    ]
                }
            }]
        });
        assert!(sanitize_xai_responses_request(&mut body));
        let parameters = &body["tools"][0]["parameters"];
        assert_eq!(parameters["type"], "object");
        assert!(parameters.get("anyOf").is_none());
        assert!(parameters.get("oneOf").is_none());
        assert_eq!(parameters["properties"]["action"]["type"], "string");
    }

    #[test]
    fn flattens_nested_root_unions_and_preserves_defs() {
        let mut body = json!({
            "model": "grok-4.6",
            "tools": [{
                "type": "function",
                "name": "mcp__codex_app__automation_update",
                "parameters": {
                    "$defs": {
                        "View": {
                            "type": "object",
                            "properties": {
                                "mode": { "const": "view" },
                                "id": { "type": "string" }
                            },
                            "required": ["mode", "id"]
                        }
                    },
                    "oneOf": [
                        { "$ref": "#/$defs/View" },
                        {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "mode": { "const": "create" },
                                        "name": { "type": "string" }
                                    },
                                    "required": ["mode", "name"]
                                },
                                { "type": "null" }
                            ]
                        }
                    ]
                }
            }]
        });
        assert!(sanitize_xai_responses_request(&mut body));
        let parameters = &body["tools"][0]["parameters"];
        assert!(parameters.get("type").is_none());
        assert!(parameters.get("$defs").is_some());
        let branches = parameters["oneOf"].as_array().unwrap();
        assert_eq!(branches.len(), 2);
        assert!(branches.iter().all(is_direct_object_branch));
    }

    #[test]
    fn drops_only_the_incompatible_function_tool() {
        let mut body = json!({
            "model": "grok-4.6",
            "tools": [
                {
                    "type": "function",
                    "name": "read_file",
                    "parameters": {
                        "type": "object",
                        "properties": { "path": { "type": "string" } }
                    }
                },
                {
                    "type": "function",
                    "name": "mcp__codex_app__automation_update",
                    "parameters": {
                        "oneOf": [
                            { "type": "string" },
                            { "type": "null" }
                        ]
                    }
                }
            ],
            "tool_choice": {
                "type": "function",
                "name": "mcp__codex_app__automation_update"
            }
        });
        assert!(sanitize_xai_responses_request(&mut body));
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read_file");
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn defaults_null_parameters_to_empty_object() {
        let mut body = json!({
            "model": "grok-4.6",
            "tools": [{
                "type": "function",
                "name": "codex_app__automation_update",
                "parameters": null
            }]
        });
        assert!(sanitize_xai_responses_request(&mut body));
        assert_eq!(
            body["tools"][0]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }
}
