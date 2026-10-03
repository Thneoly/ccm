//! Anthropic <-> OpenAI-compatible protocol translation engine (v0.4 M1).
//!
//! Pure functions plus an explicit SSE state machine translating an OpenAI
//! `chat/completions` chunk stream into an Anthropic `/v1/messages` event
//! stream. No async, no IO, no tokio, and zero dependencies on the rest of
//! the crate: `serde_json` and `std` only. The proxy wiring lives in
//! `src/proxy.rs`; this module stays pure and IO-free.
//!
//! Construction policy (honest default): translated requests are built
//! field-by-field into a new object. Anything not explicitly listed in
//! [`translate_request`] is dropped by construction, not passed through.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Map, Value};

/// Per-frame buffer cap for the SSE translator: 1 MiB. The whole stream is
/// never buffered; only the current partially-received frame lives in memory.
const SSE_FRAME_CAP: usize = 1024 * 1024;

/// Slot ceiling for accumulated tool_calls, indexed by the upstream
/// `tool_calls[].index` value. Anthropic accepts at most 128 tools per
/// request and OpenAI at most 128 parallel function calls, so an index at
/// or above this is malformed input; without the ceiling a crafted frame
/// could make `index + 1` wrap (panic on the slot access) or request a
/// giant `resize_with` (a failed allocation aborts the whole process).
const MAX_TOOL_CALL_SLOTS: usize = 128;

// ===========================================================================
// Errors
// ===========================================================================

/// Translation failures. `Display` messages are written to fit inside the
/// proxy's existing `resolve error: failed to translate request: ...` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateError {
    /// The inbound Anthropic request is not shaped as expected.
    InvalidRequest(String),
    /// The upstream OpenAI response is not shaped as expected.
    InvalidResponse(String),
    /// An SSE data frame exceeded the 1 MiB per-frame buffer cap.
    EventTooLarge,
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TranslateError::InvalidRequest(msg) => {
                write!(f, "invalid Anthropic request: {msg}")
            }
            TranslateError::InvalidResponse(msg) => {
                write!(f, "invalid OpenAI response: {msg}")
            }
            TranslateError::EventTooLarge => {
                write!(f, "SSE data frame exceeded the 1 MiB cap")
            }
        }
    }
}

impl std::error::Error for TranslateError {}

// ===========================================================================
// Once-per-process drop warnings
// ===========================================================================

/// Categories of dropped/truncated request fields. Each warns exactly once
/// per process via `eprintln!`; tests never depend on the warning output.
#[derive(Clone, Copy)]
enum DropCategory {
    CacheControl,
    Thinking,
    TopK,
    ParallelToolUse,
    StopSequences,
    UnmappableMessage,
}

impl DropCategory {
    fn message(self) -> &'static str {
        match self {
            DropCategory::CacheControl => {
                "dropping cache_control; requests via openai-compatible providers get no prompt-cache benefit"
            }
            DropCategory::Thinking => {
                "dropping thinking; extended thinking is not supported by openai-compatible providers"
            }
            DropCategory::TopK => "dropping top_k; no OpenAI equivalent exists",
            DropCategory::ParallelToolUse => {
                "dropping tool_choice.disable_parallel_tool_use; no OpenAI equivalent exists"
            }
            DropCategory::StopSequences => {
                "truncating stop_sequences to 4 entries; OpenAI accepts at most 4 stop sequences"
            }
            DropCategory::UnmappableMessage => {
                "degrading a message with no translatable blocks to a placeholder; non-base64 images and other unmappable blocks are dropped"
            }
        }
    }

    fn flag(self) -> &'static AtomicBool {
        static FLAGS: [AtomicBool; 6] = [
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
        ];
        &FLAGS[self as usize]
    }
}

fn warn_dropped(category: DropCategory) {
    if !category.flag().swap(true, Ordering::Relaxed) {
        eprintln!("ccm: openai-compatible: {}", category.message());
    }
}

// ===========================================================================
// Request translation (Anthropic -> OpenAI)
// ===========================================================================

/// A translated upstream request plus the stream flag extracted from the
/// inbound Anthropic request.
pub struct PreparedRequest {
    pub body: Value,
    // Consumed by the module's tests and external callers of the engine; the
    // proxy wiring parses the request-level flag once per request via
    // [`request_wants_stream`] instead of per candidate.
    #[allow(dead_code)]
    pub wants_stream: bool,
}

/// Request-level stream flag of an Anthropic request body: true only for an
/// explicit boolean `"stream": true`. The proxy parses this once per request,
/// before candidate iteration.
pub fn request_wants_stream(anthropic: &Value) -> bool {
    anthropic
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Placeholder pushed in place of a message whose every content block was
/// unmappable (e.g. URL-source images): keeps the turn present and the
/// messages array non-empty instead of silently dropping it.
const UNMAPPABLE_MESSAGE_PLACEHOLDER: &str =
    "[ccm: unmappable message content dropped (no translatable blocks)]";

/// Translate an Anthropic `/v1/messages` request body into an OpenAI
/// `chat/completions` request body for upstream model `model_id`.
///
/// Field-by-field explicit construction: anything not listed in the mapping
/// is dropped by construction.
pub fn translate_request(
    anthropic: &Value,
    model_id: &str,
) -> Result<PreparedRequest, TranslateError> {
    let obj = anthropic.as_object().ok_or_else(|| {
        TranslateError::InvalidRequest("request body must be a JSON object".into())
    })?;

    if value_contains_key(anthropic, "cache_control") {
        warn_dropped(DropCategory::CacheControl);
    }
    if obj.contains_key("thinking") {
        warn_dropped(DropCategory::Thinking);
    }
    if obj.contains_key("top_k") {
        warn_dropped(DropCategory::TopK);
    }

    let mut messages: Vec<Value> = Vec::new();

    // system -> first system-role message (block texts concatenated)
    if let Some(system) = obj.get("system") {
        let text = system_text(system);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    let src_messages = obj
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or_else(|| TranslateError::InvalidRequest("messages must be an array".into()))?;

    for msg in src_messages {
        let m = msg.as_object().ok_or_else(|| {
            TranslateError::InvalidRequest("each message must be a JSON object".into())
        })?;
        let role = m.get("role").and_then(|r| r.as_str()).ok_or_else(|| {
            TranslateError::InvalidRequest("each message must have a string role".into())
        })?;

        let mut text_parts: Vec<String> = Vec::new();
        let mut content_parts: Vec<Value> = Vec::new(); // ordered text/image parts
        let mut has_images = false;
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_messages: Vec<Value> = Vec::new();
        let mut saw_thinking = false;
        let mut content_was_string = false;
        let mut saw_non_thinking_block = false;

        match m.get("content") {
            None => {}
            Some(Value::String(s)) => {
                content_was_string = true;
                text_parts.push(s.clone());
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    let Some(b) = block.as_object() else { continue };
                    let block_type = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    // Thinking blocks are an intentional documented drop; any
                    // other block that maps to nothing still represents real
                    // content and must not vanish with the message.
                    if block_type != "thinking" && block_type != "redacted_thinking" {
                        saw_non_thinking_block = true;
                    }
                    match block_type {
                        "text" => {
                            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                text_parts.push(t.to_string());
                                content_parts.push(json!({"type": "text", "text": t}));
                            }
                        }
                        "thinking" | "redacted_thinking" => saw_thinking = true,
                        "tool_use" => {
                            let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let arguments = match b.get("input") {
                                Some(input) => input.to_string(),
                                None => "{}".to_string(),
                            };
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": arguments}
                            }));
                        }
                        "tool_result" => {
                            let tool_call_id =
                                b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                            tool_messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_call_id,
                                "content": tool_result_content(block)
                            }));
                        }
                        "image" => {
                            if let Some(part) = image_part(block) {
                                has_images = true;
                                content_parts.push(part);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(_) => {
                return Err(TranslateError::InvalidRequest(
                    "message content must be a string or an array of blocks".into(),
                ))
            }
        }
        if saw_thinking {
            warn_dropped(DropCategory::Thinking);
        }

        // tool_result responses precede any remaining user text so the tool
        // answers directly follow the assistant tool_calls message.
        let had_tool_messages = !tool_messages.is_empty();
        messages.extend(tool_messages);

        let text = text_parts.join("\n");
        if !tool_calls.is_empty() {
            let content = if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            };
            messages.push(json!({"role": role, "content": content, "tool_calls": tool_calls}));
        } else if has_images {
            // mixed content keeps the original block order
            messages.push(json!({"role": role, "content": content_parts}));
        } else if content_was_string || !text_parts.is_empty() {
            // pure-text message collapses to a plain string
            messages.push(json!({"role": role, "content": text}));
        } else if saw_non_thinking_block && !had_tool_messages {
            // Every block in this message was unmappable (e.g. URL-source
            // images). Dropping the message entirely could empty the
            // messages array (gateways 400 on that) or silently skip a
            // turn, so degrade to an explicit placeholder instead. A
            // thinking-only message stays dropped (documented degradation),
            // and tool_result-only messages already produced their role:tool
            // outputs above.
            warn_dropped(DropCategory::UnmappableMessage);
            messages.push(json!({
                "role": role,
                "content": UNMAPPABLE_MESSAGE_PLACEHOLDER,
            }));
        }
    }

    let messages = merge_adjacent_plain_messages(messages);

    let mut out = Map::new();
    out.insert("model".into(), json!(model_id));
    out.insert("messages".into(), Value::Array(messages));

    if let Some(v) = obj.get("max_tokens") {
        out.insert("max_tokens".into(), v.clone());
    }
    if let Some(v) = obj.get("temperature") {
        out.insert("temperature".into(), v.clone());
    }
    if let Some(v) = obj.get("top_p") {
        out.insert("top_p".into(), v.clone());
    }
    if let Some(seq) = obj.get("stop_sequences").and_then(|v| v.as_array()) {
        if seq.len() > 4 {
            warn_dropped(DropCategory::StopSequences);
        }
        out.insert(
            "stop".into(),
            Value::Array(seq.iter().take(4).cloned().collect()),
        );
    }

    // Non-boolean `stream` values are dropped entirely (by-construction
    // policy); wants_stream is only true for an explicit boolean.
    let wants_stream = request_wants_stream(anthropic);
    if let Some(v) = obj.get("stream").and_then(|v| v.as_bool()) {
        out.insert("stream".into(), json!(v));
    }
    if wants_stream {
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }

    if let Some(tools) = obj.get("tools").and_then(|v| v.as_array()) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                let t = tool.as_object()?;
                let name = t.get("name")?.as_str()?;
                let mut function = Map::new();
                function.insert("name".into(), json!(name));
                if let Some(desc) = t.get("description").and_then(|d| d.as_str()) {
                    function.insert("description".into(), json!(desc));
                }
                let parameters = t.get("input_schema").cloned().unwrap_or_else(|| json!({}));
                function.insert("parameters".into(), strip_schema_cache_control(parameters));
                Some(json!({"type": "function", "function": Value::Object(function)}))
            })
            .collect();
        out.insert("tools".into(), Value::Array(mapped));
    }

    if let Some(tc) = obj.get("tool_choice") {
        if tc.get("disable_parallel_tool_use").is_some() {
            warn_dropped(DropCategory::ParallelToolUse);
        }
        let choice_type = tc.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let mapped = match choice_type {
            "auto" => Some(json!("auto")),
            "any" => Some(json!("required")),
            "none" => Some(json!("none")),
            "tool" => {
                let name = tc.get("name").and_then(|n| n.as_str()).unwrap_or("");
                Some(json!({"type": "function", "function": {"name": name}}))
            }
            _ => None,
        };
        if let Some(mapped) = mapped {
            out.insert("tool_choice".into(), mapped);
        }
    }

    if let Some(user) = obj
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())
    {
        out.insert("user".into(), json!(user));
    }

    Ok(PreparedRequest {
        body: Value::Object(out),
        wants_stream,
    })
}

/// Strip a top-level `cache_control` from a cloned `input_schema`. The
/// schema is the only raw clone in the tools mapping (everything else is
/// rebuilt field-by-field), so a schema-level `cache_control` would
/// otherwise be the one leak past the drop-by-construction policy.
fn strip_schema_cache_control(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        if obj.remove("cache_control").is_some() {
            warn_dropped(DropCategory::CacheControl);
        }
    }
    schema
}

/// Merge adjacent messages that ended up with the same role and plain string
/// content (many gateways reject non-alternating roles). Tool messages and
/// messages carrying `tool_calls` are never merged.
fn merge_adjacent_plain_messages(messages: Vec<Value>) -> Vec<Value> {
    let is_plain_text = |v: &Value| {
        v.get("content").map(|c| c.is_string()).unwrap_or(false)
            && v.get("tool_calls").is_none()
            && v.get("tool_call_id").is_none()
    };
    let mut merged: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        if let Some(prev) = merged.last() {
            if is_plain_text(prev) && is_plain_text(&msg) && prev.get("role") == msg.get("role") {
                let joined = format!(
                    "{}\n{}",
                    prev.get("content").and_then(|c| c.as_str()).unwrap_or(""),
                    msg.get("content").and_then(|c| c.as_str()).unwrap_or("")
                );
                merged.last_mut().unwrap()["content"] = Value::String(joined);
                continue;
            }
        }
        merged.push(msg);
    }
    merged
}

/// True when any object in the JSON tree contains `key` (used to detect
/// `cache_control` anywhere in the request).
fn value_contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|v| value_contains_key(v, key))
        }
        Value::Array(items) => items.iter().any(|v| value_contains_key(v, key)),
        _ => false,
    }
}

/// Concatenated text of an Anthropic `system` field (string or block array).
fn system_text(system: &Value) -> String {
    match system {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Content for a `role: "tool"` message produced from an Anthropic
/// `tool_result` block: string content stays a string; block-array content
/// maps text blocks (and best-effort images) into string or parts form.
fn tool_result_content(block: &Value) -> Value {
    match block.get("content") {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(blocks)) => {
            let mut texts: Vec<String> = Vec::new();
            let mut images: Vec<Value> = Vec::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            texts.push(t.to_string());
                        }
                    }
                    "image" => {
                        if let Some(part) = image_part(b) {
                            images.push(part);
                        }
                    }
                    _ => {}
                }
            }
            if images.is_empty() {
                Value::String(texts.join("\n"))
            } else {
                let mut parts: Vec<Value> = texts
                    .into_iter()
                    .map(|t| json!({"type": "text", "text": t}))
                    .collect();
                parts.extend(images);
                Value::Array(parts)
            }
        }
        _ => Value::String(String::new()),
    }
}

/// Best-effort mapping of an Anthropic base64 `image` block to an OpenAI
/// `image_url` part carrying a data URL. Non-base64 or malformed sources are
/// dropped.
fn image_part(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    if source.get("type").and_then(|t| t.as_str()) != Some("base64") {
        return None;
    }
    let media_type = source
        .get("media_type")
        .and_then(|m| m.as_str())
        .unwrap_or("application/octet-stream");
    let data = source.get("data").and_then(|d| d.as_str())?;
    Some(json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{media_type};base64,{data}")}
    }))
}

// ===========================================================================
// Response translation (OpenAI -> Anthropic, non-streaming)
// ===========================================================================

/// Translate a non-streaming OpenAI `chat/completions` response into an
/// Anthropic `/v1/messages` response body with upstream model `model_id`.
pub fn translate_response(openai: &Value, model_id: &str) -> Result<Value, TranslateError> {
    let obj = openai.as_object().ok_or_else(|| {
        TranslateError::InvalidResponse("response body must be a JSON object".into())
    })?;
    let choice = obj
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or_else(|| TranslateError::InvalidResponse("response has no choices[0]".into()))?;
    let message = choice.get("message").cloned().unwrap_or(Value::Null);

    // text content (omit the block when empty)
    let mut text = String::new();
    match message.get("content") {
        Some(Value::String(s)) => text = s.clone(),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part {
                    Value::String(s) => text.push_str(s),
                    Value::Object(_) => {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let mut content: Vec<Value> = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }

    // tool calls (arguments arrive as a JSON string; parse failure gives {})
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tool_calls {
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let function = tc.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = function
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let input = match serde_json::from_str::<Value>(arguments) {
                Ok(v) => v,
                Err(err) => {
                    eprintln!(
                        "ccm: openai-compatible: tool '{name}' arguments are not valid JSON ({err}); using {{}}"
                    );
                    json!({})
                }
            };
            content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        }
    }

    let finish = choice.get("finish_reason").and_then(|f| f.as_str());
    let id = format!(
        "msg_{}",
        obj.get("id").and_then(|i| i.as_str()).unwrap_or("ccm")
    );

    Ok(json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model_id,
        "content": content,
        "stop_reason": map_finish_reason(finish),
        "stop_sequence": null,
        "usage": map_usage(obj.get("usage")),
    }))
}

/// OpenAI `finish_reason` -> Anthropic `stop_reason`.
/// `stop`, null, missing, and unknown values map to `end_turn`.
fn map_finish_reason(finish: Option<&str>) -> &'static str {
    match finish {
        Some("tool_calls") => "tool_use",
        Some("length") => "max_tokens",
        Some("content_filter") => "refusal",
        _ => "end_turn",
    }
}

/// Decomposed OpenAI usage numbers: (input, cached, output).
/// `cached` prefers `usage.prompt_tokens_details.cached_tokens` and falls
/// back to the DeepSeek top-level `prompt_cache_hit_tokens` variant.
fn usage_parts(usage: Option<&Value>) -> (i64, i64, i64) {
    match usage {
        None => (0, 0, 0),
        Some(u) => {
            let prompt = u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            let cached = u
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_i64())
                .or_else(|| u.get("prompt_cache_hit_tokens").and_then(|v| v.as_i64()))
                .unwrap_or(0)
                .max(0);
            let completion = u
                .get("completion_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            (prompt, cached, completion)
        }
    }
}

/// OpenAI usage -> Anthropic usage. Anthropic `input_tokens` EXCLUDES cached
/// tokens while OpenAI `prompt_tokens` INCLUDES them, so input is
/// `prompt_tokens - cached` floored at 0. No cache_creation equivalent
/// exists; it is omitted.
fn map_usage(usage: Option<&Value>) -> Value {
    let (prompt, cached, completion) = usage_parts(usage);
    let input = (prompt - cached).max(0);
    let mut out = json!({"input_tokens": input, "output_tokens": completion});
    if cached > 0 {
        out["cache_read_input_tokens"] = json!(cached);
    }
    out
}

// ===========================================================================
// Error body translation
// ===========================================================================

/// True when an upstream JSON body or stream frame carries an `error` field
/// worth surfacing: any non-null shape counts. The OpenAI schema uses an
/// object, but string-form errors appear in the wild (OneAPI-class
/// aggregators); `null` is not an error.
pub fn carries_error(body: &Value) -> bool {
    body.get("error").is_some_and(|v| !v.is_null())
}

/// Translate an OpenAI-style terminal error body into the Anthropic error
/// envelope `{"type":"error","error":{type,message}}`. A body that does not
/// parse as an OpenAI error becomes an `api_error` envelope carrying a short
/// fragment of the raw body.
pub fn translate_error_body(openai_error: &Value) -> Value {
    let err = openai_error.get("error").and_then(|e| e.as_object());
    let anthropic_type = match err.and_then(|e| e.get("type")).and_then(|t| t.as_str()) {
        Some("invalid_request_error") => "invalid_request_error",
        Some("authentication_error") => "authentication_error",
        Some("rate_limit_error") => "rate_limit_error",
        Some("api_error") | Some("server_error") => "api_error",
        Some("insufficient_quota") | Some("overloaded") => "overloaded_error",
        _ => "api_error",
    };
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
        // string-form `error` fields carry the message directly
        .or_else(|| {
            openai_error
                .get("error")
                .and_then(|e| e.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("upstream error: {}", raw_fragment(openai_error)));
    json!({"type": "error", "error": {"type": anthropic_type, "message": message}})
}

/// Short prefix of the raw body for error envelopes without a message.
fn raw_fragment(value: &Value) -> String {
    value.to_string().chars().take(120).collect()
}

// ===========================================================================
// SSE translation (OpenAI chunk stream -> Anthropic event stream)
// ===========================================================================

/// One translated Anthropic SSE event: `event` is the event name and `data`
/// is the JSON text of the event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

impl SseEvent {
    /// Wire format of this event: `event: <name>\ndata: <json>\n\n`.
    pub fn to_wire_bytes(&self) -> Vec<u8> {
        format!("event: {}\ndata: {}\n\n", self.event, self.data).into_bytes()
    }
}

/// An accumulated tool_call: id/name from first sighting, argument fragments
/// appended verbatim in arrival order.
#[derive(Debug, Clone)]
struct ToolAccumulator {
    id: String,
    name: String,
    arguments: String,
}

/// Incremental translator from an OpenAI `chat/completions` SSE byte stream
/// to Anthropic `/v1/messages` events.
///
/// Framing: `data: {json}` lines separated by empty lines (LF or CRLF),
/// terminated by `data: [DONE]`. `feed` buffers only the current partial
/// frame (capped at 1 MiB); the whole stream is never buffered.
pub struct SseTranslator {
    model_id: String,
    buf: Vec<u8>,
    message_id: Option<String>,
    started: bool,
    text_block_index: Option<usize>,
    next_block_index: usize,
    tools: Vec<Option<ToolAccumulator>>,
    finish_reason: Option<String>,
    final_usage: Option<Value>,
    closed: bool,
    errored: bool,
}

impl SseTranslator {
    pub fn new(model_id: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
            buf: Vec::new(),
            message_id: None,
            started: false,
            text_block_index: None,
            next_block_index: 0,
            tools: Vec::new(),
            finish_reason: None,
            final_usage: None,
            closed: false,
            errored: false,
        }
    }

    /// True once the translator reached a terminal state: after `[DONE]`
    /// closed the stream, or after any error ended it. The proxy wiring
    /// uses this to end the client response body immediately — a gateway
    /// that holds the 200 connection open after its final frame must not
    /// hang the client while the event stream is already complete.
    pub fn is_done(&self) -> bool {
        self.errored || self.closed
    }

    /// Feed raw upstream bytes; returns translated events for every complete
    /// data frame received so far.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, TranslateError> {
        if self.errored || self.closed {
            return Ok(Vec::new());
        }
        self.buf.extend_from_slice(chunk);
        self.drain_frames()
    }

    /// Close the stream. WHATWG SSE dispatches a complete final line at
    /// EOF: a buffer whose lines are all terminated is a complete frame
    /// that merely lacked the blank separator (e.g. `data: [DONE]\n`).
    /// Only an unterminated partial line is a torn stream, which produces
    /// the `error` event instead of a fake clean close.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>, TranslateError> {
        if self.errored || self.closed {
            return Ok(Vec::new());
        }
        let ends_with_newline = self.buf.ends_with(b"\n");
        let frame: Vec<u8> = std::mem::take(&mut self.buf);
        let mut events = Vec::new();
        if ends_with_newline {
            // dispatch the complete trailing frame (strip its line endings)
            let mut end = frame.len();
            while end > 0 && matches!(frame[end - 1], b'\n' | b'\r') {
                end -= 1;
            }
            if end > 0 {
                events = self.process_frame(&frame[..end]);
            }
        } else if frame.iter().any(|b| !matches!(b, b'\n' | b'\r')) {
            // unterminated partial line: the stream was cut mid-frame
            self.errored = true;
            return Ok(vec![error_event("stream ended mid-frame")]);
        }
        if self.errored || self.closed {
            return Ok(events);
        }
        events.extend(self.close_stream());
        Ok(events)
    }

    /// Split the buffer on empty lines and process each complete frame.
    fn drain_frames(&mut self) -> Result<Vec<SseEvent>, TranslateError> {
        let mut events = Vec::new();
        loop {
            let separator = match (
                find_subslice(&self.buf, b"\n\n"),
                find_subslice(&self.buf, b"\r\n\r\n"),
            ) {
                (Some(a), Some(b)) => Some(if a <= b { (a, 2) } else { (b, 4) }),
                (Some(a), None) => Some((a, 2)),
                (None, Some(b)) => Some((b, 4)),
                (None, None) => None,
            };
            let Some((frame_len, sep_len)) = separator else {
                if self.buf.len() > SSE_FRAME_CAP {
                    return Err(TranslateError::EventTooLarge);
                }
                break;
            };
            if frame_len > SSE_FRAME_CAP {
                return Err(TranslateError::EventTooLarge);
            }
            let frame: Vec<u8> = self.buf.drain(..frame_len + sep_len).collect();
            events.extend(self.process_frame(&frame[..frame_len]));
            if self.errored || self.closed {
                self.buf.clear();
                break;
            }
        }
        Ok(events)
    }

    /// Extract `data:` payload lines from one frame (LF or CRLF, other
    /// fields ignored) and dispatch on `[DONE]` / JSON / invalid JSON.
    fn process_frame(&mut self, frame: &[u8]) -> Vec<SseEvent> {
        let text = String::from_utf8_lossy(frame);
        let mut data_lines: Vec<&str> = Vec::new();
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                data_lines.push(rest);
            }
        }
        if data_lines.is_empty() {
            return Vec::new(); // keepalive or comment-only frame
        }
        let data = data_lines.join("\n");
        let data = data.trim();
        if data.is_empty() {
            return Vec::new();
        }
        if data == "[DONE]" {
            return self.close_stream();
        }
        match serde_json::from_str::<Value>(data) {
            Ok(chunk) => self.process_chunk(&chunk),
            Err(err) => {
                self.errored = true;
                vec![error_event(&format!("invalid JSON in data frame: {err}"))]
            }
        }
    }

    /// Translate one OpenAI stream chunk. A final frame carrying usage but
    /// no choices is captured, not forwarded. A frame carrying an `error`
    /// object terminates the stream (gateways report mid-stream failures
    /// this way on a still-200 response). Other valid frames without
    /// choices or usage are ignored.
    fn process_chunk(&mut self, chunk: &Value) -> Vec<SseEvent> {
        let mut events = Vec::new();
        // capture usage from any chunk carrying it; the canonical source is
        // the final choices-less frame injected by stream_options
        if let Some(usage) = chunk.get("usage") {
            if usage.is_object() {
                self.final_usage = Some(usage.clone());
            }
        }
        // Some gateways (OpenAI under load, Azure content filters,
        // OneAPI-class aggregators) report failure mid-stream as a
        // valid-JSON frame carrying an `error` field on the still-200 SSE
        // response. Dropping it as a choices-less frame would synthesize a
        // clean close over an upstream failure; instead map it through the
        // same envelope translation as non-200 bodies and terminate. Any
        // non-null error shape counts — string-form errors are real in the
        // wild.
        if carries_error(chunk) {
            self.errored = true;
            return vec![sse("error", translate_error_body(chunk))];
        }
        let choice = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .cloned();
        let Some(choice) = choice else {
            return events; // usage-only frame: captured, not forwarded
        };

        if !self.started {
            self.started = true;
            let id = chunk
                .get("id")
                .and_then(|i| i.as_str())
                .map(|s| format!("msg_{s}"))
                .unwrap_or_else(|| "msg_ccm".to_string());
            self.message_id = Some(id.clone());
            events.push(sse(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.model_id,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        // honest placeholder; real usage rides message_delta
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    }
                }),
            ));
        }

        if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if !finish.is_empty() {
                self.finish_reason = Some(finish.to_string());
            }
        }

        let delta = choice.get("delta");

        // text deltas: lazily open content block 0 on the first delta
        if let Some(content) = delta
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
        {
            if !content.is_empty() {
                if self.text_block_index.is_none() {
                    let index = self.next_block_index;
                    self.next_block_index += 1;
                    self.text_block_index = Some(index);
                    events.push(sse(
                        "content_block_start",
                        json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "text", "text": ""},
                        }),
                    ));
                }
                let index = self.text_block_index.unwrap_or(0);
                events.push(sse(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": content},
                    }),
                ));
            }
        }

        // tool_calls: accumulate by index; never streamed live (argument
        // fragments may interleave across indices, while Anthropic allows
        // only one open content block at a time). reasoning_content fields
        // inside deltas are silently ignored.
        if let Some(tool_calls) = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(|t| t.as_array())
        {
            for tc in tool_calls {
                let raw_index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                if raw_index >= MAX_TOOL_CALL_SLOTS as u64 {
                    // An out-of-range index degrades like any other
                    // malformed frame instead of panicking (`index + 1`
                    // wraps at u64::MAX) or attempting a giant resize (a
                    // failed allocation aborts the process).
                    self.errored = true;
                    return vec![error_event(&format!(
                        "tool_calls index {raw_index} exceeds the supported maximum of {}",
                        MAX_TOOL_CALL_SLOTS - 1
                    ))];
                }
                let index = raw_index as usize;
                if self.tools.len() <= index {
                    self.tools.resize_with(index + 1, || None);
                }
                if self.tools[index].is_none() {
                    self.tools[index] = Some(ToolAccumulator {
                        id: tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        arguments: String::new(),
                    });
                }
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    if let Some(tool) = &mut self.tools[index] {
                        tool.arguments.push_str(args);
                    }
                }
            }
        }

        events
    }

    /// Closing sequence: flush tool blocks in index order (closing an open
    /// text block first - blocks close in order), then `message_delta`
    /// carrying the mapped stop reason and merged usage, then `message_stop`
    /// exactly once.
    fn close_stream(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        if !self.started {
            // zero-content stream: still emit a well-formed sequence
            self.started = true;
            let id = self
                .message_id
                .clone()
                .unwrap_or_else(|| "msg_ccm".to_string());
            events.push(sse(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.model_id,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    }
                }),
            ));
        }

        if let Some(index) = self.text_block_index.take() {
            events.push(sse(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ));
        }

        for slot in self.tools.iter_mut() {
            let Some(tool) = slot else { continue };
            let index = self.next_block_index;
            self.next_block_index += 1;
            events.push(sse(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {
                        "type": "tool_use",
                        "id": tool.id,
                        "name": tool.name,
                        "input": {},
                    },
                }),
            ));
            if !tool.arguments.is_empty() {
                events.push(sse(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": tool.arguments},
                    }),
                ));
            }
            events.push(sse(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ));
        }
        self.tools.clear();

        let stop_reason = map_finish_reason(self.finish_reason.as_deref());
        let mut usage = Map::new();
        if let Some(final_usage) = &self.final_usage {
            let (prompt, cached, completion) = usage_parts(Some(final_usage));
            usage.insert("output_tokens".into(), json!(completion));
            if final_usage.get("prompt_tokens").is_some() {
                usage.insert("input_tokens".into(), json!((prompt - cached).max(0)));
                if cached > 0 {
                    usage.insert("cache_read_input_tokens".into(), json!(cached));
                }
            }
        }
        if !usage.contains_key("output_tokens") {
            usage.insert("output_tokens".into(), json!(0));
        }
        events.push(sse(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": Value::Object(usage),
            }),
        ));
        events.push(sse("message_stop", json!({"type": "message_stop"})));
        self.closed = true;
        events
    }
}

/// Build an Anthropic SSE event from a JSON payload.
fn sse(event: &str, data: Value) -> SseEvent {
    SseEvent {
        event: event.to_string(),
        data: data.to_string(),
    }
}

/// Terminal `error` event for mid-stream translation failures. The message
/// continues with the failure detail. Public so the proxy wiring can emit the
/// same envelope for stream wrapper failures (e.g. the frame cap).
pub fn error_event(detail: &str) -> SseEvent {
    sse(
        "error",
        json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": format!("ccm: upstream translation failure: {detail}"),
            },
        }),
    )
}

/// Earliest occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_all(events: &[SseEvent]) -> Vec<Value> {
        events
            .iter()
            .map(|e| serde_json::from_str(&e.data).expect("event data is valid JSON"))
            .collect()
    }

    fn event_names(events: &[SseEvent]) -> Vec<&str> {
        events.iter().map(|e| e.event.as_str()).collect()
    }

    /// Concatenated `text_delta` payloads across an event stream.
    fn text_concat(events: &[SseEvent]) -> String {
        parse_all(events)
            .into_iter()
            .filter(|v| v["delta"]["type"] == "text_delta")
            .filter_map(|v| v["delta"]["text"].as_str().map(str::to_string))
            .collect()
    }

    // 1. system as string and as block array -> single system message
    #[test]
    fn system_string_and_block_array_map_to_single_system_message() {
        let a = translate_request(
            &json!({
                "model": "claude-x",
                "max_tokens": 10,
                "system": "be brief",
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "up-model",
        )
        .unwrap();
        let messages = a.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "be brief"})
        );
        assert_eq!(messages[1], json!({"role": "user", "content": "hi"}));
        assert_eq!(a.body["model"], "up-model");

        let b = translate_request(
            &json!({
                "max_tokens": 10,
                "system": [
                    {"type": "text", "text": "one"},
                    {"type": "text", "text": "two"},
                    {"type": "text", "text": "three", "cache_control": {"type": "ephemeral"}}
                ],
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "m",
        )
        .unwrap();
        let messages = b.body["messages"].as_array().unwrap();
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "one\ntwo\nthree"})
        );
    }

    // 2. tool_use -> tool_calls; tool_result blocks -> role:tool messages
    #[test]
    fn tool_use_and_tool_result_map_to_tool_calls_and_tool_messages() {
        let req = translate_request(
            &json!({
                "max_tokens": 100,
                "messages": [
                    {"role": "user", "content": "weather?"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "SF"}},
                        {"type": "tool_use", "id": "toolu_2", "name": "get_time", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny"},
                        {"type": "tool_result", "tool_use_id": "toolu_2",
                         "content": [{"type": "text", "text": "3pm"}]}
                    ]}
                ]
            }),
            "m",
        )
        .unwrap();
        let messages = req.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0], json!({"role": "user", "content": "weather?"}));
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], json!(null));
        assert_eq!(
            messages[1]["tool_calls"],
            json!([
                {"id": "toolu_1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}},
                {"id": "toolu_2", "type": "function",
                 "function": {"name": "get_time", "arguments": "{}"}}
            ])
        );
        assert_eq!(
            messages[2],
            json!({"role": "tool", "tool_call_id": "toolu_1", "content": "sunny"})
        );
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "toolu_2", "content": "3pm"})
        );
    }

    // 3. tool_choice all four states
    #[test]
    fn tool_choice_maps_all_four_states() {
        let cases = [
            (json!({"type": "auto"}), json!("auto")),
            (json!({"type": "any"}), json!("required")),
            (json!({"type": "none"}), json!("none")),
            (
                json!({"type": "tool", "name": "get_weather"}),
                json!({"type": "function", "function": {"name": "get_weather"}}),
            ),
        ];
        for (anthropic_choice, openai_choice) in cases {
            let req = translate_request(
                &json!({
                    "max_tokens": 10,
                    "tool_choice": anthropic_choice,
                    "messages": [{"role": "user", "content": "hi"}]
                }),
                "m",
            )
            .unwrap();
            assert_eq!(req.body["tool_choice"], openai_choice);
        }
    }

    // 4. stop_sequences longer than 4 truncated to 4
    #[test]
    fn stop_sequences_truncated_to_four() {
        let req = translate_request(
            &json!({
                "max_tokens": 10,
                "stop_sequences": ["a", "b", "c", "d", "e", "f"],
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "m",
        )
        .unwrap();
        assert_eq!(req.body["stop"], json!(["a", "b", "c", "d"]));
    }

    // 5. image base64 -> data URL
    #[test]
    fn image_base64_maps_to_data_url() {
        let req = translate_request(
            &json!({
                "max_tokens": 10,
                "messages": [{"role": "user", "content": [
                    {"type": "image",
                     "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}},
                    {"type": "text", "text": "what is this?"}
                ]}]
            }),
            "m",
        )
        .unwrap();
        assert_eq!(
            req.body["messages"][0]["content"],
            json!([
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}},
                {"type": "text", "text": "what is this?"}
            ])
        );
    }

    // 6. adjacent same-role text messages merged
    #[test]
    fn adjacent_same_role_text_messages_merge() {
        let req = translate_request(
            &json!({
                "max_tokens": 10,
                "messages": [
                    {"role": "user", "content": "part one"},
                    {"role": "user", "content": [{"type": "text", "text": "part two"}]},
                    {"role": "assistant", "content": "answer"},
                    {"role": "user", "content": "next"}
                ]
            }),
            "m",
        )
        .unwrap();
        let messages = req.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[0],
            json!({"role": "user", "content": "part one\npart two"})
        );
        assert_eq!(
            messages[1],
            json!({"role": "assistant", "content": "answer"})
        );
        assert_eq!(messages[2], json!({"role": "user", "content": "next"}));
    }

    // 7. dropped keys absent; unknown top-level fields dropped by construction
    #[test]
    fn dropped_and_unknown_fields_absent_from_output() {
        let req = translate_request(
            &json!({
                "max_tokens": 10,
                "top_k": 40,
                "thinking": {"type": "enabled", "budget_tokens": 1024},
                "future_field": {"nested": true},
                "messages": [
                    {"role": "user", "content": [
                        {"type": "text", "text": "q", "cache_control": {"type": "ephemeral"}}
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "internal", "signature": "sig1"},
                        {"type": "text", "text": "a"}
                    ]}
                ]
            }),
            "m",
        )
        .unwrap();
        let serialized = req.body.to_string();
        assert!(!serialized.contains("cache_control"));
        assert!(!serialized.contains("thinking"));
        assert!(!serialized.contains("top_k"));
        assert!(!serialized.contains("future_field"));
        assert_eq!(req.body["messages"][0]["content"], "q");
        assert_eq!(req.body["messages"][1]["content"], "a");
    }

    // 8. wants_stream true injects stream_options
    #[test]
    fn stream_flag_and_stream_options() {
        let on = translate_request(
            &json!({
                "max_tokens": 10, "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "m",
        )
        .unwrap();
        assert!(on.wants_stream);
        assert_eq!(on.body["stream"], json!(true));
        assert_eq!(on.body["stream_options"], json!({"include_usage": true}));

        let off = translate_request(
            &json!({
                "max_tokens": 10, "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "m",
        )
        .unwrap();
        assert!(!off.wants_stream);
        assert_eq!(off.body["stream"], json!(false));
        assert!(off.body.get("stream_options").is_none());

        let absent = translate_request(
            &json!({
                "max_tokens": 10,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "m",
        )
        .unwrap();
        assert!(!absent.wants_stream);
        assert!(absent.body.get("stream").is_none());
        assert!(absent.body.get("stream_options").is_none());
    }

    // 9. finish_reason full table; tool arguments parse failure -> input {}
    #[test]
    fn finish_reason_table_and_unparseable_tool_arguments() {
        let cases = [
            (json!("stop"), json!("end_turn")),
            (json!("tool_calls"), json!("tool_use")),
            (json!("length"), json!("max_tokens")),
            (json!("content_filter"), json!("refusal")),
            (Value::Null, json!("end_turn")),
        ];
        for (finish, want) in cases {
            let openai = json!({
                "id": "r1",
                "choices": [{"index": 0,
                             "message": {"role": "assistant", "content": "hi"},
                             "finish_reason": finish}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 1}
            });
            let out = translate_response(&openai, "m").unwrap();
            assert_eq!(out["stop_reason"], want, "finish_reason {finish}");
        }

        let openai = json!({
            "id": "r2",
            "choices": [{"index": 0, "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": "f", "arguments": "{not json"}}]
            }, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        });
        let out = translate_response(&openai, "m").unwrap();
        assert_eq!(
            out["content"][0],
            json!({"type": "tool_use", "id": "call_1", "name": "f", "input": {}})
        );
        assert_eq!(out["id"], "msg_r2");
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "m");
        assert_eq!(out["stop_sequence"], json!(null));
    }

    // 10. usage subtraction incl. the DeepSeek prompt_cache_hit_tokens variant
    #[test]
    fn usage_subtraction_and_deepseek_cache_variant() {
        let openai = json!({
            "id": "r",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                         "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 7,
                "prompt_tokens_details": {"cached_tokens": 40}
            }
        });
        let out = translate_response(&openai, "m").unwrap();
        assert_eq!(
            out["usage"],
            json!({"input_tokens": 60, "output_tokens": 7, "cache_read_input_tokens": 40})
        );
        assert!(out["usage"].get("cache_creation_input_tokens").is_none());

        let deepseek = json!({
            "id": "r",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 7,
                      "prompt_cache_hit_tokens": 40}
        });
        let out = translate_response(&deepseek, "m").unwrap();
        assert_eq!(
            out["usage"],
            json!({"input_tokens": 60, "output_tokens": 7, "cache_read_input_tokens": 40})
        );
    }

    // 11. error body type table plus unparseable-body fallback
    #[test]
    fn error_body_type_table_and_unparseable_fallback() {
        let cases = [
            ("invalid_request_error", "invalid_request_error"),
            ("authentication_error", "authentication_error"),
            ("rate_limit_error", "rate_limit_error"),
            ("api_error", "api_error"),
            ("server_error", "api_error"),
            ("insufficient_quota", "overloaded_error"),
            ("overloaded", "overloaded_error"),
            ("mystery_type", "api_error"),
        ];
        for (openai_type, anthropic_type) in cases {
            let body = json!({"error": {"message": "boom", "type": openai_type, "code": 500}});
            let out = translate_error_body(&body);
            assert_eq!(
                out,
                json!({"type": "error", "error": {"type": anthropic_type, "message": "boom"}}),
                "openai error type {openai_type}"
            );
        }

        // a body that is not an OpenAI error object -> api_error + raw fragment
        let out = translate_error_body(&json!({"detail": "not an openai error"}));
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "api_error");
        let message = out["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("not an openai error"),
            "message was {message}"
        );
    }

    // 12. SSE happy path: exact event order, text, message_delta usage, stop last
    #[test]
    fn sse_happy_path_event_order_and_content() {
        let f1 = json!({"id": "chatcmpl-1",
                        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hello"}}]});
        let f2 = json!({"id": "chatcmpl-1",
                        "choices": [{"index": 0, "delta": {"content": " world"}, "finish_reason": null}]});
        let f3 = json!({"id": "chatcmpl-1",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
        let f4 = json!({"id": "chatcmpl-1", "choices": [],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 2}});
        let bytes =
            format!("data: {f1}\n\ndata: {f2}\n\ndata: {f3}\n\ndata: {f4}\n\ndata: [DONE]\n\n");

        let mut t = SseTranslator::new("up-model");
        let events = t.feed(bytes.as_bytes()).unwrap();
        let events = [events, t.finish().unwrap()].concat();

        assert_eq!(
            event_names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        let parsed = parse_all(&events);
        assert_eq!(parsed[0]["type"], "message_start");
        assert_eq!(parsed[0]["message"]["id"], "msg_chatcmpl-1");
        assert_eq!(parsed[0]["message"]["type"], "message");
        assert_eq!(parsed[0]["message"]["role"], "assistant");
        assert_eq!(parsed[0]["message"]["model"], "up-model");
        assert_eq!(parsed[1]["index"], 0);
        assert_eq!(parsed[1]["content_block"]["type"], "text");
        let text = format!(
            "{}{}",
            parsed[2]["delta"]["text"].as_str().unwrap(),
            parsed[3]["delta"]["text"].as_str().unwrap()
        );
        assert_eq!(text, "Hello world");
        assert_eq!(parsed[4]["index"], 0);
        assert_eq!(parsed[5]["type"], "message_delta");
        assert_eq!(parsed[5]["delta"]["stop_reason"], "end_turn");
        assert_eq!(parsed[5]["delta"]["stop_sequence"], json!(null));
        assert_eq!(
            parsed[5]["usage"],
            json!({"input_tokens": 10, "output_tokens": 2})
        );
        assert_eq!(parsed[6]["type"], "message_stop");
    }

    // 13. SSE tool path: interleaved fragments across two indices flushed in
    //     index order AFTER finish; concatenated arguments are valid JSON
    #[test]
    fn sse_tool_path_flushes_in_index_order_after_finish() {
        let f1 = json!({"id": "t1", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call_a", "type": "function",
             "function": {"name": "alpha", "arguments": "{\"x\":"}},
            {"index": 1, "id": "call_b", "type": "function",
             "function": {"name": "beta", "arguments": "{\"y\":1}"}}
        ]}}]});
        let f2 = json!({"id": "t1", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "1}"}}
        ]}}]});
        let f3 = json!({"id": "t1",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]});

        let mut t = SseTranslator::new("up");
        let mut events = t
            .feed(format!("data: {f1}\n\ndata: {f2}\n\ndata: {f3}\n\n").as_bytes())
            .unwrap();
        // tool blocks are never streamed live: only message_start so far
        assert_eq!(event_names(&events), ["message_start"]);

        events.extend(t.feed(b"data: [DONE]\n\n").unwrap());
        assert_eq!(
            event_names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        let parsed = parse_all(&events);
        // flush order is tool_call index order: call_a before call_b
        assert_eq!(parsed[1]["index"], 0);
        assert_eq!(
            parsed[1]["content_block"],
            json!({"type": "tool_use", "id": "call_a", "name": "alpha", "input": {}})
        );
        assert_eq!(parsed[2]["delta"]["type"], "input_json_delta");
        let args_a: Value =
            serde_json::from_str(parsed[2]["delta"]["partial_json"].as_str().unwrap()).unwrap();
        assert_eq!(args_a, json!({"x": 1}));
        assert_eq!(parsed[4]["index"], 1);
        assert_eq!(
            parsed[4]["content_block"],
            json!({"type": "tool_use", "id": "call_b", "name": "beta", "input": {}})
        );
        let args_b: Value =
            serde_json::from_str(parsed[5]["delta"]["partial_json"].as_str().unwrap()).unwrap();
        assert_eq!(args_b, json!({"y": 1}));
        assert_eq!(parsed[7]["delta"]["stop_reason"], "tool_use");
    }

    // 14. SSE chunk-boundary robustness: replay the same byte stream split at
    //     EVERY offset (multi-byte UTF-8 and blank-line separators end up
    //     split across boundaries) and assert identical output.
    #[test]
    fn sse_chunk_boundaries_never_change_output() {
        let f1 = json!({"id": "c1", "choices": [{"index": 0, "delta": {"content": "Hi 你好"}}]});
        let f2 = json!({"id": "c1",
                        "choices": [{"index": 0, "delta": {"content": "!"}, "finish_reason": null}]});
        let f3 = json!({"id": "c1",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "length"}]});
        let bytes: Vec<u8> =
            format!("data: {f1}\n\ndata: {f2}\n\ndata: {f3}\n\ndata: [DONE]\n\n").into_bytes();

        // baseline: whole stream in one feed
        let mut base = SseTranslator::new("up");
        let baseline = base.feed(&bytes).unwrap();
        assert_eq!(text_concat(&baseline), "Hi 你好!");
        assert!(base.finish().unwrap().is_empty());

        // every two-chunk split point (covers UTF-8 and separator splits)
        for split in 0..=bytes.len() {
            let mut t = SseTranslator::new("up");
            let mut events = t.feed(&bytes[..split]).unwrap();
            events.extend(t.feed(&bytes[split..]).unwrap());
            events.extend(t.finish().unwrap());
            assert_eq!(events, baseline, "split at offset {split}");
        }

        // byte-by-byte replay is also identical
        let mut t = SseTranslator::new("up");
        let mut events = Vec::new();
        for byte in &bytes {
            events.extend(t.feed(std::slice::from_ref(byte)).unwrap());
        }
        events.extend(t.finish().unwrap());
        assert_eq!(events, baseline);
    }

    // 15. SSE final usage frame -> message_delta.usage merges input and cache
    #[test]
    fn sse_final_usage_frame_merges_into_message_delta() {
        let f1 = json!({"id": "u1", "choices": [{"index": 0, "delta": {"content": "hi"}}]});
        let f2 = json!({"id": "u1", "choices": [],
                        "usage": {"prompt_tokens": 100, "completion_tokens": 3,
                                  "prompt_tokens_details": {"cached_tokens": 40}}});
        let bytes = format!("data: {f1}\n\ndata: {f2}\n\ndata: [DONE]\n\n");
        let mut t = SseTranslator::new("up");
        let events = t.feed(bytes.as_bytes()).unwrap();
        let delta = parse_all(&events)
            .into_iter()
            .find(|v| v["type"] == "message_delta")
            .expect("message_delta present");
        assert_eq!(
            delta["usage"],
            json!({"output_tokens": 3, "input_tokens": 60, "cache_read_input_tokens": 40})
        );
    }

    // 16. SSE invalid JSON frame -> error event, no message_stop ever, terminal
    #[test]
    fn sse_invalid_json_frame_is_terminal_and_never_stops() {
        let good = format!(
            "data: {}\n\n",
            json!({"id": "g", "choices": [{"index": 0, "delta": {"content": "ok"}}]})
        );
        let mut t = SseTranslator::new("up");
        let mut events = t.feed(good.as_bytes()).unwrap();
        events.extend(t.feed(b"data: {not json}\n\n").unwrap());

        let names = event_names(&events);
        assert_eq!(names.last().copied(), Some("error"));
        assert!(!names.contains(&"message_stop"));

        let err: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
        assert_eq!(err["type"], "error");
        assert_eq!(err["error"]["type"], "api_error");
        assert!(err["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("ccm: upstream translation failure:"));

        // terminal state: no further events from feed or finish
        assert!(t.feed(b"data: [DONE]\n\n").unwrap().is_empty());
        assert!(t.finish().unwrap().is_empty());
    }

    // 17. SSE [DONE] with no prior finish_reason -> closes with end_turn
    #[test]
    fn sse_done_without_finish_reason_closes_with_end_turn() {
        let f = json!({"choices": [{"index": 0, "delta": {"content": "hi"}}]}); // no id either
        let bytes = format!("data: {f}\n\ndata: [DONE]\n\n");
        let mut t = SseTranslator::new("up");
        let events = t.feed(bytes.as_bytes()).unwrap();
        assert_eq!(
            event_names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let parsed = parse_all(&events);
        assert_eq!(parsed[0]["message"]["id"], "msg_ccm"); // id fallback
        assert_eq!(parsed[4]["delta"]["stop_reason"], "end_turn");
        assert_eq!(parsed[5]["type"], "message_stop");
    }

    // 18. SSE frame exceeding 1 MiB -> EventTooLarge
    #[test]
    fn sse_frame_exceeding_cap_errors() {
        let mut frame = b"data: ".to_vec();
        frame.extend(std::iter::repeat_n(b'x', SSE_FRAME_CAP + 1));

        let mut t = SseTranslator::new("up");
        match t.feed(&frame) {
            Err(TranslateError::EventTooLarge) => {}
            other => panic!("expected EventTooLarge, got {other:?}"),
        }

        // same outcome when the oversized frame arrives across multiple feeds
        let mut t = SseTranslator::new("up");
        let half = frame.len() / 2;
        assert!(t.feed(&frame[..half]).is_ok());
        match t.feed(&frame[half..]) {
            Err(TranslateError::EventTooLarge) => {}
            other => panic!("expected EventTooLarge, got {other:?}"),
        }
    }

    // 19. SSE torn EOF: finish() with half a frame buffered -> error event
    #[test]
    fn sse_torn_eof_produces_error_not_fake_stop() {
        let good = format!(
            "data: {}\n\n",
            json!({"id": "g", "choices": [{"index": 0, "delta": {"content": "ok"}}]})
        );
        let mut t = SseTranslator::new("up");
        let events = t.feed(good.as_bytes()).unwrap();
        assert!(!events.iter().any(|e| e.event == "message_stop"));

        // torn half-frame: buffered, no events yet
        assert!(t.feed(b"data: {\"choices\"").unwrap().is_empty());

        let tail = t.finish().unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].event, "error");
        let err: Value = serde_json::from_str(&tail[0].data).unwrap();
        assert_eq!(err["type"], "error");
        assert_eq!(err["error"]["type"], "api_error");
        assert!(err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("mid-frame"));

        // terminal: finish again produces nothing
        assert!(t.finish().unwrap().is_empty());
    }

    // 20. message_start usage.input_tokens == 0 (honest placeholder)
    #[test]
    fn sse_message_start_usage_input_tokens_is_zero() {
        let f = json!({"id": "z",
                       "choices": [{"index": 0, "delta": {"content": "hi"},
                                    "finish_reason": "stop"}],
                       "usage": {"prompt_tokens": 999, "completion_tokens": 1}});
        let bytes = format!("data: {f}\n\ndata: [DONE]\n\n");
        let mut t = SseTranslator::new("up");
        let events = t.feed(bytes.as_bytes()).unwrap();
        let start = parse_all(&events)[0].clone();
        assert_eq!(start["type"], "message_start");
        assert_eq!(start["message"]["usage"]["input_tokens"], 0);
        // real usage rides message_delta instead
        let delta = parse_all(&events)
            .into_iter()
            .find(|v| v["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["usage"]["input_tokens"], 999);
    }

    // 21. cache_control nested inside a tool input_schema is stripped (the
    //     one raw-clone path in the tools mapping)
    #[test]
    fn nested_cache_control_in_input_schema_is_stripped() {
        let req = translate_request(
            &json!({
                "max_tokens": 10,
                "tools": [{
                    "name": "get_weather",
                    "description": "w",
                    "input_schema": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                        "cache_control": {"type": "ephemeral"}
                    }
                }],
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "m",
        )
        .unwrap();
        assert!(!req.body.to_string().contains("cache_control"));
        assert_eq!(req.body["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(
            req.body["tools"][0]["function"]["parameters"]["properties"]["city"],
            json!({"type": "string"})
        );
    }

    // 22. non-boolean stream values are dropped entirely
    #[test]
    fn non_bool_stream_value_is_dropped() {
        let req = translate_request(
            &json!({
                "max_tokens": 10, "stream": "true",
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "m",
        )
        .unwrap();
        assert!(!req.wants_stream);
        assert!(req.body.get("stream").is_none());
        assert!(req.body.get("stream_options").is_none());
    }

    // 23. reasoning_content deltas (DeepSeek-style) produce no events, leak
    //     nothing, and do not disturb the sequence
    #[test]
    fn reasoning_content_deltas_are_ignored() {
        let f1 = json!({"id": "r1", "choices": [{"index": 0,
                        "delta": {"role": "assistant", "reasoning_content": "thinking hard"}}]});
        let f2 = json!({"id": "r1", "choices": [{"index": 0,
                        "delta": {"content": "answer", "reasoning_content": "more"}}]});
        let f3 = json!({"id": "r1",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
        let mut t = SseTranslator::new("up");
        let mut events = t
            .feed(format!("data: {f1}\n\ndata: {f2}\n\ndata: {f3}\n\n").as_bytes())
            .unwrap();
        events.extend(t.feed(b"data: [DONE]\n\n").unwrap());
        assert_eq!(
            event_names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(text_concat(&events), "answer");
        assert!(!events.iter().any(|e| e.data.contains("reasoning")));
    }

    // 24. text deltas arriving AFTER tool_call deltas still yield legal
    //     ordering: the text block opens/closes first, tool blocks follow
    #[test]
    fn text_after_tool_call_deltas_keeps_block_order() {
        let f1 = json!({"id": "m1", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call_a", "type": "function",
             "function": {"name": "alpha", "arguments": "{\"a\":1}"}}
        ]}}]});
        let f2 = json!({"id": "m1",
                        "choices": [{"index": 0, "delta": {"content": "working"}}]});
        let f3 = json!({"id": "m1",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]});
        let mut t = SseTranslator::new("up");
        let mut events = t
            .feed(format!("data: {f1}\n\ndata: {f2}\n\ndata: {f3}\n\n").as_bytes())
            .unwrap();
        // tool block is not streamed live; the text block opens when text arrives
        assert_eq!(
            event_names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
        events.extend(t.feed(b"data: [DONE]\n\n").unwrap());
        assert_eq!(
            event_names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let parsed = parse_all(&events);
        assert_eq!(parsed[1]["index"], 0);
        assert_eq!(parsed[1]["content_block"]["type"], "text");
        assert_eq!(parsed[4]["index"], 1);
        assert_eq!(parsed[4]["content_block"]["type"], "tool_use");
        assert_eq!(parsed[7]["delta"]["stop_reason"], "tool_use");
    }

    // 25. WHATWG EOF dispatch: a complete trailing data line without the
    //     blank separator still dispatches; an unterminated partial line is
    //     torn and produces the error event
    #[test]
    fn trailing_complete_line_dispatches_at_eof() {
        let f1 = json!({"id": "chatcmpl-e1", "choices": [{"index": 0,
                        "delta": {"content": "hi"}, "finish_reason": "stop"}]});
        // the stream ends right after "data: [DONE]\n" - no trailing blank line
        let mut t = SseTranslator::new("up");
        let mut events = t
            .feed(format!("data: {f1}\n\ndata: [DONE]\n").as_bytes())
            .unwrap();
        // the first frame was dispatched by its separator; [DONE] is buffered
        assert_eq!(
            event_names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
        events.extend(t.finish().unwrap());
        assert_eq!(
            event_names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        // torn: the final line was cut mid-frame with no terminator
        let mut torn = SseTranslator::new("up");
        let events = torn
            .feed(b"data: {\"id\": \"x\", \"choices\": [{\"inde")
            .unwrap();
        let events = [events, torn.finish().unwrap()].concat();
        assert_eq!(event_names(&events), ["error"]);
        assert!(torn.finish().unwrap().is_empty());
    }

    // 26. an out-of-range tool_calls index is a terminal bad frame — never
    //     a panic (u64::MAX wraps index+1 to 0) or a giant resize (an
    //     allocation failure aborts the whole process)
    #[test]
    fn out_of_range_tool_call_index_is_terminal_bad_frame() {
        for bad in [u64::MAX, 4_294_967_296, MAX_TOOL_CALL_SLOTS as u64] {
            let frame = json!({"id": "b", "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": bad, "id": "call_x", "type": "function",
                 "function": {"name": "boom", "arguments": ""}}
            ]}}]});
            let mut t = SseTranslator::new("up");
            let events = t.feed(format!("data: {frame}\n\n").as_bytes()).unwrap();
            let names = event_names(&events);
            assert_eq!(names.last().copied(), Some("error"), "index {bad}");
            assert!(!names.contains(&"message_stop"), "index {bad}");
            let err: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
            assert!(
                err["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("tool_calls index"),
                "index {bad}"
            );
            // terminal: nothing more from feed or finish
            assert!(t.feed(b"data: [DONE]\n\n").unwrap().is_empty());
            assert!(t.finish().unwrap().is_empty());
        }

        // the highest legal index still accumulates normally
        let ok = json!({"id": "b", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": MAX_TOOL_CALL_SLOTS as u64 - 1, "id": "call_ok", "type": "function",
             "function": {"name": "fine", "arguments": "{}"}}
        ]}}]});
        let mut t = SseTranslator::new("up");
        let events = t
            .feed(format!("data: {ok}\n\ndata: [DONE]\n\n").as_bytes())
            .unwrap();
        assert!(events.iter().any(|e| e.event == "message_stop"));
        assert!(events.iter().any(|e| e.data.contains("\"name\":\"fine\"")));
    }

    // 27. a mid-stream error frame (valid JSON, no choices) terminates the
    //     stream with the translated upstream error — never a faked clean close
    #[test]
    fn sse_error_frame_is_terminal_and_maps_the_envelope() {
        let good = format!(
            "data: {}\n\n",
            json!({"id": "g", "choices": [{"index": 0, "delta": {"content": "partial"}}]})
        );
        let err_frame = json!({"error": {"message": "The server had an error",
                                         "type": "server_error"}});
        let mut t = SseTranslator::new("up");
        let mut events = t.feed(good.as_bytes()).unwrap();
        events.extend(t.feed(format!("data: {err_frame}\n\n").as_bytes()).unwrap());
        // the gateway just closes after the error frame — no [DONE]
        events.extend(t.finish().unwrap());

        let names = event_names(&events);
        assert_eq!(names.last().copied(), Some("error"));
        assert!(!names.contains(&"message_stop"));
        let err: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
        assert_eq!(err["type"], "error");
        assert_eq!(err["error"]["type"], "api_error"); // server_error -> api_error
        assert_eq!(err["error"]["message"], "The server had an error");
        // terminal: nothing more from feed or finish
        assert!(t.feed(b"data: [DONE]\n\n").unwrap().is_empty());
        assert!(t.finish().unwrap().is_empty());
    }

    // 28. error frames keep the Anthropic type mapping (rate_limit), and an
    //     error object wins even when the frame also carries choices
    #[test]
    fn sse_error_frame_type_mapping_and_choices_coexistence() {
        let rl = json!({"error": {"message": "Rate limit reached",
                                  "type": "rate_limit_error"}});
        let mut t = SseTranslator::new("up");
        let events = t.feed(format!("data: {rl}\n\n").as_bytes()).unwrap();
        let err: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
        assert_eq!(err["error"]["type"], "rate_limit_error");
        assert_eq!(err["error"]["message"], "Rate limit reached");

        let hybrid = json!({"error": {"message": "boom", "type": "server_error"},
                            "choices": [{"index": 0, "delta": {"content": "x"}}]});
        let mut t = SseTranslator::new("up");
        let events = t.feed(format!("data: {hybrid}\n\n").as_bytes()).unwrap();
        assert_eq!(event_names(&events).last().copied(), Some("error"));
        assert!(!events.iter().any(|e| e.event == "message_stop"));
    }

    // 31. error frames of any non-null shape terminate the stream: a
    //     string-form error carries the message; `"error": null` is not an
    //     error and the frame's choices process normally
    #[test]
    fn sse_error_frame_shape_coverage() {
        let string_err = json!({"error": "gateway exploded"});
        let mut t = SseTranslator::new("up");
        let events = t
            .feed(format!("data: {string_err}\n\n").as_bytes())
            .unwrap();
        let names = event_names(&events);
        assert_eq!(names.last().copied(), Some("error"));
        assert!(!names.contains(&"message_stop"));
        let err: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
        assert_eq!(err["error"]["type"], "api_error");
        assert_eq!(err["error"]["message"], "gateway exploded");
        // terminal: nothing more from feed or finish
        assert!(t.is_done());
        assert!(t.feed(b"data: [DONE]\n\n").unwrap().is_empty());
        assert!(t.finish().unwrap().is_empty());

        // "error": null is not an error
        let null_err = json!({"error": null,
                              "choices": [{"index": 0, "delta": {"content": "ok"}}]});
        let mut t = SseTranslator::new("up");
        let events = t
            .feed(format!("data: {null_err}\n\ndata: [DONE]\n\n").as_bytes())
            .unwrap();
        assert!(events.iter().any(|e| e.event == "message_stop"));
        assert!(!events.iter().any(|e| e.event == "error"));

        // non-stream error body: string form becomes the message
        let out = translate_error_body(&json!({"error": "boom string"}));
        assert_eq!(out["error"]["type"], "api_error");
        assert_eq!(out["error"]["message"], "boom string");
    }

    // 29. a message whose every block is unmappable (URL-source image)
    //     degrades to a placeholder instead of vanishing — the turn stays
    //     present and the messages array never ends up empty
    #[test]
    fn fully_unmappable_message_degrades_to_placeholder() {
        let req = translate_request(
            &json!({
                "max_tokens": 10,
                "messages": [
                    {"role": "user", "content": [
                        {"type": "image", "source": {"type": "url",
                         "url": "https://example.com/cat.png"}}
                    ]}
                ]
            }),
            "m",
        )
        .unwrap();
        let msgs = req.body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "the turn must not vanish");
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(
            msgs[0]["content"],
            Value::String(UNMAPPABLE_MESSAGE_PLACEHOLDER.to_string())
        );
    }

    // 30. placeholder only when NOTHING mapped: an empty block array and a
    //     thinking-only message stay dropped (documented degradations), and
    //     a mixed message keeps its mapped text
    #[test]
    fn placeholder_only_when_nothing_mapped() {
        let req = translate_request(
            &json!({
                "max_tokens": 10,
                "messages": [
                    {"role": "user", "content": []},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "internal"}
                    ]},
                    {"role": "user", "content": [
                        {"type": "text", "text": "look"},
                        {"type": "image", "source": {"type": "url",
                         "url": "https://example.com/y.png"}}
                    ]}
                ]
            }),
            "m",
        )
        .unwrap();
        let msgs = req.body["messages"].as_array().unwrap();
        // empty-array and thinking-only messages stay dropped; the mixed
        // one maps its text
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "look");
    }
}
