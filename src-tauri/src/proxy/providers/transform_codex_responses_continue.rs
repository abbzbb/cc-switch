//! Grok/xAI native Responses: do not treat a reasoning-only
//! `response.completed` as a finished Codex turn (abbzbb#14).
//!
//! xAI can spend the whole output budget on `reasoning` and then emit
//! `status=completed` with no `message` and no `function_call`. Codex records
//! `task_complete` with `last_agent_message = null` and the UI looks finished.
//! Classify the output, auto-continue the same provider a couple of times, and
//! if it is still empty rewrite the terminal event to `response.incomplete`.

use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::proxy::hyper_client::ProxyResponse;
use crate::proxy::sse::{append_utf8_safe, strip_sse_field, take_sse_block};
use crate::proxy::ProxyError;

pub(crate) const XAI_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 16384;
pub(crate) const XAI_XHIGH_MAX_OUTPUT_TOKENS: u64 = 32768;
pub(crate) const XAI_CONTINUE_MAX_OUTPUT_TOKENS: u64 = 65536;
pub(crate) const XAI_REASONING_CONTINUE_LIMIT: u32 = 2;

const CONTINUE_NUDGE: &str = "Your previous response ended after reasoning only, with no user-visible message and no function call. Continue the same turn: call a tool or write the user-facing answer. Do not stop after reasoning.";

const INSPECT_BUFFER_CAP: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponsesTurnKind {
    Productive,
    ReasoningOnly,
}

pub(crate) enum InspectedTurn {
    Passthrough(ProxyResponse),
    ReasoningOnly {
        status: http::StatusCode,
        headers: http::HeaderMap,
        body: Bytes,
        completed_response: Value,
        is_sse: bool,
    },
}

pub(crate) fn classify_output_items(output: &[Value]) -> ResponsesTurnKind {
    if output.iter().any(item_is_productive) {
        ResponsesTurnKind::Productive
    } else {
        ResponsesTurnKind::ReasoningOnly
    }
}

pub(crate) fn classify_completed_response(
    response: &Value,
    seen_items: &[Value],
) -> ResponsesTurnKind {
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        if !output.is_empty() {
            return classify_output_items(output);
        }
    }
    classify_output_items(seen_items)
}

fn item_is_productive(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => false,
        Some("message") => message_has_visible_text(item),
        Some(
            "function_call"
            | "custom_tool_call"
            | "namespace"
            | "tool_search_call"
            | "web_search_call"
            | "file_search_call"
            | "mcp_call"
            | "image_generation_call"
            | "code_interpreter_call"
            | "code_execution_call"
            | "shell_call",
        ) => true,
        Some(_) => item.get("name").is_some() || item.get("call_id").is_some(),
        None => false,
    }
}

fn message_has_visible_text(item: &Value) -> bool {
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return false;
    };
    content
        .iter()
        .any(|part| match part.get("type").and_then(Value::as_str) {
            Some("output_text" | "text") => part
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty()),
            Some("refusal") => true,
            _ => false,
        })
}

fn sse_event_is_productive(event: &str, value: &Value) -> bool {
    match event {
        "response.output_text.delta" => value
            .get("delta")
            .and_then(Value::as_str)
            .is_some_and(|delta| !delta.is_empty()),
        "response.function_call_arguments.delta"
        | "response.custom_tool_call_input.delta"
        | "response.mcp_call_arguments.delta" => true,
        "response.output_item.added" | "response.output_item.done" => {
            value.get("item").is_some_and(item_is_productive)
        }
        "response.content_part.added" | "response.content_part.done" => {
            value.get("part").is_some_and(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("output_text" | "text" | "refusal")
                ) && part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_none_or(|text| !text.trim().is_empty())
            })
        }
        _ => false,
    }
}

fn parse_sse_block(block: &str) -> Option<(String, Value)> {
    let mut named_event = None;
    let mut data_lines = Vec::new();
    for line in block.lines() {
        if let Some(event) = strip_sse_field(line, "event") {
            named_event = Some(event.trim().to_string());
        } else if let Some(data) = strip_sse_field(line, "data") {
            data_lines.push(data);
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(&data_lines.join("\n")).ok()?;
    let event = named_event
        .filter(|event| !event.is_empty())
        .or_else(|| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    Some((event, value))
}

pub(crate) fn append_continue_nudge(body: &mut Value) -> bool {
    if !body.is_object() {
        return false;
    }
    let nudge = json!({
        "role": "developer",
        "content": [{ "type": "input_text", "text": CONTINUE_NUDGE }]
    });
    match body.get_mut("input") {
        Some(Value::Array(items)) => {
            items.push(nudge);
            true
        }
        Some(Value::String(text)) => {
            let previous = text.clone();
            body["input"] = json!([
                { "role": "user", "content": [{ "type": "input_text", "text": previous }] },
                nudge
            ]);
            true
        }
        None => {
            body["input"] = json!([nudge]);
            true
        }
        Some(_) => false,
    }
}

pub(crate) fn raise_continue_max_output_tokens(body: &mut Value) -> u64 {
    let current = body
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(XAI_DEFAULT_MAX_OUTPUT_TOKENS);
    let next = current
        .saturating_mul(2)
        .clamp(XAI_XHIGH_MAX_OUTPUT_TOKENS, XAI_CONTINUE_MAX_OUTPUT_TOKENS);
    if let Some(object) = body.as_object_mut() {
        object.insert("max_output_tokens".to_string(), json!(next));
    }
    next
}

pub(crate) fn prepare_reasoning_continue_request(body: &mut Value) {
    append_continue_nudge(body);
    raise_continue_max_output_tokens(body);
}

pub(crate) fn mark_response_incomplete(response: &mut Value) {
    if let Some(object) = response.as_object_mut() {
        object.insert("status".to_string(), json!("incomplete"));
        object.insert(
            "incomplete_details".to_string(),
            json!({ "reason": "max_output_tokens" }),
        );
    }
}

pub(crate) fn rewrite_sse_completed_to_incomplete(sse: &str) -> String {
    let mut buffer = sse.to_string();
    let mut leftover = String::new();
    let mut out = String::with_capacity(sse.len() + 64);
    while let Some(block) = take_sse_block(&mut buffer) {
        out.push_str(&rewrite_sse_block(&block));
        out.push_str("\n\n");
    }
    leftover.push_str(&buffer);
    if !leftover.trim().is_empty() {
        out.push_str(&rewrite_sse_block(&leftover));
        if !out.ends_with("\n\n") {
            out.push_str("\n\n");
        }
    }
    out
}

fn rewrite_sse_block(block: &str) -> String {
    let Some((event, mut value)) = parse_sse_block(block) else {
        return block.trim_end().to_string();
    };
    if event != "response.completed"
        && value.get("type").and_then(Value::as_str) != Some("response.completed")
    {
        return block.trim_end().to_string();
    }
    if let Some(response) = value.get_mut("response") {
        mark_response_incomplete(response);
    } else {
        mark_response_incomplete(&mut value);
    }
    value["type"] = json!("response.incomplete");
    format!(
        "event: response.incomplete\ndata: {}",
        serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
    )
}

pub(crate) fn rewrite_json_completed_to_incomplete(body: &mut Value) {
    if body.get("type").and_then(Value::as_str) == Some("response.completed") {
        body["type"] = json!("response.incomplete");
    }
    if body.get("response").is_some() {
        if let Some(response) = body.get_mut("response") {
            mark_response_incomplete(response);
        }
    } else {
        mark_response_incomplete(body);
    }
}

pub(crate) fn reasoning_only_to_incomplete_response(
    status: http::StatusCode,
    mut headers: http::HeaderMap,
    body: Bytes,
    is_sse: bool,
) -> ProxyResponse {
    headers.remove(http::header::CONTENT_LENGTH);
    headers.remove(http::header::TRANSFER_ENCODING);
    let rewritten = if is_sse {
        Bytes::from(rewrite_sse_completed_to_incomplete(
            &String::from_utf8_lossy(&body),
        ))
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(mut value) => {
                rewrite_json_completed_to_incomplete(&mut value);
                Bytes::from(serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec()))
            }
            Err(_) => body,
        }
    };
    ProxyResponse::buffered(status, headers, rewritten)
}

pub(crate) async fn inspect_native_responses_turn(
    response: ProxyResponse,
) -> Result<InspectedTurn, ProxyError> {
    let status = response.status();
    let headers = response.headers().clone();
    let declared_sse = response.is_sse();
    let mut stream = Box::pin(response.bytes_stream());
    let mut raw = bytes::BytesMut::new();
    let mut parse_buffer = String::new();
    let mut utf8_remainder = Vec::new();
    let mut seen_items: Vec<Value> = Vec::new();
    let mut seen_productive = false;
    let mut saw_sse = declared_sse;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            ProxyError::ForwardFailed(format!(
                "Failed while inspecting Grok Responses stream: {error}"
            ))
        })?;
        if raw.len().saturating_add(chunk.len()) > INSPECT_BUFFER_CAP {
            raw.extend_from_slice(&chunk);
            return Ok(passthrough_overflow(status, headers, raw.freeze(), stream));
        }
        raw.extend_from_slice(&chunk);
        append_utf8_safe(&mut parse_buffer, &mut utf8_remainder, &chunk);

        if !saw_sse {
            let trimmed = parse_buffer.trim_start();
            if trimmed.starts_with('{') {
                if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                    return Ok(classify_json_turn(status, headers, raw.freeze(), value));
                }
                continue;
            }
            if trimmed.starts_with("event:") || trimmed.starts_with("data:") {
                saw_sse = true;
            } else if !trimmed.is_empty() {
                continue;
            }
        }

        while let Some(block) = take_sse_block(&mut parse_buffer) {
            match inspect_sse_block(&block, &mut seen_items) {
                SseBlockKind::Productive => seen_productive = true,
                SseBlockKind::Completed(completed) => {
                    let kind = classify_completed_response(&completed, &seen_items);
                    if kind == ResponsesTurnKind::ReasoningOnly && !seen_productive {
                        return Ok(InspectedTurn::ReasoningOnly {
                            status,
                            headers,
                            body: raw.freeze(),
                            completed_response: completed,
                            is_sse: true,
                        });
                    }
                    return Ok(passthrough_buffered(status, headers, raw.freeze(), stream));
                }
                SseBlockKind::TerminalOther => {
                    return Ok(passthrough_buffered(status, headers, raw.freeze(), stream));
                }
                SseBlockKind::Ignore => {}
            }
            if seen_productive {
                return Ok(passthrough_buffered(status, headers, raw.freeze(), stream));
            }
        }
    }

    let leftover = parse_buffer.trim();
    if leftover.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(leftover) {
            return Ok(classify_json_turn(status, headers, raw.freeze(), value));
        }
    }
    Ok(InspectedTurn::Passthrough(ProxyResponse::buffered(
        status,
        headers,
        raw.freeze(),
    )))
}

enum SseBlockKind {
    Ignore,
    Productive,
    Completed(Value),
    TerminalOther,
}

fn inspect_sse_block(block: &str, seen_items: &mut Vec<Value>) -> SseBlockKind {
    let Some((event, value)) = parse_sse_block(block) else {
        return SseBlockKind::Ignore;
    };
    if let Some(item) = value.get("item").cloned() {
        if item.is_object() {
            seen_items.push(item);
        }
    }
    if sse_event_is_productive(&event, &value) {
        return SseBlockKind::Productive;
    }
    match event.as_str() {
        "response.completed" => {
            let completed = value.get("response").cloned().unwrap_or(value);
            SseBlockKind::Completed(completed)
        }
        "response.failed" | "response.incomplete" | "error" => SseBlockKind::TerminalOther,
        _ => SseBlockKind::Ignore,
    }
}

fn classify_json_turn(
    status: http::StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
    value: Value,
) -> InspectedTurn {
    let response = value.get("response").unwrap_or(&value);
    let response_status = response.get("status").and_then(Value::as_str);
    if matches!(response_status, Some("failed" | "cancelled" | "incomplete")) {
        return InspectedTurn::Passthrough(ProxyResponse::buffered(status, headers, body));
    }
    if response_status == Some("completed")
        && classify_completed_response(response, &[]) == ResponsesTurnKind::ReasoningOnly
    {
        return InspectedTurn::ReasoningOnly {
            status,
            headers,
            body,
            completed_response: response.clone(),
            is_sse: false,
        };
    }
    InspectedTurn::Passthrough(ProxyResponse::buffered(status, headers, body))
}

fn passthrough_buffered<S>(
    status: http::StatusCode,
    headers: http::HeaderMap,
    buffered: Bytes,
    rest: S,
) -> InspectedTurn
where
    S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let replay = futures::stream::iter(std::iter::once(Ok(buffered))).chain(rest);
    InspectedTurn::Passthrough(ProxyResponse::streamed(status, headers, replay))
}

fn passthrough_overflow<S>(
    status: http::StatusCode,
    headers: http::HeaderMap,
    buffered: Bytes,
    rest: S,
) -> InspectedTurn
where
    S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    log::warn!("[Codex] Grok Responses inspect buffer exceeded {INSPECT_BUFFER_CAP} bytes; forwarding without continue");
    passthrough_buffered(status, headers, buffered, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::{HeaderValue, CONTENT_TYPE};
    use futures::stream;
    use http::StatusCode;
    use serde_json::json;

    fn sse_headers() -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        headers
    }

    fn json_headers() -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers
    }

    #[test]
    fn classify_reasoning_only_and_empty_message() {
        assert_eq!(
            classify_output_items(&[json!({"type": "reasoning", "summary": []})]),
            ResponsesTurnKind::ReasoningOnly
        );
        assert_eq!(
            classify_output_items(&[
                json!({"type": "reasoning"}),
                json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "   "}]})
            ]),
            ResponsesTurnKind::ReasoningOnly
        );
        assert_eq!(classify_output_items(&[]), ResponsesTurnKind::ReasoningOnly);
    }

    #[test]
    fn classify_message_and_function_call_as_productive() {
        assert_eq!(
            classify_output_items(&[json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "done"}]
            })]),
            ResponsesTurnKind::Productive
        );
        assert_eq!(
            classify_output_items(&[
                json!({"type": "reasoning"}),
                json!({"type": "function_call", "name": "view_image", "call_id": "c1", "arguments": "{}"})
            ]),
            ResponsesTurnKind::Productive
        );
    }

    #[test]
    fn rewrite_sse_completed_becomes_incomplete() {
        let sse = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\"}]}}\n\n"
        );
        let rewritten = rewrite_sse_completed_to_incomplete(sse);
        assert!(rewritten.contains("event: response.incomplete"));
        assert!(rewritten.contains("\"status\":\"incomplete\""));
        assert!(rewritten.contains("\"reason\":\"max_output_tokens\""));
        assert!(!rewritten.contains("event: response.completed"));
        assert!(rewritten.contains("\"type\":\"reasoning\""));
    }

    #[test]
    fn append_nudge_and_raise_budget() {
        let mut body = json!({
            "model": "grok-4.6",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "read the ppt"}]}],
            "max_output_tokens": 16384
        });
        prepare_reasoning_continue_request(&mut body);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["role"], "developer");
        assert_eq!(body["max_output_tokens"], 32768);
    }

    #[tokio::test]
    async fn inspect_reasoning_only_sse_completed() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":4}}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        let inspected = inspect_native_responses_turn(response).await.unwrap();
        match inspected {
            InspectedTurn::ReasoningOnly {
                completed_response,
                is_sse,
                ..
            } => {
                assert!(is_sse);
                assert_eq!(completed_response["status"], "completed");
            }
            InspectedTurn::Passthrough(_) => panic!("expected reasoning-only completed"),
        }
    }

    #[tokio::test]
    async fn inspect_function_call_is_passthrough() {
        let sse = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"name\":\"view_image\",\"call_id\":\"c1\",\"arguments\":\"\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"name\":\"view_image\"}]}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        let inspected = inspect_native_responses_turn(response).await.unwrap();
        assert!(matches!(inspected, InspectedTurn::Passthrough(_)));
    }

    #[tokio::test]
    async fn inspect_json_reasoning_only_completed() {
        let body = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [{"type": "reasoning", "summary": []}],
            "usage": {"input_tokens": 8, "output_tokens": 3}
        });
        let response = ProxyResponse::buffered(
            StatusCode::OK,
            json_headers(),
            Bytes::from(serde_json::to_vec(&body).unwrap()),
        );
        let inspected = inspect_native_responses_turn(response).await.unwrap();
        assert!(matches!(
            inspected,
            InspectedTurn::ReasoningOnly { is_sse: false, .. }
        ));
    }
}
