//! Clamp Responses item / call ids to the OpenAI 64-character limit.
//!
//! Codex (and some third-party Responses upstreams such as xAI) emit `input[].id`
//! / `call_id` values longer than OpenAI's Responses schema allows
//! (`maxLength: 64`). The native passthrough used to forward those ids verbatim,
//! so a multi-turn body replayed onto `api.openai.com` (or any strict compatible
//! gateway) 400s with `string_above_max_length` before generation starts —
//! typically on a later `input[n].id` once the conversation has accumulated
//! tool results (see abbzbb/cc-switch#8).
//!
//! This pass walks the request `input` / `output` item lists and
//! `previous_response_id`, and remaps every over-long `id` / `call_id` onto a
//! stable SHA-256 hex id (preserving a short `msg_` / `fc_` / `rs_` / `call_`
//! prefix when present). The same original string always maps to the same short
//! id, so `function_call.call_id` and `function_call_output.call_id` stay
//! paired. Ids that are already ≤ 64 are left untouched, so the prompt-cache
//! prefix of a well-formed body does not change.

use std::collections::HashMap;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// OpenAI Responses `id` / `call_id` maxLength.
pub(crate) const RESPONSES_ID_MAX_LEN: usize = 64;

/// Keys rewritten on each Responses item. `previous_response_id` is handled
/// separately at the top level.
const ITEM_ID_KEYS: &[&str] = &["id", "call_id"];

/// Remap over-long Responses ids in place. Returns whether anything changed.
/// Deterministic and idempotent: a second pass on the rewritten body is a no-op.
pub(crate) fn clamp_responses_item_ids(body: &mut Value) -> bool {
    if !body.is_object() {
        return false;
    }

    let mut map = HashMap::new();
    let mut changed = false;

    changed |= clamp_string_field(body, "previous_response_id", &mut map);

    for key in ["input", "output"] {
        if let Some(value) = body.get_mut(key) {
            changed |= clamp_item_list(value, &mut map);
        }
    }

    changed
}

fn clamp_item_list(value: &mut Value, map: &mut HashMap<String, String>) -> bool {
    match value {
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= clamp_item(item, map);
            }
            changed
        }
        Value::Object(_) => clamp_item(value, map),
        _ => false,
    }
}

fn clamp_item(item: &mut Value, map: &mut HashMap<String, String>) -> bool {
    let Some(obj) = item.as_object_mut() else {
        return false;
    };

    let mut changed = false;
    for key in ITEM_ID_KEYS {
        let Some(original) = obj.get(*key).and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        if !id_exceeds_limit(&original) {
            continue;
        }
        let replacement = map
            .entry(original.clone())
            .or_insert_with(|| shorten_responses_id(&original))
            .clone();
        obj.insert((*key).to_string(), json!(replacement));
        changed = true;
    }
    if let Some(content) = obj.get_mut("content") {
        changed |= clamp_item_list(content, map);
    }
    changed
}

fn clamp_string_field(body: &mut Value, key: &str, map: &mut HashMap<String, String>) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    let Some(original) = obj.get(key).and_then(Value::as_str).map(str::to_owned) else {
        return false;
    };
    if !id_exceeds_limit(&original) {
        return false;
    }
    let replacement = map
        .entry(original.clone())
        .or_insert_with(|| shorten_responses_id(&original))
        .clone();
    obj.insert(key.to_string(), json!(replacement));
    true
}

fn id_exceeds_limit(id: &str) -> bool {
    id.chars().count() > RESPONSES_ID_MAX_LEN
}

/// Stable ≤64-character stand-in for an over-long Responses id.
///
/// Already-short ids are returned unchanged. Long ids keep a short alphabetic
/// prefix (`msg_`, `rs_`, `fc_`, `call_`, `resp_`, …) so logs stay recognizable,
/// then fill the rest with lowercase SHA-256 hex of the original string.
pub(crate) fn shorten_responses_id(id: &str) -> String {
    if !id_exceeds_limit(id) {
        return id.to_string();
    }

    let prefix = leading_id_prefix(id);
    let hex = sha256_hex(id.as_bytes());
    let mut out = String::with_capacity(RESPONSES_ID_MAX_LEN);
    out.push_str(prefix);
    let remain = RESPONSES_ID_MAX_LEN.saturating_sub(out.len());
    out.push_str(&hex[..remain.min(hex.len())]);
    debug_assert!(out.chars().count() <= RESPONSES_ID_MAX_LEN);
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Leading `abc_` prefix of at most 8 ASCII bytes (`msg_`, `call_`, `resp_`).
fn leading_id_prefix(id: &str) -> &str {
    const MAX_PREFIX: usize = 8;
    let bytes = id.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < MAX_PREFIX && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && i < MAX_PREFIX && bytes[i] == b'_' {
        &id[..i + 1]
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn long_id(prefix: &str, total: usize) -> String {
        let mut id = prefix.to_string();
        id.push_str(&"x".repeat(total.saturating_sub(prefix.len())));
        assert_eq!(id.len(), total);
        id
    }

    #[test]
    fn eighty_three_char_input_id_is_rewritten_and_not_forwarded_raw() {
        let original = long_id("rs_", 83);
        let mut body = json!({
            "model": "gpt-5.6-luna",
            "input": [
                { "type": "message", "role": "user", "content": "hi" },
                { "type": "reasoning", "id": original }
            ]
        });

        assert!(clamp_responses_item_ids(&mut body));

        let rewritten = body["input"][1]["id"].as_str().unwrap();
        assert!(
            rewritten.chars().count() <= RESPONSES_ID_MAX_LEN,
            "rewritten id still too long: {rewritten} ({})",
            rewritten.chars().count()
        );
        assert_ne!(rewritten, original);
        assert!(
            !serde_json::to_string(&body).unwrap().contains(&original),
            "original 83-char id must not be forwarded raw"
        );
        assert!(rewritten.starts_with("rs_"));
        assert_eq!(rewritten, shorten_responses_id(&original));
    }

    #[test]
    fn matching_call_ids_remap_together() {
        let call = long_id("call_", 83);
        let item = long_id("fc_", 83);
        let mut body = json!({
            "input": [
                {
                    "type": "function_call",
                    "id": item,
                    "call_id": call,
                    "name": "shell",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": call,
                    "output": "ok"
                }
            ]
        });

        assert!(clamp_responses_item_ids(&mut body));

        let mapped_call = body["input"][0]["call_id"].as_str().unwrap();
        assert_eq!(body["input"][1]["call_id"].as_str().unwrap(), mapped_call);
        assert!(mapped_call.chars().count() <= RESPONSES_ID_MAX_LEN);
        assert!(mapped_call.starts_with("call_"));
        assert_ne!(mapped_call, call);

        let mapped_item = body["input"][0]["id"].as_str().unwrap();
        assert!(mapped_item.chars().count() <= RESPONSES_ID_MAX_LEN);
        assert!(mapped_item.starts_with("fc_"));
        assert_ne!(mapped_item, mapped_call);
    }

    #[test]
    fn short_ids_are_left_alone() {
        let mut body = json!({
            "previous_response_id": "resp_short",
            "input": [
                { "type": "message", "id": "msg_0123456789abcdef", "role": "user", "content": "hi" },
                { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "t", "arguments": "{}" }
            ]
        });
        let before = body.clone();
        assert!(!clamp_responses_item_ids(&mut body));
        assert_eq!(body, before);
    }

    #[test]
    fn previous_response_id_is_clamped() {
        let original = long_id("resp_", 83);
        let mut body = json!({
            "previous_response_id": original,
            "input": []
        });
        assert!(clamp_responses_item_ids(&mut body));
        let rewritten = body["previous_response_id"].as_str().unwrap();
        assert!(rewritten.chars().count() <= RESPONSES_ID_MAX_LEN);
        assert!(rewritten.starts_with("resp_"));
        assert!(!serde_json::to_string(&body).unwrap().contains(&original));
    }

    #[test]
    fn arguments_payload_is_not_walked() {
        let buried = long_id("msg_", 83);
        let mut body = json!({
            "input": [{
                "type": "function_call",
                "id": "fc_ok",
                "call_id": "call_ok",
                "name": "echo",
                "arguments": format!(r#"{{"id":"{buried}"}}"#)
            }]
        });
        assert!(!clamp_responses_item_ids(&mut body));
        assert!(body["input"][0]["arguments"]
            .as_str()
            .unwrap()
            .contains(&buried));
    }

    #[test]
    fn remap_is_stable_and_idempotent() {
        let original = long_id("msg_", 83);
        let mut first = json!({ "input": [{ "type": "message", "id": original }] });
        let mut second = first.clone();
        assert!(clamp_responses_item_ids(&mut first));
        assert!(clamp_responses_item_ids(&mut second));
        assert_eq!(first["input"][0]["id"], second["input"][0]["id"]);
        assert!(!clamp_responses_item_ids(&mut first));
    }

    #[test]
    fn sixty_four_char_id_is_preserved() {
        let original = long_id("msg_", 64);
        assert_eq!(shorten_responses_id(&original), original);
        let mut body = json!({ "input": [{ "id": original }] });
        assert!(!clamp_responses_item_ids(&mut body));
        assert_eq!(body["input"][0]["id"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn unprefixed_long_id_is_pure_hex() {
        let original = "x".repeat(83);
        let rewritten = shorten_responses_id(&original);
        assert_eq!(rewritten.len(), RESPONSES_ID_MAX_LEN);
        assert!(rewritten.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            rewritten,
            &sha256_hex(original.as_bytes())[..RESPONSES_ID_MAX_LEN]
        );
    }
}
