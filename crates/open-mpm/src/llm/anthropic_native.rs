//! Anthropic native `/v1/messages` request/response shim.
//!
//! Why: #59 — when we route to `api.anthropic.com` directly (instead of
//! through OpenRouter's OpenAI-compatible `/v1/chat/completions`) we must
//! speak Anthropic's native wire format. That format is different enough
//! from OpenAI's that a round-trip through `serde_json::to_value` isn't
//! sufficient: `system` is a top-level field, tools use `input_schema`
//! instead of `parameters`, tool results are content blocks, responses are
//! a flat array of content blocks with `stop_reason` instead of nested
//! `choices[0].message.{content, finish_reason}`.
//! What: Two pure functions — `build_anthropic_request` (internal typed
//! messages + OpenAI-shape tools → Anthropic request body) and
//! `parse_anthropic_response` (Anthropic response JSON → `AnthropicResponse`
//! with extracted text, tool calls, stop reason, and cache-aware usage).
//! Test: See module tests at the bottom.

use anyhow::{Context, Result};
use async_openai::types::{
    ChatCompletionMessageToolCall, ChatCompletionRequestMessage, ChatCompletionTool,
    ChatCompletionToolType, FunctionCall,
};
use serde_json::{Value, json};

use crate::perf::TokenUsage;

/// Tool-call shape parsed from an Anthropic native response, already converted
/// to the same internal `ChatCompletionMessageToolCall` type the OpenRouter
/// path returns so the downstream dispatch loop doesn't need to branch.
pub type AnthropicToolCall = ChatCompletionMessageToolCall;

/// Parsed Anthropic `/v1/messages` response.
///
/// Why: Callers need text + tool calls + usage in one pass; keeping them in
/// a struct avoids re-parsing the JSON from multiple sites.
/// What: `text_content` is the concatenation of all `text` blocks;
/// `tool_calls` holds any `tool_use` blocks already normalized to
/// `ChatCompletionMessageToolCall`; `stop_reason` is the raw string from the
/// response; `usage` carries cache-aware token counts.
/// Test: `parse_anthropic_response_extracts_text_and_tool_use`,
/// `parse_anthropic_response_reads_cache_read_tokens`.
#[derive(Debug, Clone)]
pub struct AnthropicResponse {
    pub text_content: Option<String>,
    pub tool_calls: Vec<AnthropicToolCall>,
    /// Retained for observability/debug logging and future end-of-turn
    /// heuristics; the chat loop currently infers termination from the
    /// presence/absence of `tool_calls`.
    #[allow(dead_code)]
    pub stop_reason: String,
    pub usage: TokenUsage,
}

/// Build an Anthropic native `/v1/messages` request body from our internal
/// message list and OpenAI-shape tools.
///
/// Why: The PM + sub-agent paths already assemble typed
/// `ChatCompletionRequestMessage` values and OpenAI-shape `ChatCompletionTool`
/// values; this function converts them to the Anthropic shape so we can
/// opt-in to direct-API routing without changing every call site.
/// What: Extracts a top-level `system` string from the first `role:system`
/// message, rewrites remaining messages into Anthropic's user/assistant
/// format (with tool-result content blocks when a `role:tool` message is
/// present), and maps each tool's `parameters` to `input_schema`. When
/// `enable_caching=true` a `cache_control: {type: "ephemeral"}` marker is
/// attached to the system block and to the last tool definition so
/// subsequent requests hit the prompt cache.
/// Test: `build_anthropic_request_places_system_top_level`,
/// `build_anthropic_request_converts_tool_parameters_to_input_schema`,
/// `build_anthropic_request_strips_provider_prefix_from_model`,
/// `build_anthropic_request_converts_tool_result_messages`.
pub fn build_anthropic_request(
    model: &str,
    messages: &[ChatCompletionRequestMessage],
    tools: &[ChatCompletionTool],
    temperature: f32,
    max_tokens: u32,
    tool_choice: Option<&Value>,
    enable_caching: bool,
) -> Result<Value> {
    // Anthropic's `model` field expects a bare name like `claude-sonnet-4-5`
    // (no `anthropic/` or `openrouter/` prefix). Strip any single leading
    // `vendor/` segment so configs written for OpenRouter keep working.
    let model_name = strip_provider_prefix(model);

    // Round-trip messages through JSON to inspect role/content uniformly.
    let mut system_prompt: Option<String> = None;
    let mut out_messages: Vec<Value> = Vec::with_capacity(messages.len());

    for msg in messages {
        let v = serde_json::to_value(msg).context("serialize chat message for Anthropic")?;
        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "system" => {
                // Take only the FIRST system message's text. Later ones are
                // ignored — our callers only ever send one.
                if system_prompt.is_none() {
                    system_prompt = Some(extract_text_content(v.get("content")));
                }
            }
            "user" => {
                // User messages are already in a compatible shape: a string
                // content field is valid on Anthropic too. Pass through as-is
                // with only the fields Anthropic accepts.
                let content = v.get("content").cloned().unwrap_or(Value::Null);
                out_messages.push(json!({
                    "role": "user",
                    "content": content,
                }));
            }
            "assistant" => {
                out_messages.push(convert_assistant_message(&v));
            }
            "tool" => {
                // Anthropic represents tool results as a user message with a
                // `tool_result` content block. If the previous out_message is
                // already a user message with tool_result content, append;
                // otherwise push a new one.
                let tool_use_id = v
                    .get("tool_call_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let result_text = extract_text_content(v.get("content"));
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": result_text,
                });
                if let Some(last) = out_messages.last_mut()
                    && last.get("role").and_then(|r| r.as_str()) == Some("user")
                    && let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut())
                {
                    arr.push(block);
                    continue;
                }
                out_messages.push(json!({
                    "role": "user",
                    "content": [block],
                }));
            }
            _ => {
                // Unknown roles are dropped with a trace; we don't want to
                // synthesize garbage into the request.
                tracing::debug!(role = %role, "dropping unknown role when building Anthropic request");
            }
        }
    }

    let mut body = json!({
        "model": model_name,
        "max_tokens": max_tokens,
        "temperature": temperature,
        "messages": out_messages,
    });

    if let Some(sys) = system_prompt {
        if enable_caching {
            // Anthropic supports cache_control on the system block.
            body["system"] = json!([
                {
                    "type": "text",
                    "text": sys,
                    "cache_control": {"type": "ephemeral"}
                }
            ]);
        } else {
            body["system"] = Value::String(sys);
        }
    }

    // #29: Conversation-history cache breakpoint.
    //
    // Why: The system-prompt cache (above) only de-duplicates the agent's
    // identity prompt. Long multi-turn workflows (e.g. research phases that
    // accumulate many tool results) re-pay full input rates on every turn for
    // the GROWING conversation history. Adding a second `cache_control`
    // breakpoint on the LAST assistant block lets Anthropic cache up through
    // the most recent assistant turn so the next request only pays cache-read
    // rates (~10%) for everything before it.
    // What: When `enable_caching=true` AND the converted message list contains
    // an assistant message AND the rough token estimate of all messages
    // exceeds 2000 tokens, attach `cache_control: {"type":"ephemeral"}` to the
    // last content block of the most recent assistant message. Anthropic
    // allows up to 4 cache breakpoints per request; we use only 2 (system +
    // last assistant) so callers stay well under the limit.
    // Test: `build_anthropic_request_caches_last_assistant_when_large`,
    // `build_anthropic_request_skips_history_cache_when_small`.
    if enable_caching && history_token_estimate(&out_messages) > 2000 {
        attach_cache_control_to_last_assistant(&mut out_messages);
        // Re-emit messages because we already moved a clone into `body` above.
        body["messages"] = Value::Array(out_messages.clone());
    }

    if !tools.is_empty() {
        let mut tools_out: Vec<Value> = Vec::with_capacity(tools.len());
        for t in tools {
            let tv = serde_json::to_value(t).context("serialize tool for Anthropic")?;
            // OpenAI shape: {type:"function", function:{name,description,parameters}}
            let func = tv.get("function").cloned().unwrap_or(Value::Null);
            let name = func
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = func
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = func
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}}));
            tools_out.push(json!({
                "name": name,
                "description": description,
                "input_schema": input_schema,
            }));
        }
        if enable_caching
            && let Some(last) = tools_out.last_mut()
            && let Some(obj) = last.as_object_mut()
        {
            obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
        }
        body["tools"] = Value::Array(tools_out);
    }

    if let Some(tc) = tool_choice {
        body["tool_choice"] = tc.clone();
    }

    Ok(body)
}

/// Cheap token estimate for an Anthropic-shaped messages array (#29).
///
/// Why: We need to decide whether the conversation has grown large enough to
/// warrant a second cache breakpoint without dragging in a full tokenizer.
/// Anthropic's tokenizer averages ~4 chars/token in English; a char-count /
/// 4 heuristic is plenty accurate to gate "history > 2000 tokens".
/// What: Walks the JSON, summing the byte length of every text string and
/// `tool_result` content found, returning `chars / 4` as a u32.
/// Test: `history_token_estimate_counts_text_blocks`.
fn history_token_estimate(messages: &[Value]) -> u32 {
    let mut chars: usize = 0;
    for m in messages {
        match m.get("content") {
            Some(Value::String(s)) => chars += s.len(),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    if let Some(s) = b.get("text").and_then(|v| v.as_str()) {
                        chars += s.len();
                    }
                    if let Some(s) = b.get("content").and_then(|v| v.as_str()) {
                        chars += s.len();
                    }
                    if let Some(input) = b.get("input") {
                        chars += input.to_string().len();
                    }
                }
            }
            _ => {}
        }
    }
    (chars / 4) as u32
}

/// Attach `cache_control: {type:"ephemeral"}` to the last content block of
/// the most recent assistant message (#29).
///
/// Why: A second cache breakpoint placed at the most recent assistant turn
/// lets every subsequent request hit the prompt cache for the entire history
/// up to that point, dropping read costs by ~90% for long sessions.
/// What: Scans `messages` from the end for an assistant message; normalizes
/// its content to an array of blocks and inserts `cache_control` on the last
/// block. No-op when no assistant turn exists yet.
/// Test: `attach_cache_control_to_last_assistant_inserts_marker`.
fn attach_cache_control_to_last_assistant(messages: &mut [Value]) {
    for m in messages.iter_mut().rev() {
        if m.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        // Normalize content to an array shape so we can attach cache_control
        // to the last block uniformly.
        let content = m.get("content").cloned();
        let mut arr: Vec<Value> = match content {
            Some(Value::Array(a)) => a,
            Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
            _ => vec![],
        };
        if let Some(last) = arr.last_mut()
            && let Some(obj) = last.as_object_mut()
        {
            obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
        }
        m["content"] = Value::Array(arr);
        return;
    }
}

/// Parse a raw Anthropic `/v1/messages` response JSON into `AnthropicResponse`.
///
/// Why: The chat loop already works in terms of `ChatCompletionMessageToolCall`;
/// converting on ingress keeps the tool-dispatch code path identical for both
/// routes.
/// What: Walks `response.content[]`, accumulating `text` blocks into
/// `text_content` and `tool_use` blocks into `tool_calls`. Reads
/// `stop_reason` (defaulting to `"end_turn"`) and extracts usage including
/// `cache_read_input_tokens` / `cache_creation_input_tokens`.
/// Test: `parse_anthropic_response_extracts_text_and_tool_use`,
/// `parse_anthropic_response_reads_cache_read_tokens`,
/// `parse_anthropic_response_empty_content_yields_none`.
pub fn parse_anthropic_response(response: &Value) -> AnthropicResponse {
    let mut text_buf = String::new();
    let mut tool_calls: Vec<AnthropicToolCall> = Vec::new();

    if let Some(blocks) = response.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(s) = block.get("text").and_then(|v| v.as_str()) {
                        if !text_buf.is_empty() {
                            text_buf.push('\n');
                        }
                        text_buf.push_str(s);
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    let arguments =
                        serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                    tool_calls.push(ChatCompletionMessageToolCall {
                        id,
                        r#type: ChatCompletionToolType::Function,
                        function: FunctionCall { name, arguments },
                    });
                }
                _ => {}
            }
        }
    }

    let stop_reason = response
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn")
        .to_string();

    let usage = response
        .get("usage")
        .map(|u| {
            let prompt = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let completion = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let cache_read = u
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let cache_creation = u
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            TokenUsage::new(prompt, completion, cache_read, cache_creation)
        })
        .unwrap_or_default();

    let text_content = if text_buf.is_empty() {
        None
    } else {
        Some(text_buf)
    };

    AnthropicResponse {
        text_content,
        tool_calls,
        stop_reason,
        usage,
    }
}

/// Strip a single leading `vendor/` segment from a model name.
///
/// Why: Direct Anthropic API rejects names like `anthropic/claude-sonnet-4-5`;
/// it wants the bare model id. Callers' configs use the OpenRouter form.
/// What: If `model` contains `/`, returns the substring after the first `/`.
/// Test: `strip_provider_prefix_cases`.
fn strip_provider_prefix(model: &str) -> String {
    match model.split_once('/') {
        Some((_, rest)) => rest.to_string(),
        None => model.to_string(),
    }
}

/// Convert a serialized assistant message (OpenAI shape) into Anthropic's
/// content-block array. Text content becomes a `text` block; any `tool_calls`
/// array becomes `tool_use` blocks.
fn convert_assistant_message(v: &Value) -> Value {
    let mut blocks: Vec<Value> = Vec::new();

    // Text content (string or null).
    if let Some(text) = v.get("content").and_then(|c| c.as_str())
        && !text.is_empty()
    {
        blocks.push(json!({"type": "text", "text": text}));
    }

    if let Some(calls) = v.get("tool_calls").and_then(|c| c.as_array()) {
        for tc in calls {
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let function = tc.get("function").cloned().unwrap_or(Value::Null);
            let name = function
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args_str = function
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
            blocks.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }

    if blocks.is_empty() {
        // Empty assistant turns would be rejected; emit an empty text block
        // so the request stays valid (Anthropic is lenient about empty text).
        blocks.push(json!({"type": "text", "text": ""}));
    }

    json!({"role": "assistant", "content": blocks})
}

/// Best-effort extraction of text content from either a bare string, an
/// array of content blocks, or null. Used for both system and tool-result
/// inputs.
fn extract_text_content(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut buf = String::new();
            for block in arr {
                if let Some(s) = block.get("text").and_then(|v| v.as_str()) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(s);
                }
            }
            buf
        }
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::{
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
        ChatCompletionToolArgs, FunctionObjectArgs,
    };

    fn sys(text: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestSystemMessageArgs::default()
            .content(text)
            .build()
            .unwrap()
            .into()
    }

    fn user(text: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestUserMessageArgs::default()
            .content(text)
            .build()
            .unwrap()
            .into()
    }

    fn tool_msg(call_id: &str, content: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestToolMessageArgs::default()
            .tool_call_id(call_id)
            .content(content)
            .build()
            .unwrap()
            .into()
    }

    fn assistant_tool_call(id: &str, name: &str, args: &str) -> ChatCompletionRequestMessage {
        let call = ChatCompletionMessageToolCall {
            id: id.into(),
            r#type: ChatCompletionToolType::Function,
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        };
        ChatCompletionRequestAssistantMessageArgs::default()
            .tool_calls(vec![call])
            .build()
            .unwrap()
            .into()
    }

    fn make_tool(name: &str, params: Value) -> ChatCompletionTool {
        let func = FunctionObjectArgs::default()
            .name(name)
            .description("a tool")
            .parameters(params)
            .build()
            .unwrap();
        ChatCompletionToolArgs::default()
            .function(func)
            .build()
            .unwrap()
    }

    #[test]
    fn strip_provider_prefix_cases() {
        assert_eq!(
            strip_provider_prefix("anthropic/claude-sonnet-4-5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(strip_provider_prefix("claude-haiku-4"), "claude-haiku-4");
        assert_eq!(strip_provider_prefix(""), "");
    }

    #[test]
    fn build_anthropic_request_places_system_top_level() {
        let messages = vec![sys("You are helpful."), user("Hi there")];
        let body = build_anthropic_request(
            "anthropic/claude-sonnet-4-5",
            &messages,
            &[],
            0.2,
            1024,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["system"], json!("You are helpful."));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "system should be lifted out of messages");
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn build_anthropic_request_strips_provider_prefix_from_model() {
        let body = build_anthropic_request(
            "anthropic/claude-sonnet-4-5",
            &[user("hi")],
            &[],
            0.0,
            100,
            None,
            false,
        )
        .unwrap();
        assert_eq!(body["model"], json!("claude-sonnet-4-5"));
    }

    #[test]
    fn build_anthropic_request_converts_tool_parameters_to_input_schema() {
        let params = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "required": ["x"]
        });
        let tool = make_tool("do_thing", params.clone());
        let body = build_anthropic_request(
            "claude-sonnet-4-5",
            &[sys("s"), user("u")],
            &[tool],
            0.0,
            100,
            None,
            false,
        )
        .unwrap();
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("do_thing"));
        assert_eq!(tools[0]["input_schema"], params);
        assert!(
            tools[0].get("parameters").is_none(),
            "native request must not carry OpenAI 'parameters' key"
        );
    }

    #[test]
    fn build_anthropic_request_caching_marks_system_and_last_tool() {
        let tool = make_tool("t", json!({"type":"object","properties":{}}));
        let body = build_anthropic_request(
            "claude-sonnet-4-5",
            &[sys("s"), user("u")],
            &[tool],
            0.0,
            100,
            None,
            true, // enable_caching
        )
        .unwrap();
        // System should now be a content-block array with cache_control.
        let sys_arr = body["system"].as_array().expect("system as array");
        assert_eq!(sys_arr[0]["cache_control"]["type"], "ephemeral");
        // Last tool should carry cache_control.
        let last_tool = body["tools"].as_array().unwrap().last().unwrap();
        assert_eq!(last_tool["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn build_anthropic_request_converts_tool_result_messages() {
        let messages = vec![
            sys("s"),
            user("please search"),
            assistant_tool_call("toolu_1", "search", r#"{"q":"rust"}"#),
            tool_msg("toolu_1", "found 42 results"),
        ];
        let body =
            build_anthropic_request("claude-sonnet-4-5", &messages, &[], 0.0, 100, None, false)
                .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        // Expected: [user("please search"), assistant(tool_use), user(tool_result)]
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        let assistant_blocks = msgs[1]["content"].as_array().unwrap();
        assert!(
            assistant_blocks
                .iter()
                .any(|b| b["type"] == "tool_use" && b["id"] == "toolu_1"),
            "missing tool_use block: {assistant_blocks:?}"
        );
        assert_eq!(msgs[2]["role"], "user");
        let result_blocks = msgs[2]["content"].as_array().unwrap();
        assert_eq!(result_blocks[0]["type"], "tool_result");
        assert_eq!(result_blocks[0]["tool_use_id"], "toolu_1");
        assert_eq!(result_blocks[0]["content"], "found 42 results");
    }

    #[test]
    fn build_anthropic_request_passes_tool_choice_through() {
        let body = build_anthropic_request(
            "claude-sonnet-4-5",
            &[user("hi")],
            &[],
            0.0,
            100,
            Some(&json!({"type": "any"})),
            false,
        )
        .unwrap();
        assert_eq!(body["tool_choice"], json!({"type": "any"}));
    }

    #[test]
    fn parse_anthropic_response_extracts_text_and_tool_use() {
        let resp = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Working on it."},
                {"type": "tool_use", "id": "toolu_abc", "name": "finish_task",
                 "input": {"summary": "done"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let parsed = parse_anthropic_response(&resp);
        assert_eq!(parsed.text_content.as_deref(), Some("Working on it."));
        assert_eq!(parsed.stop_reason, "tool_use");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "toolu_abc");
        assert_eq!(parsed.tool_calls[0].function.name, "finish_task");
        let args: Value = serde_json::from_str(&parsed.tool_calls[0].function.arguments).unwrap();
        assert_eq!(args["summary"], json!("done"));
        assert_eq!(parsed.usage.prompt_tokens, 100);
        assert_eq!(parsed.usage.completion_tokens, 50);
    }

    #[test]
    fn parse_anthropic_response_reads_cache_read_tokens() {
        let resp = json!({
            "content": [{"type":"text","text":"hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 80,
                "cache_creation_input_tokens": 3
            }
        });
        let parsed = parse_anthropic_response(&resp);
        assert_eq!(parsed.usage.cache_read_tokens, 80);
        assert_eq!(parsed.usage.cache_creation_tokens, 3);
    }

    #[test]
    fn parse_anthropic_response_empty_content_yields_none() {
        let resp = json!({"content": [], "stop_reason": "end_turn"});
        let parsed = parse_anthropic_response(&resp);
        assert!(parsed.text_content.is_none());
        assert!(parsed.tool_calls.is_empty());
        assert_eq!(parsed.stop_reason, "end_turn");
    }

    #[test]
    fn parse_anthropic_response_defaults_stop_reason() {
        let resp = json!({"content": [{"type":"text","text":"x"}]});
        let parsed = parse_anthropic_response(&resp);
        assert_eq!(parsed.stop_reason, "end_turn");
    }

    // --- #29: Conversation-history cache breakpoint tests ---

    #[test]
    fn history_token_estimate_counts_text_blocks() {
        // ~ chars/4 token estimate; 8000 chars -> ~2000 tokens.
        let msgs = vec![
            json!({"role":"user","content":"hello"}),
            json!({"role":"assistant","content":[{"type":"text","text":"a".repeat(8000)}]}),
        ];
        let est = history_token_estimate(&msgs);
        assert!(est >= 2000, "expected >=2000, got {est}");
    }

    #[test]
    fn attach_cache_control_to_last_assistant_inserts_marker() {
        let mut msgs = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":"first reply"}),
            json!({"role":"user","content":"thanks"}),
            json!({"role":"assistant","content":[{"type":"text","text":"second reply"}]}),
        ];
        attach_cache_control_to_last_assistant(&mut msgs);
        // Last assistant message is index 3; its content[0] should now carry cache_control.
        let blocks = msgs[3]["content"].as_array().expect("array");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        // Earlier assistant message (index 1) must NOT have been modified.
        // (still a String content)
        assert!(msgs[1]["content"].is_string());
    }

    #[test]
    fn build_anthropic_request_caches_last_assistant_when_large() {
        // #29: A long assistant turn (>2000 tokens, ~8000 chars) should trigger
        // a second cache breakpoint on the most recent assistant block.
        let big = "x".repeat(10_000);
        let assistant: ChatCompletionRequestMessage =
            ChatCompletionRequestAssistantMessageArgs::default()
                .content(big.as_str())
                .build()
                .unwrap()
                .into();
        let msgs = vec![sys("you are helpful"), user("hi"), assistant];
        let body = build_anthropic_request(
            "anthropic/claude-sonnet-4-6",
            &msgs,
            &[],
            0.2,
            1024,
            None,
            true,
        )
        .unwrap();
        let messages = body["messages"].as_array().expect("messages");
        // The last message is the assistant turn — its last block should have cache_control.
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "assistant");
        let content = last["content"].as_array().expect("array content");
        let last_block = content.last().unwrap();
        assert_eq!(
            last_block["cache_control"]["type"], "ephemeral",
            "expected cache_control on last assistant block: {body:#}"
        );
    }

    #[test]
    fn build_anthropic_request_skips_history_cache_when_small() {
        // #29: Below the 2000-token threshold we must NOT emit a second
        // breakpoint — only the system prompt gets cached.
        let assistant: ChatCompletionRequestMessage =
            ChatCompletionRequestAssistantMessageArgs::default()
                .content("short reply")
                .build()
                .unwrap()
                .into();
        let msgs = vec![sys("sys"), user("hi"), assistant];
        let body = build_anthropic_request(
            "anthropic/claude-sonnet-4-6",
            &msgs,
            &[],
            0.2,
            1024,
            None,
            true,
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        // Either still a string content, or an array without cache_control.
        match &last["content"] {
            Value::String(_) => {}
            Value::Array(a) => {
                for b in a {
                    assert!(
                        b.get("cache_control").is_none(),
                        "small history must not emit cache_control: {b}"
                    );
                }
            }
            _ => {}
        }
    }

    #[test]
    fn build_anthropic_request_skips_history_cache_when_caching_disabled() {
        // #29: Even with a long history, enable_caching=false must not emit
        // any cache_control markers — caching is opt-in via the agent config.
        let big = "x".repeat(10_000);
        let assistant: ChatCompletionRequestMessage =
            ChatCompletionRequestAssistantMessageArgs::default()
                .content(big.as_str())
                .build()
                .unwrap()
                .into();
        let msgs = vec![sys("sys"), user("hi"), assistant];
        let body = build_anthropic_request(
            "anthropic/claude-sonnet-4-6",
            &msgs,
            &[],
            0.2,
            1024,
            None,
            false, // caching disabled
        )
        .unwrap();
        // System should be a plain string (no cache_control wrapping).
        assert!(body["system"].is_string());
        // Last assistant message must not have cache_control either.
        let messages = body["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        if let Some(arr) = last["content"].as_array() {
            for b in arr {
                assert!(b.get("cache_control").is_none());
            }
        }
    }
}
