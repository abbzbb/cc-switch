//! Grok/xAI native Responses: do not treat an empty hop as a finished Codex
//! turn (abbzbb#14, abbzbb#16, abbzbb#19).
//!
//! xAI can (a) spend the whole output budget on `reasoning`, (b) close HTTP 200
//! without `response.completed`, (c) emit only a short progress sentence, or
//! (d) drop the chunked body mid-SSE. Codex then records `task_complete` with
//! `last_agent_message = null`, or the inspector surfaces a 502.
//! Classify the output, retry a decode/truncation once on the same card,
//! auto-continue empty hops a couple of times, and if it is still empty rewrite
//! the terminal event to `response.incomplete`.

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::proxy::hyper_client::ProxyResponse;
use crate::proxy::sse::{append_utf8_safe, strip_sse_field, take_sse_block};

pub(crate) const XAI_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 16384;
pub(crate) const XAI_XHIGH_MAX_OUTPUT_TOKENS: u64 = 32768;
pub(crate) const XAI_CONTINUE_MAX_OUTPUT_TOKENS: u64 = 65536;
pub(crate) const XAI_REASONING_CONTINUE_LIMIT: u32 = 2;
pub(crate) const XAI_STREAM_DECODE_RETRY_LIMIT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SameCardKind {
    StreamBroken,
    EmptyHop,
}

/// Policy for `apply_xai_reasoning_continue`. Exhausted stream-breaks rewrite
/// incomplete; they never surface as `ForwardFailed` / 502.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SameCardFollowup {
    RetryOriginal,
    ContinueEmpty,
    RewriteIncomplete,
}

pub(crate) fn same_card_followup(
    kind: SameCardKind,
    stream_retries: u32,
    continues: u32,
) -> SameCardFollowup {
    match kind {
        SameCardKind::StreamBroken => {
            if stream_retries < XAI_STREAM_DECODE_RETRY_LIMIT {
                SameCardFollowup::RetryOriginal
            } else {
                SameCardFollowup::RewriteIncomplete
            }
        }
        SameCardKind::EmptyHop => {
            if continues < XAI_REASONING_CONTINUE_LIMIT {
                SameCardFollowup::ContinueEmpty
            } else {
                SameCardFollowup::RewriteIncomplete
            }
        }
    }
}

/// Short progress-only assistant text stays in the empty-hop bucket.
/// Longer text is treated as a real answer even if it starts with "let me".
/// Keep this at a status-sentence length so "I'll look at parse.rs and fix it"
/// is not swallowed as an empty hop.
const PROGRESS_ONLY_MAX_CHARS: usize = 80;

const CONTINUE_NUDGE: &str = concat!(
    "Your previous response did not finish the turn: it was empty, cut off, reasoning-only, or only a short progress note, with no tool call and no user-facing answer. ",
    "Continue the same turn: call a tool or write the answer. Do not stop after a status or plan sentence. ",
    "The next output must be a function_call or the final answer, not another status sentence. ",
    "上一跳未完成：空输出、被截断、只有 reasoning，或只有一句进度/计划且没有工具调用和最终答案。",
    "同一轮继续：立刻调用工具或写出答案。禁止再只回复计划或进度句。"
);

const INSPECT_BUFFER_CAP: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponsesTurnKind {
    Productive,
    ReasoningOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmptyHopReason {
    ReasoningOnly,
    EmptyOutput,
    ZeroUsage,
    ProgressOnly,
    MissingTerminal,
}

impl EmptyHopReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReasoningOnly => "reasoning-only",
            Self::EmptyOutput => "empty-output",
            Self::ZeroUsage => "zero-usage",
            Self::ProgressOnly => "progress-only",
            Self::MissingTerminal => "missing-terminal",
        }
    }
}

pub(crate) enum InspectedTurn {
    Passthrough(ProxyResponse),
    ReasoningOnly {
        status: http::StatusCode,
        headers: http::HeaderMap,
        body: Bytes,
        completed_response: Value,
        is_sse: bool,
        reason: EmptyHopReason,
    },
    /// Chunk decode / truncated `data:` after HTTP 200. Same-card retry of the
    /// current request; still broken → incomplete, never 502.
    StreamBroken {
        status: http::StatusCode,
        headers: http::HeaderMap,
        body: Bytes,
        is_sse: bool,
        error: String,
        leftover_event: Option<String>,
        leftover_bytes: usize,
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
        Some("message") => message_is_productive(item),
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

fn message_is_productive(item: &Value) -> bool {
    if message_has_refusal(item) {
        return true;
    }
    let text = extract_message_text(item);
    let trimmed = text.trim();
    !trimmed.is_empty() && !is_progress_only_text(trimmed)
}

fn message_has_refusal(item: &Value) -> bool {
    item.get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("refusal"))
        })
}

fn extract_message_text(item: &Value) -> String {
    let Some(content) = item.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for part in parts {
        if matches!(
            part.get("type").and_then(Value::as_str),
            Some("output_text" | "text")
        ) {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                out.push_str(text);
            }
        }
    }
    out
}

fn message_has_visible_text(item: &Value) -> bool {
    !extract_message_text(item).trim().is_empty()
}

pub(crate) fn is_progress_only_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.chars().count() > PROGRESS_ONLY_MAX_CHARS {
        return false;
    }
    if trimmed.contains('\n') || trimmed.contains("```") {
        return false;
    }
    let normalized = collapse_ws(trimmed).to_lowercase();
    looks_like_progress_phrase(&normalized)
}

fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            prev_space = false;
            out.push(ch);
        }
    }
    out
}

fn looks_like_progress_phrase(s: &str) -> bool {
    if has_answer_signal(s) {
        return false;
    }
    if starts_with_zh_progress(s) || looks_like_zh_plan_sentence(s) {
        return true;
    }

    const STARTERS: &[&str] = &[
        "let me ",
        "i'll ",
        "i will ",
        "i am ",
        "i'm ",
        "i am going to ",
        "i'm going to ",
        "going to ",
        "first i'll ",
        "first i will ",
        "i'll start",
        "let me start",
        "starting by ",
        "taking a look",
        "have a look",
    ];
    if !STARTERS.iter().any(|needle| s.starts_with(needle)) {
        return false;
    }
    const VERBS: &[&str] = &[
        "look", "check", "inspect", "read", "open", "examine", "start", "begin", "review", "scan",
        "peek", "view",
    ];
    VERBS.iter().any(|verb| s.contains(verb))
}

fn starts_with_zh_progress(s: &str) -> bool {
    const ZH: &[&str] = &[
        "我先看",
        "我先检查",
        "我先檢查",
        "我先读",
        "我先讀",
        "我先打开",
        "我先打開",
        "我先把",
        "我先核",
        "我先筛",
        "我先篩",
        "先看一下",
        "先看下",
        "先检查",
        "先檢查",
        "先核对",
        "先核對",
        "先查看",
        "先读取",
        "先讀取",
        "让我看",
        "讓我看",
        "让我检查",
        "讓我檢查",
        "让我先",
        "讓我先",
        "我来看",
        "我來看",
        "我来检查",
        "我來檢查",
        "我看看",
        "看一下你",
        "稍等我",
        "等我先",
        "我打开",
        "我打開",
    ];
    // Prefix only — substring match turned "我打开了 README，结论是…" into an empty hop.
    ZH.iter().any(|needle| s.starts_with(needle))
}

/// Grok often stalls in one short Chinese status/plan sentence that does not
/// start with `我先…` / `让我…`. Require a status opener **and** either a
/// 先…再 plan or an unfinished commitment so "按原计划用现有实现。" stays a
/// real answer.
fn looks_like_zh_plan_sentence(s: &str) -> bool {
    const OPENERS: &[&str] = &[
        "继续",
        "繼續",
        "按原",
        "按计",
        "按計",
        "开始",
        "開始",
        "这次",
        "這次",
        "接下来",
        "接下來",
        "随后",
        "隨後",
        "正在",
        "马上",
        "馬上",
        "现在",
        "現在",
        "直接",
        "立刻",
        "接着",
        "接著",
        "我这边",
        "我這邊",
        "我先",
        "让我",
        "讓我",
        "我来",
        "我來",
    ];
    if !OPENERS.iter().any(|opener| s.starts_with(opener)) {
        return false;
    }
    zh_has_first_then_plan(s) || zh_has_unfinished_commitment(s)
}

fn zh_has_first_then_plan(s: &str) -> bool {
    let Some(idx) = s.find('先') else {
        return false;
    };
    let rest = &s[idx + '先'.len_utf8()..];
    rest.contains('再')
        || rest.contains("然后")
        || rest.contains("然後")
        || rest.contains("接着")
        || rest.contains("接著")
}

fn zh_has_unfinished_commitment(s: &str) -> bool {
    s.contains("不再停")
        || s.contains("不再中断")
        || s.contains("不再中斷")
        || s.contains("全部做完")
        || s.contains("全部判完")
        || s.contains("全部核完")
        || s.contains("全部判定")
}

fn has_answer_signal(s: &str) -> bool {
    const SIGNALS: &[&str] = &[
        "结论",
        "結論",
        "因为",
        "因為",
        "已经",
        "已經",
        "完成了",
        "发现",
        "發現",
        "结果",
        "結果",
        "如下",
        "应该",
        "應該",
        "建议",
        "建議",
        "所以",
        "因此",
        "问题在",
        "問題在",
        "原因是",
        "because",
        " and fix",
        " then fix",
        ".rs",
        ".ts",
        ".tsx",
        "readme",
        "路由",
    ];
    SIGNALS.iter().any(|signal| s.contains(signal))
}

/// Hold the inspect buffer until a terminal event, a tool call, or the text is
/// clearly a real answer (over the progress cap / has a newline or fence).
/// Token prefixes like "I'll" must not flush to Codex.
fn text_is_under_hold_cap(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.contains('\n') || trimmed.contains("```") {
        return false;
    }
    trimmed.chars().count() <= PROGRESS_ONLY_MAX_CHARS
}

fn usage_all_zero(response: &Value) -> bool {
    let Some(usage) = response.get("usage") else {
        return false;
    };
    let get = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cached_details = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    get("input_tokens") == 0
        && get("output_tokens") == 0
        && get("prompt_tokens") == 0
        && get("completion_tokens") == 0
        && get("cached_tokens") == 0
        && get("cache_creation_tokens") == 0
        && cached_details == 0
}

fn items_have_progress_only_message(items: &[Value]) -> bool {
    items.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("message")
            && message_has_visible_text(item)
            && !message_is_productive(item)
    })
}

fn items_have_reasoning(items: &[Value]) -> bool {
    items
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
}

fn empty_hop_reason(
    response: &Value,
    items: &[Value],
    accumulated_text: &str,
    missing_terminal: bool,
) -> EmptyHopReason {
    if missing_terminal {
        return EmptyHopReason::MissingTerminal;
    }
    if is_progress_only_text(accumulated_text.trim()) || items_have_progress_only_message(items) {
        return EmptyHopReason::ProgressOnly;
    }
    if usage_all_zero(response) {
        return EmptyHopReason::ZeroUsage;
    }
    if items_have_reasoning(items)
        || response
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|output| output.iter().any(items_have_reasoning_item))
    {
        return EmptyHopReason::ReasoningOnly;
    }
    EmptyHopReason::EmptyOutput
}

fn items_have_reasoning_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("reasoning")
}

fn sse_event_is_productive(event: &str, value: &Value) -> bool {
    match event {
        "response.output_text.delta" => {
            value
                .get("delta")
                .and_then(Value::as_str)
                .is_some_and(|delta| {
                    let trimmed = delta.trim();
                    !trimmed.is_empty()
                        && !is_progress_only_text(trimmed)
                        && has_answer_signal(trimmed)
                })
        }
        "response.function_call_arguments.delta"
        | "response.custom_tool_call_input.delta"
        | "response.mcp_call_arguments.delta" => true,
        "response.output_item.added" | "response.output_item.done" => {
            value.get("item").is_some_and(item_is_productive)
        }
        "response.content_part.added" | "response.content_part.done" => value
            .get("part")
            .is_some_and(|part| part.get("type").and_then(Value::as_str) == Some("refusal")),
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
    let doubled = current
        .saturating_mul(2)
        .max(XAI_XHIGH_MAX_OUTPUT_TOKENS)
        .max(current);
    let next = if current >= XAI_CONTINUE_MAX_OUTPUT_TOKENS {
        current
    } else {
        doubled.min(XAI_CONTINUE_MAX_OUTPUT_TOKENS)
    };
    if let Some(object) = body.as_object_mut() {
        object.insert("max_output_tokens".to_string(), json!(next));
    }
    next
}

pub(crate) fn prepare_reasoning_continue_request(body: &mut Value, previous: Option<&Value>) {
    if let Some(previous) = previous {
        append_previous_hop_output(body, previous);
    }
    append_continue_nudge(body);
    raise_continue_max_output_tokens(body);
}

fn append_previous_hop_output(body: &mut Value, previous: &Value) -> bool {
    let output = previous
        .get("output")
        .or_else(|| {
            previous
                .get("response")
                .and_then(|response| response.get("output"))
        })
        .and_then(Value::as_array);
    let Some(output) = output else {
        return false;
    };
    if output.is_empty() {
        return false;
    }
    let extras: Vec<Value> = output.iter().cloned().map(output_item_as_input).collect();
    match body.get_mut("input") {
        Some(Value::Array(items)) => {
            items.extend(extras);
            true
        }
        Some(Value::String(text)) => {
            let previous_text = text.clone();
            let mut items = vec![json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": previous_text }]
            })];
            items.extend(extras);
            body["input"] = Value::Array(items);
            true
        }
        None => {
            body["input"] = Value::Array(extras);
            true
        }
        Some(_) => false,
    }
}

fn output_item_as_input(mut item: Value) -> Value {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match item_type.as_str() {
        "message" => {
            if let Some(object) = item.as_object_mut() {
                object.entry("role").or_insert_with(|| json!("assistant"));
                object.remove("status");
                if let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) {
                    for part in content {
                        if let Some(part) = part.as_object_mut() {
                            if part.get("type").and_then(Value::as_str) == Some("output_text") {
                                part.insert("type".to_string(), json!("input_text"));
                            }
                        }
                    }
                }
            }
            item
        }
        "reasoning" => {
            let id = item.get("id").cloned();
            let summary = item.get("summary").cloned();
            let mut next = serde_json::Map::new();
            next.insert("type".to_string(), json!("reasoning"));
            if let Some(id) = id {
                next.insert("id".to_string(), id);
            }
            if let Some(summary) = summary {
                next.insert("summary".to_string(), summary);
            }
            Value::Object(next)
        }
        _ => item,
    }
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
    rewrite_sse_completed_to_incomplete_inner(sse, None)
}

fn rewrite_sse_completed_to_incomplete_inner(sse: &str, usage_source: Option<&Value>) -> String {
    let mut buffer = sse.to_string();
    let mut leftover = String::new();
    let mut out = String::with_capacity(sse.len() + 128);
    let mut rewrote_completed = false;
    while let Some(block) = take_sse_block(&mut buffer) {
        append_rewritten_sse_frames(&mut out, &block, &mut rewrote_completed);
    }
    leftover.push_str(&buffer);
    if !leftover.trim().is_empty() {
        append_rewritten_sse_frames(&mut out, &leftover, &mut rewrote_completed);
    }
    if !rewrote_completed && !out.contains("event: response.incomplete") {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push_str("\n\n");
        }
        out.push_str(&synthetic_incomplete_sse(usage_source));
    }
    out
}

fn append_rewritten_sse_frames(out: &mut String, block: &str, rewrote_completed: &mut bool) {
    let mut parsed_any = false;
    for frame in leftover_sse_frames(block) {
        if parse_sse_block(frame).is_none() {
            if leftover_is_truncated_sse(frame) {
                log::warn!(
                    "[Codex] dropping truncated Grok SSE leftover ({} bytes) before incomplete rewrite",
                    frame.len()
                );
            }
            continue;
        }
        parsed_any = true;
        let rewritten = rewrite_sse_block(frame);
        if rewritten.contains("event: response.incomplete") {
            *rewrote_completed = true;
        }
        out.push_str(&rewritten);
        out.push_str("\n\n");
    }
    if !parsed_any && leftover_is_truncated_sse(block) {
        log::warn!(
            "[Codex] dropping truncated Grok SSE leftover ({} bytes) before incomplete rewrite",
            block.len()
        );
    }
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

fn synthetic_incomplete_sse(usage_source: Option<&Value>) -> String {
    let mut response = json!({ "output": [] });
    if let Some(source) = usage_source {
        let inner = source.get("response").unwrap_or(source);
        if let Some(id) = inner.get("id") {
            response["id"] = id.clone();
        }
        if let Some(model) = inner.get("model") {
            response["model"] = model.clone();
        }
        if let Some(usage) = inner.get("usage") {
            response["usage"] = usage.clone();
        }
    }
    mark_response_incomplete(&mut response);
    let value = json!({
        "type": "response.incomplete",
        "response": response
    });
    format!(
        "event: response.incomplete\ndata: {}\n\n",
        serde_json::to_string(&value)
            .unwrap_or_else(|_| "{\"type\":\"response.incomplete\"}".to_string())
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
    headers: http::HeaderMap,
    body: Bytes,
    is_sse: bool,
) -> ProxyResponse {
    reasoning_only_to_incomplete_response_with_usage(status, headers, body, is_sse, None)
}

pub(crate) fn reasoning_only_to_incomplete_response_with_usage(
    status: http::StatusCode,
    mut headers: http::HeaderMap,
    body: Bytes,
    is_sse: bool,
    usage_source: Option<&Value>,
) -> ProxyResponse {
    headers.remove(http::header::CONTENT_LENGTH);
    headers.remove(http::header::TRANSFER_ENCODING);
    let rewritten = if is_sse {
        let sse = String::from_utf8_lossy(&body);
        Bytes::from(if usage_source.is_some() {
            rewrite_sse_completed_to_incomplete_inner(&sse, usage_source)
        } else {
            rewrite_sse_completed_to_incomplete(&sse)
        })
    } else if body.is_empty() {
        synthesize_incomplete_json(usage_source)
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(mut value) => {
                rewrite_json_completed_to_incomplete(&mut value);
                copy_usage_if_missing(&mut value, usage_source);
                Bytes::from(serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec()))
            }
            Err(_) => synthesize_incomplete_json(usage_source),
        }
    };
    ProxyResponse::buffered(status, headers, rewritten)
}

fn copy_usage_if_missing(body: &mut Value, usage_source: Option<&Value>) {
    let Some(source) = usage_source else {
        return;
    };
    let inner = source.get("response").unwrap_or(source);
    let Some(usage) = inner.get("usage") else {
        return;
    };
    if body.get("usage").is_none() {
        if let Some(response) = body.get_mut("response") {
            if response.get("usage").is_none() {
                response["usage"] = usage.clone();
            }
        } else {
            body["usage"] = usage.clone();
        }
    }
}

fn synthesize_incomplete_json(usage_source: Option<&Value>) -> Bytes {
    let mut value = json!({ "output": [] });
    if let Some(source) = usage_source {
        let inner = source.get("response").unwrap_or(source);
        if let Some(id) = inner.get("id") {
            value["id"] = id.clone();
        }
        if let Some(model) = inner.get("model") {
            value["model"] = model.clone();
        }
        if let Some(usage) = inner.get("usage") {
            value["usage"] = usage.clone();
        }
    }
    mark_response_incomplete(&mut value);
    Bytes::from(serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()))
}

pub(crate) async fn inspect_native_responses_turn(response: ProxyResponse) -> InspectedTurn {
    inspect_native_responses_turn_timed(response, None).await
}

pub(crate) async fn inspect_native_responses_turn_timed(
    response: ProxyResponse,
    idle_timeout: Option<Duration>,
) -> InspectedTurn {
    let status = response.status();
    if !status.is_success() {
        return InspectedTurn::Passthrough(response);
    }

    let headers = response.headers().clone();
    let declared_sse = response.is_sse();
    let mut stream = Box::pin(response.bytes_stream());
    let mut raw = bytes::BytesMut::new();
    let mut parse_buffer = String::new();
    let mut utf8_remainder = Vec::new();
    let mut seen_items: Vec<Value> = Vec::new();
    let mut seen_productive = false;
    let mut saw_sse = declared_sse;
    let mut accumulated_text = String::new();
    let mut last_event: Option<String> = None;
    let mut last_response: Option<Value> = None;
    let mut saw_text_delta = false;

    loop {
        let chunk = if let Some(idle) = idle_timeout {
            match tokio::time::timeout(idle, stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    let leftover =
                        leftover_snapshot(&parse_buffer, &utf8_remainder, last_event.clone());
                    if items_have_reasoning(&seen_items)
                        || last_event
                            .as_deref()
                            .is_some_and(|event| event.contains("reasoning"))
                    {
                        log::warn!(
                            "[Codex] Grok Responses inspect idle timeout {}s with reasoning already in-flight (event={:?}); treating as empty hop instead of stream-broken",
                            idle.as_secs(),
                            leftover.0
                        );
                        let completed = stub_response(last_response, &seen_items);
                        let reason =
                            empty_hop_reason(&completed, &seen_items, &accumulated_text, true);
                        return InspectedTurn::ReasoningOnly {
                            status,
                            headers,
                            body: raw.freeze(),
                            completed_response: completed,
                            is_sse: saw_sse || declared_sse,
                            reason,
                        };
                    }
                    log::warn!(
                        "[Codex] Grok Responses inspect idle timeout {}s (event={:?} leftover={} bytes)",
                        idle.as_secs(),
                        leftover.0,
                        leftover.1
                    );
                    return InspectedTurn::StreamBroken {
                        status,
                        headers,
                        body: raw.freeze(),
                        is_sse: saw_sse || declared_sse,
                        error: format!("idle timeout {}s", idle.as_secs()),
                        leftover_event: leftover.0,
                        leftover_bytes: leftover.1,
                    };
                }
            }
        } else {
            match stream.next().await {
                Some(chunk) => chunk,
                None => break,
            }
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let leftover = leftover_snapshot(&parse_buffer, &utf8_remainder, last_event);
                log::warn!(
                    "[Codex] Grok Responses stream broke while inspecting: {} (event={:?} leftover={} bytes)",
                    error,
                    leftover.0,
                    leftover.1
                );
                return InspectedTurn::StreamBroken {
                    status,
                    headers,
                    body: raw.freeze(),
                    is_sse: saw_sse || declared_sse,
                    error: error.to_string(),
                    leftover_event: leftover.0,
                    leftover_bytes: leftover.1,
                };
            }
        };
        if raw.len().saturating_add(chunk.len()) > INSPECT_BUFFER_CAP {
            raw.extend_from_slice(&chunk);
            if !seen_productive && text_is_under_hold_cap(&accumulated_text) {
                let completed = stub_response(last_response, &seen_items);
                let reason = empty_hop_reason(&completed, &seen_items, &accumulated_text, true);
                return InspectedTurn::ReasoningOnly {
                    status,
                    headers,
                    body: raw.freeze(),
                    completed_response: completed,
                    is_sse: saw_sse || declared_sse,
                    reason,
                };
            }
            return passthrough_overflow(status, headers, raw.freeze(), stream);
        }
        raw.extend_from_slice(&chunk);
        append_utf8_safe(&mut parse_buffer, &mut utf8_remainder, &chunk);

        if !saw_sse {
            let trimmed = parse_buffer.trim_start();
            if trimmed.starts_with('{') {
                if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                    return classify_json_turn(status, headers, raw.freeze(), value);
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
            for frame in leftover_sse_frames(&block) {
                match inspect_sse_block(
                    frame,
                    &mut seen_items,
                    &mut accumulated_text,
                    &mut last_response,
                    &mut saw_text_delta,
                ) {
                    SseBlockKind::Productive => seen_productive = true,
                    SseBlockKind::Completed(completed) => {
                        let body = raw.freeze();
                        if let Some(hop) = maybe_reasoning_only_completed(
                            status,
                            headers.clone(),
                            body.clone(),
                            completed,
                            &seen_items,
                            &accumulated_text,
                            seen_productive,
                        ) {
                            return hop;
                        }
                        // Hold until a terminal event so truncated SSE can still
                        // be rewritten to `response.incomplete`, and so
                        // `inject_dropped_tool_search` sees a Buffered body.
                        return passthrough_inspected(status, headers, body);
                    }
                    SseBlockKind::TerminalOther => {
                        return passthrough_inspected(status, headers, raw.freeze());
                    }
                    SseBlockKind::Ignore(event) => {
                        if !event.is_empty() {
                            last_event = Some(event);
                        }
                    }
                }
            }
        }
    }

    // Trailing incomplete UTF-8 must not poison a complete last JSON frame.
    // Try leftover as-is first; only fold remainder in if the buffer is empty.
    if parse_buffer.trim().is_empty() && !utf8_remainder.is_empty() {
        parse_buffer.push_str(&String::from_utf8_lossy(&utf8_remainder));
        utf8_remainder.clear();
    }

    let leftover = parse_buffer.trim().trim_end_matches('\u{FFFD}').trim();
    if leftover.starts_with('{') && !saw_sse {
        if let Ok(value) = serde_json::from_str::<Value>(leftover) {
            return classify_json_turn(status, headers, raw.freeze(), value);
        }
        let snapshot = leftover_snapshot(&parse_buffer, &[], last_event);
        log::warn!(
            "[Codex] Grok Responses JSON truncated while inspecting (event={:?} leftover={} bytes)",
            snapshot.0,
            snapshot.1
        );
        return InspectedTurn::StreamBroken {
            status,
            headers,
            body: raw.freeze(),
            is_sse: false,
            error: "truncated JSON body".to_string(),
            leftover_event: snapshot.0,
            leftover_bytes: snapshot.1,
        };
    }

    if !leftover.is_empty() {
        let mut parsed_any = false;
        for frame in leftover_sse_frames(leftover) {
            if parse_sse_block(frame).is_none() {
                if leftover_is_truncated_sse(frame) {
                    let snapshot = leftover_snapshot(frame, &[], last_event.clone());
                    log::warn!(
                        "[Codex] Grok Responses SSE truncated while inspecting (event={:?} leftover={} bytes)",
                        snapshot.0,
                        snapshot.1
                    );
                    return InspectedTurn::StreamBroken {
                        status,
                        headers,
                        body: raw.freeze(),
                        is_sse: true,
                        error: "truncated SSE data:".to_string(),
                        leftover_event: snapshot.0,
                        leftover_bytes: snapshot.1,
                    };
                }
                continue;
            }
            parsed_any = true;
            match inspect_sse_block(
                frame,
                &mut seen_items,
                &mut accumulated_text,
                &mut last_response,
                &mut saw_text_delta,
            ) {
                SseBlockKind::Productive => seen_productive = true,
                SseBlockKind::Completed(completed) => {
                    let body = raw.freeze();
                    if let Some(hop) = maybe_reasoning_only_completed(
                        status,
                        headers.clone(),
                        body.clone(),
                        completed,
                        &seen_items,
                        &accumulated_text,
                        seen_productive,
                    ) {
                        return hop;
                    }
                    return passthrough_inspected(status, headers, body);
                }
                SseBlockKind::TerminalOther => {
                    return passthrough_inspected(status, headers, raw.freeze());
                }
                SseBlockKind::Ignore(_) => {}
            }
        }
        if !parsed_any && leftover_is_truncated_sse(leftover) {
            let snapshot = leftover_snapshot(&parse_buffer, &[], last_event);
            log::warn!(
                "[Codex] Grok Responses SSE truncated while inspecting (event={:?} leftover={} bytes)",
                snapshot.0,
                snapshot.1
            );
            return InspectedTurn::StreamBroken {
                status,
                headers,
                body: raw.freeze(),
                is_sse: true,
                error: "truncated SSE data:".to_string(),
                leftover_event: snapshot.0,
                leftover_bytes: snapshot.1,
            };
        }
    }

    if eof_is_productive(seen_productive, &seen_items, &accumulated_text) {
        // Real output without `response.completed` is a truncated stream, not a
        // finished turn. Retry once then rewrite incomplete — never tell Codex
        // the hop completed.
        let leftover = leftover_snapshot(&parse_buffer, &utf8_remainder, last_event.clone());
        return InspectedTurn::StreamBroken {
            status,
            headers,
            body: raw.freeze(),
            is_sse: saw_sse || declared_sse,
            error: "missing terminal after productive output".to_string(),
            leftover_event: leftover.0,
            leftover_bytes: leftover.1,
        };
    }

    let completed = stub_response(last_response, &seen_items);
    let reason = empty_hop_reason(&completed, &seen_items, &accumulated_text, true);
    InspectedTurn::ReasoningOnly {
        status,
        headers,
        body: raw.freeze(),
        completed_response: completed,
        is_sse: saw_sse || declared_sse,
        reason,
    }
}

enum SseBlockKind {
    Ignore(String),
    Productive,
    Completed(Value),
    TerminalOther,
}

fn inspect_sse_block(
    block: &str,
    seen_items: &mut Vec<Value>,
    accumulated_text: &mut String,
    last_response: &mut Option<Value>,
    saw_text_delta: &mut bool,
) -> SseBlockKind {
    let Some((event, value)) = parse_sse_block(block) else {
        return SseBlockKind::Ignore(String::new());
    };
    if let Some(response) = value.get("response") {
        if response.is_object() {
            *last_response = Some(response.clone());
        }
    }
    if event == "response.output_text.delta" {
        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
            *saw_text_delta = true;
            accumulated_text.push_str(delta);
        }
    }
    if let Some(item) = value.get("item").cloned() {
        if item.is_object() {
            if item.get("type").and_then(Value::as_str) == Some("message") {
                let text = extract_message_text(&item);
                if !text.is_empty()
                    && (accumulated_text.is_empty()
                        || text.starts_with(accumulated_text.as_str())
                        || text.len() > accumulated_text.len())
                {
                    *accumulated_text = text;
                }
            }
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
        _ => SseBlockKind::Ignore(event),
    }
}

fn leftover_sse_frames(leftover: &str) -> Vec<&str> {
    let frames = leftover_event_frames(leftover);
    if frames.len() == 1 && parse_sse_block(frames[0]).is_none() {
        let data_frames = leftover_data_frames(frames[0]);
        if data_frames.len() > 1
            && data_frames
                .iter()
                .any(|frame| parse_sse_block(frame).is_some())
        {
            return data_frames;
        }
    }
    frames
}

fn leftover_event_frames(leftover: &str) -> Vec<&str> {
    let trimmed = leftover.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut starts = Vec::new();
    if trimmed.starts_with("event:") || trimmed.starts_with("data:") {
        starts.push(0);
    }
    for (idx, _) in trimmed.match_indices("\nevent:") {
        starts.push(idx + 1);
    }
    for (idx, _) in trimmed.match_indices("\r\nevent:") {
        starts.push(idx + 2);
    }
    split_frames(trimmed, &mut starts)
}

fn leftover_data_frames(block: &str) -> Vec<&str> {
    let mut starts = vec![0];
    for needle in ["\ndata:", "\r\ndata:"] {
        let mut search = 0;
        while let Some(rel) = block[search..].find(needle) {
            let idx = search + rel;
            let after = &block[idx + needle.len()..];
            if after.trim_start().starts_with('{') {
                starts.push(idx + needle.len() - "data:".len());
            }
            search = idx + needle.len();
        }
    }
    split_frames(block, &mut starts)
}

fn split_frames<'a>(trimmed: &'a str, starts: &mut Vec<usize>) -> Vec<&'a str> {
    starts.sort_unstable();
    starts.dedup();
    if starts.is_empty() {
        return vec![trimmed];
    }
    let mut frames = Vec::with_capacity(starts.len());
    for i in 0..starts.len() {
        let end = starts.get(i + 1).copied().unwrap_or(trimmed.len());
        let frame = trimmed[starts[i]..end].trim();
        if !frame.is_empty() {
            frames.push(frame);
        }
    }
    frames
}

fn leftover_is_truncated_sse(leftover: &str) -> bool {
    let trimmed = leftover.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.starts_with("data:")
        || trimmed.contains("\ndata:")
        || trimmed.contains("\r\ndata:")
        || (trimmed.starts_with("event:") && trimmed.contains("data:"))
}

fn maybe_reasoning_only_completed(
    status: http::StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
    completed: Value,
    seen_items: &[Value],
    accumulated_text: &str,
    seen_productive: bool,
) -> Option<InspectedTurn> {
    if seen_productive {
        return None;
    }
    if classify_completed_response(&completed, seen_items) == ResponsesTurnKind::Productive {
        return None;
    }
    let output_items = completed
        .get("output")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(seen_items);
    // Full progress/reasoning in completed.output wins over a token prefix
    // like "I'll" / "我" still sitting in accumulated_text.
    if items_have_progress_only_message(output_items)
        || items_have_progress_only_message(seen_items)
        || items_have_reasoning(output_items)
        || items_have_reasoning(seen_items)
    {
        return Some(InspectedTurn::ReasoningOnly {
            status,
            headers,
            body,
            reason: empty_hop_reason(&completed, seen_items, accumulated_text, false),
            completed_response: completed,
            is_sse: true,
        });
    }
    if eof_is_productive(false, seen_items, accumulated_text) {
        return None;
    }
    Some(InspectedTurn::ReasoningOnly {
        status,
        headers,
        body,
        reason: empty_hop_reason(&completed, seen_items, accumulated_text, false),
        completed_response: completed,
        is_sse: true,
    })
}

fn eof_is_productive(seen_productive: bool, seen_items: &[Value], accumulated_text: &str) -> bool {
    if seen_productive {
        return true;
    }
    if classify_output_items(seen_items) == ResponsesTurnKind::Productive {
        return true;
    }
    let trimmed = accumulated_text.trim();
    !trimmed.is_empty() && !is_progress_only_text(trimmed)
}

fn leftover_snapshot(
    parse_buffer: &str,
    utf8_remainder: &[u8],
    last_event: Option<String>,
) -> (Option<String>, usize) {
    let leftover_bytes = parse_buffer.len().saturating_add(utf8_remainder.len());
    let event = leftover_event_name(parse_buffer).or(last_event);
    (event, leftover_bytes)
}

fn leftover_event_name(leftover: &str) -> Option<String> {
    for line in leftover.lines() {
        if let Some(event) = strip_sse_field(line, "event") {
            let event = event.trim();
            if !event.is_empty() {
                return Some(event.to_string());
            }
        }
        if let Some(data) = strip_sse_field(line, "data") {
            if let Ok(value) = serde_json::from_str::<Value>(data) {
                if let Some(event) = value.get("type").and_then(Value::as_str) {
                    return Some(event.to_string());
                }
            }
            if let Some(event) = truncated_json_type(data) {
                return Some(event);
            }
        }
    }
    None
}

fn truncated_json_type(data: &str) -> Option<String> {
    let marker = "\"type\"";
    let idx = data.find(marker)?;
    let rest = data[idx + marker.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let event = &rest[..end];
    if event.is_empty() {
        None
    } else {
        Some(event.to_string())
    }
}

fn stub_response(last_response: Option<Value>, seen_items: &[Value]) -> Value {
    let mut response = last_response.unwrap_or_else(|| json!({}));
    if response.get("output").is_none() {
        response["output"] = json!(seen_items);
    }
    response
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
    let kind = classify_completed_response(response, &[]);
    if kind == ResponsesTurnKind::ReasoningOnly
        && matches!(response_status, Some("completed") | None)
    {
        let text = response
            .get("output")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(extract_message_text)
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        let items = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let reason = empty_hop_reason(response, &items, &text, false);
        return InspectedTurn::ReasoningOnly {
            status,
            headers,
            body,
            completed_response: response.clone(),
            is_sse: false,
            reason,
        };
    }
    InspectedTurn::Passthrough(ProxyResponse::buffered(status, headers, body))
}

fn passthrough_inspected(
    status: http::StatusCode,
    headers: http::HeaderMap,
    body: Bytes,
) -> InspectedTurn {
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

    fn assistant_message(text: &str) -> Value {
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        })
    }

    #[test]
    fn same_card_followup_never_maps_stream_broken_to_passthrough() {
        assert_eq!(
            same_card_followup(SameCardKind::StreamBroken, 0, 0),
            SameCardFollowup::RetryOriginal
        );
        assert_eq!(
            same_card_followup(SameCardKind::StreamBroken, 1, 0),
            SameCardFollowup::RewriteIncomplete
        );
        assert_eq!(
            same_card_followup(SameCardKind::EmptyHop, 0, 0),
            SameCardFollowup::ContinueEmpty
        );
        assert_eq!(
            same_card_followup(SameCardKind::EmptyHop, 0, 2),
            SameCardFollowup::RewriteIncomplete
        );
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
            classify_output_items(&[assistant_message("done")]),
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
    fn classify_short_progress_only_as_empty() {
        assert!(is_progress_only_text(
            "I'll start by inspecting the screenshot you shared."
        ));
        assert!(is_progress_only_text(
            "我先看一下你发的截图，然后再看 PPT。"
        ));
        assert!(!is_progress_only_text("done"));
        assert!(!is_progress_only_text(
            "The screenshot shows a login form with an email field."
        ));
        assert!(!is_progress_only_text("Let me be clear: ship the fix."));
        assert!(!is_progress_only_text("I am seeing a race in the parser."));
        assert!(!is_progress_only_text("Please look at src/main.rs:40."));
        assert!(!is_progress_only_text("我打开了 README，结论是应改路由。"));
        assert!(!is_progress_only_text(
            "I'll look at the race in parse.rs and fix it."
        ));
        assert_eq!(
            classify_output_items(&[assistant_message(
                "I'll start by inspecting the screenshot you shared."
            )]),
            ResponsesTurnKind::ReasoningOnly
        );
        assert_eq!(
            classify_output_items(&[
                assistant_message("I'll take a look at the PPT."),
                json!({"type": "function_call", "name": "view_image", "call_id": "c1"})
            ]),
            ResponsesTurnKind::Productive
        );
    }

    #[test]
    fn classify_chinese_status_plans_without_tool_calls_as_progress_only() {
        const HOPS: &[&str] = &[
            "继续处理任务：先核对应输入和清单匹配，再按规则批量判定。",
            "按原计划把剩余条目全部做完：先核对应关系，再批量处理并出最终表。",
            "开始实际处理：先核对输入对应，再把全部条目判定并出表。",
            "这次直接把核对和全部判定做完，不再停在中间步骤。",
            "继续全文筛选：先核对应文件和清单匹配，再按规则批量判定。",
            "马上把剩余条目全部做完：先核对应关系，再批量处理。",
            "现在按规则把全部判定做完，不再停在中间步骤。",
            "直接把核对和全部判定做完，不再停。",
            "我先核对应关系，再批量处理。",
        ];
        for hop in HOPS {
            assert!(is_progress_only_text(hop), "expected progress-only: {hop}");
            assert_eq!(
                classify_output_items(&[assistant_message(hop)]),
                ResponsesTurnKind::ReasoningOnly,
                "expected empty-hop classification: {hop}"
            );
        }
        assert_eq!(
            classify_output_items(&[
                assistant_message("按原计划把剩余条目全部做完：先核对应关系，再批量处理。"),
                json!({"type": "function_call", "name": "read_file", "call_id": "c1"})
            ]),
            ResponsesTurnKind::Productive
        );
        assert!(!is_progress_only_text("按原计划已经做完，结果如下。"));
        assert!(!is_progress_only_text("开始实际处理后发现三处应改。"));
        assert!(!is_progress_only_text("继续用现有实现，结论是路由没错。"));
        assert!(!is_progress_only_text("先检查过了，问题在路由。"));
        assert!(!is_progress_only_text("我打开了 README，结论是应改路由。"));
        assert!(!is_progress_only_text("按原计划用现有实现。"));
        assert!(!is_progress_only_text("应该先合并再发布。"));
        assert!(!is_progress_only_text("建议先回滚再修。"));
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
    fn rewrite_drops_truncated_completed_leftover() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\""
        );
        let rewritten = rewrite_sse_completed_to_incomplete(sse);
        assert!(rewritten.contains("event: response.incomplete"));
        assert!(rewritten.contains("\"status\":\"incomplete\""));
        assert!(!rewritten.contains("event: response.completed"));
        assert!(!rewritten.contains("\"status\":\"completed\""));
    }

    #[test]
    fn rewrite_sse_without_completed_appends_incomplete() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n"
        );
        let rewritten = rewrite_sse_completed_to_incomplete(sse);
        assert!(rewritten.contains("event: response.incomplete"));
        assert!(rewritten.contains("\"status\":\"incomplete\""));
        assert!(rewritten.contains("event: response.created"));
    }

    #[test]
    fn append_nudge_and_raise_budget() {
        let mut body = json!({
            "model": "grok-4.6",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "read the ppt"}]}],
            "max_output_tokens": 16384
        });
        prepare_reasoning_continue_request(&mut body, None);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["role"], "developer");
        let nudge = input[1]["content"][0]["text"].as_str().unwrap();
        assert!(nudge.contains("function_call"));
        assert!(nudge.contains("禁止再只回复"));
        assert_eq!(body["max_output_tokens"], 32768);
        let mut high = json!({ "max_output_tokens": 128000 });
        assert_eq!(raise_continue_max_output_tokens(&mut high), 128000);
        assert_eq!(high["max_output_tokens"], 128000);
        assert_eq!(
            same_card_followup(SameCardKind::EmptyHop, 0, 1),
            SameCardFollowup::ContinueEmpty
        );
    }

    #[test]
    fn continue_appends_previous_hop_output_before_nudge() {
        let mut body = json!({
            "model": "grok-4.6",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "max_output_tokens": 16384
        });
        let previous = json!({
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": "looking"}]}
            ]
        });
        prepare_reasoning_continue_request(&mut body, Some(&previous));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[3]["role"], "developer");
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
        let inspected = inspect_native_responses_turn(response).await;
        match inspected {
            InspectedTurn::ReasoningOnly {
                completed_response,
                is_sse,
                reason,
                ..
            } => {
                assert!(is_sse);
                assert_eq!(completed_response["status"], "completed");
                assert_eq!(reason, EmptyHopReason::ReasoningOnly);
            }
            InspectedTurn::Passthrough(_) => panic!("expected reasoning-only completed"),
            InspectedTurn::StreamBroken { .. } => panic!("expected reasoning-only completed"),
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
        let inspected = inspect_native_responses_turn(response).await;
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
        let inspected = inspect_native_responses_turn(response).await;
        assert!(matches!(
            inspected,
            InspectedTurn::ReasoningOnly { is_sse: false, .. }
        ));
    }

    #[tokio::test]
    async fn inspect_eof_without_completed_is_empty_hop() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[]}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, is_sse, .. } => {
                assert!(is_sse);
                assert_eq!(reason, EmptyHopReason::MissingTerminal);
            }
            InspectedTurn::Passthrough(_) => panic!("eof without completed should continue"),
            InspectedTurn::StreamBroken { .. } => panic!("clean eof is not a stream break"),
        }
    }

    #[tokio::test]
    async fn inspect_zero_usage_empty_completed_is_empty_hop() {
        let sse = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, .. } => {
                assert_eq!(reason, EmptyHopReason::ZeroUsage);
            }
            InspectedTurn::Passthrough(_) => panic!("zero-usage empty completed should continue"),
            InspectedTurn::StreamBroken { .. } => panic!("complete sse is not a stream break"),
        }
    }

    #[tokio::test]
    async fn inspect_delta_answer_with_empty_completed_is_passthrough() {
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        assert!(
            matches!(
                inspect_native_responses_turn(response).await,
                InspectedTurn::Passthrough(_)
            ),
            "real delta text must not continue when completed.output is empty"
        );
    }

    #[tokio::test]
    async fn inspect_glued_frames_without_blank_line_classifies_last() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\"}}\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\"}],\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, .. } => {
                assert_eq!(reason, EmptyHopReason::ReasoningOnly);
            }
            InspectedTurn::StreamBroken { .. } => {
                panic!("glued complete frames must not look truncated")
            }
            InspectedTurn::Passthrough(_) => {
                panic!("reasoning-only glued completed should continue")
            }
        }
    }

    #[tokio::test]
    async fn inspect_non_success_is_passthrough() {
        let response = ProxyResponse::buffered(
            StatusCode::BAD_GATEWAY,
            sse_headers(),
            Bytes::from("upstream 502"),
        );
        assert!(matches!(
            inspect_native_responses_turn(response).await,
            InspectedTurn::Passthrough(_)
        ));
    }

    #[tokio::test]
    async fn inspect_truncated_json_body_is_stream_broken() {
        let response = ProxyResponse::buffered(
            StatusCode::OK,
            json_headers(),
            Bytes::from("{\"status\":\"completed\",\"output\":["),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::StreamBroken { is_sse, error, .. } => {
                assert!(!is_sse);
                assert!(error.contains("truncated JSON"));
            }
            InspectedTurn::Passthrough(_) => panic!("truncated json should retry"),
            InspectedTurn::ReasoningOnly { .. } => panic!("truncated json should retry"),
        }
    }

    #[tokio::test]
    async fn inspect_short_real_answer_is_passthrough() {
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}]}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        assert!(matches!(
            inspect_native_responses_turn(response).await,
            InspectedTurn::Passthrough(_)
        ));
    }

    #[tokio::test]
    async fn inspect_progress_token_deltas_still_empty_hop() {
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"I'll\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\" start by inspecting the screenshot you shared.\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"I'll start by inspecting the screenshot you shared.\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"I'll start by inspecting the screenshot you shared.\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":18}}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, .. } => {
                assert_eq!(reason, EmptyHopReason::ProgressOnly);
            }
            InspectedTurn::Passthrough(_) => {
                panic!("token-prefixed progress must not flush before completed")
            }
            InspectedTurn::StreamBroken { .. } => panic!("complete sse is not a stream break"),
        }
    }

    #[tokio::test]
    async fn inspect_progress_prefix_delta_then_full_item_is_empty_hop() {
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"I'll\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"I'll start by inspecting the screenshot you shared.\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"I'll start by inspecting the screenshot you shared.\"}]}]}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, .. } => {
                assert_eq!(reason, EmptyHopReason::ProgressOnly);
            }
            InspectedTurn::Passthrough(_) => {
                panic!("prefix I'll plus full progress item must continue, not passthrough")
            }
            InspectedTurn::StreamBroken { .. } => panic!("complete sse is not a stream break"),
        }
    }

    #[test]
    fn rewrite_glued_frames_without_blank_line_rewrites_completed() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\"}}\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\"}]}}"
        );
        let rewritten = rewrite_sse_completed_to_incomplete(sse);
        assert!(rewritten.contains("event: response.incomplete"));
        assert!(rewritten.contains("\"status\":\"incomplete\""));
        assert!(!rewritten.contains("event: response.completed"));
        assert!(rewritten.contains("\"type\":\"reasoning\""));
    }

    #[test]
    fn rewrite_multi_event_single_trailing_blank_does_not_keep_completed() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\"}]}}\n\n"
        );
        let rewritten = rewrite_sse_completed_to_incomplete(sse);
        assert!(rewritten.contains("event: response.incomplete"));
        assert!(!rewritten.contains("event: response.completed"));
    }

    #[tokio::test]
    async fn inspect_complete_last_frame_ignores_trailing_utf8_remainder() {
        let mut sse =
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}".to_vec();
        sse.push(0xe4);
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, .. } => {
                assert_eq!(reason, EmptyHopReason::ZeroUsage);
            }
            InspectedTurn::StreamBroken { .. } => {
                panic!("trailing incomplete utf-8 must not poison a complete last frame")
            }
            InspectedTurn::Passthrough(_) => panic!("empty completed should continue"),
        }
    }

    #[tokio::test]
    async fn inspect_chinese_status_plan_without_tool_call_is_empty_hop() {
        let text = "按原计划把剩余条目全部做完：先核对应关系，再批量处理并出最终表。";
        let item = assistant_message(text);
        let delta = json!({ "type": "response.output_text.delta", "delta": text });
        let done = json!({
            "type": "response.output_item.done",
            "item": item.clone()
        });
        let completed = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [item],
                "usage": { "input_tokens": 12, "output_tokens": 18 }
            }
        });
        let sse = format!(
            "event: response.output_text.delta\ndata: {}\n\n\
             event: response.output_item.done\ndata: {}\n\n\
             event: response.completed\ndata: {}\n\n",
            delta, done, completed
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, .. } => {
                assert_eq!(reason, EmptyHopReason::ProgressOnly);
            }
            InspectedTurn::Passthrough(_) => {
                panic!("chinese status/plan with no tool call must continue, not complete")
            }
            InspectedTurn::StreamBroken { .. } => panic!("complete sse is not a stream break"),
        }
    }

    #[tokio::test]
    async fn inspect_chinese_progress_token_deltas_still_empty_hop() {
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"我\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"先看一下你发的截图，然后再看 PPT。\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"我先看一下你发的截图，然后再看 PPT。\"}]}]}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, .. } => {
                assert_eq!(reason, EmptyHopReason::ProgressOnly);
            }
            InspectedTurn::Passthrough(_) => panic!("chinese progress tokens should continue"),
            InspectedTurn::StreamBroken { .. } => panic!("complete sse is not a stream break"),
        }
    }

    #[tokio::test]
    async fn inspect_complete_last_frame_without_blank_line_is_not_truncated() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, .. } => {
                assert_eq!(reason, EmptyHopReason::ZeroUsage);
            }
            InspectedTurn::StreamBroken { .. } => {
                panic!("complete completed JSON missing blank line is not truncated")
            }
            InspectedTurn::Passthrough(_) => panic!("empty completed should continue"),
        }
    }

    #[tokio::test]
    async fn inspect_short_real_answer_eof_without_completed_is_stream_broken() {
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::StreamBroken { error, .. } => {
                assert!(error.contains("missing terminal"));
            }
            InspectedTurn::Passthrough(_) => {
                panic!("productive eof without completed must retry, not passthrough")
            }
            InspectedTurn::ReasoningOnly { .. } => {
                panic!("real answer is not an empty hop")
            }
        }
    }

    #[tokio::test]
    async fn inspect_long_answer_does_not_passthrough_before_completed() {
        let long = "A".repeat(120);
        let sse = format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{long}\"}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::StreamBroken { error, .. } => {
                assert!(error.contains("missing terminal"));
            }
            InspectedTurn::Passthrough(_) => {
                panic!("text over 80 chars must not flush before a terminal event")
            }
            InspectedTurn::ReasoningOnly { .. } => {
                panic!("long real answer is not an empty hop")
            }
        }
    }

    #[test]
    fn continue_input_converts_output_text_and_strips_reasoning_extras() {
        let mut body = json!({ "input": [] });
        let previous = json!({
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "hi"}]
                },
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [],
                    "encrypted_content": "secret"
                }
            ]
        });
        append_previous_hop_output(&mut body, &previous);
        let input = body["input"].as_array().expect("input array");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert!(input[0].get("status").is_none());
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["id"], "rs_1");
        assert!(input[1].get("encrypted_content").is_none());
    }

    #[tokio::test]
    async fn inspect_short_progress_sentence_is_empty_hop() {
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"I'll start by inspecting the screenshot you shared.\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"I'll start by inspecting the screenshot you shared.\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"I'll start by inspecting the screenshot you shared.\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":18}}}\n\n"
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly { reason, .. } => {
                assert_eq!(reason, EmptyHopReason::ProgressOnly);
            }
            InspectedTurn::Passthrough(_) => panic!("progress-only message should continue"),
            InspectedTurn::StreamBroken { .. } => panic!("complete sse is not a stream break"),
        }
    }

    #[tokio::test]
    async fn inspect_truncated_sse_is_stream_broken() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\""
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::StreamBroken {
                leftover_event,
                leftover_bytes,
                error,
                ..
            } => {
                assert!(leftover_bytes > 0);
                assert_eq!(leftover_event.as_deref(), Some("response.completed"));
                assert!(error.contains("truncated"));
            }
            InspectedTurn::Passthrough(_) => panic!("truncated completed json should retry"),
            InspectedTurn::ReasoningOnly { .. } => panic!("truncated completed json should retry"),
        }
    }

    #[tokio::test]
    async fn inspect_invalid_utf8_tail_is_empty_hop_not_stream_broken() {
        let mut sse =
            b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n".to_vec();
        sse.push(0xff);
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::ReasoningOnly {
                reason: EmptyHopReason::MissingTerminal,
                ..
            } => {}
            InspectedTurn::StreamBroken { .. } => {
                panic!("invalid utf-8 tail must not 502 / stream-break")
            }
            InspectedTurn::Passthrough(_) => panic!("empty created-only hop should continue"),
            InspectedTurn::ReasoningOnly { reason, .. } => {
                panic!("unexpected empty-hop reason {reason:?}")
            }
        }
    }

    #[tokio::test]
    async fn inspect_idle_timeout_is_stream_broken() {
        let sse = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n";
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([Ok(Bytes::from(sse))]).chain(stream::pending()),
        );
        match inspect_native_responses_turn_timed(response, Some(Duration::from_millis(80))).await {
            InspectedTurn::StreamBroken { error, .. } => {
                assert!(error.contains("idle timeout"));
            }
            InspectedTurn::Passthrough(_) => panic!("idle hang should not passthrough"),
            InspectedTurn::ReasoningOnly { .. } => panic!("idle hang should not continue-as-empty"),
        }
    }

    #[tokio::test]
    async fn inspect_chunk_error_is_stream_broken() {
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([
                Ok(Bytes::from(
                    "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
                )),
                Err(std::io::Error::other("error decoding response body")),
            ]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::StreamBroken {
                error,
                leftover_event,
                ..
            } => {
                assert!(error.contains("decoding response body"));
                assert_eq!(leftover_event.as_deref(), Some("response.created"));
            }
            InspectedTurn::Passthrough(_) => panic!("chunk error should retry, not passthrough"),
            InspectedTurn::ReasoningOnly { .. } => panic!("chunk error should retry, not continue"),
        }
    }

    #[tokio::test]
    async fn stream_broken_rewrites_to_incomplete_200() {
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            sse_headers(),
            stream::iter([
                Ok(Bytes::from(
                    "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
                )),
                Err(std::io::Error::other("error decoding response body")),
            ]),
        );
        match inspect_native_responses_turn(response).await {
            InspectedTurn::StreamBroken {
                status,
                headers,
                body,
                is_sse,
                ..
            } => {
                let rewritten =
                    reasoning_only_to_incomplete_response(status, headers, body, is_sse);
                assert_eq!(rewritten.status(), StatusCode::OK);
                let bytes = rewritten.bytes_with_limit(64 * 1024).await.unwrap();
                let text = String::from_utf8_lossy(&bytes);
                assert!(text.contains("event: response.incomplete"));
                assert!(!text.contains("event: response.completed"));
            }
            _ => panic!("expected StreamBroken"),
        }
    }
}
