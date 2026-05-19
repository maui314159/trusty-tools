//! AWS Bedrock Converse API client.
//!
//! Why: Bedrock uses AWS SigV4 auth and a native Converse API format, not
//! the OpenAI-compatible format used by OpenRouter/Anthropic direct. This
//! module wraps `aws-sdk-bedrockruntime` so the agent harness can route any
//! `bedrock/<model_id>` agent through AWS instead of OpenRouter, while
//! preserving the `(content, tool_calls, usage)` shape the chat loop expects.
//! What: Builds an authenticated `bedrockruntime::Client` from an optional
//! AWS profile + region, runs single-turn `chat()` and multi-turn
//! `chat_with_tools()` against the Converse API, and translates OpenAI-format
//! tool definitions into Bedrock `ToolConfiguration`.
//! Test: `serde_json_to_document_roundtrip`, `build_tool_config_translates_openai_schema`,
//! `extract_text_handles_mixed_blocks`. End-to-end smoke test
//! `bedrock_smoke_test` (gated `#[ignore]`) requires real AWS credentials.

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_bedrockruntime::Client as BedrockClient;
use aws_sdk_bedrockruntime::operation::converse::ConverseOutput;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, InferenceConfiguration, Message, SystemContentBlock, Tool,
    ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock, ToolSpecification,
    ToolUseBlock,
};
use aws_smithy_types::{Document, Number};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::perf::TokenUsage;
use crate::tools::{ToolRegistry, ToolResult};

/// Build an AWS Bedrock client using the standard credential chain.
///
/// Why: The AWS SDK's default chain handles env vars, `~/.aws/credentials`
/// profiles, instance metadata, and SSO; centralizing client creation here
/// keeps that complexity out of the agent runner and lets us layer
/// per-agent profile/region overrides on top.
/// What: If `profile` is set, selects that named profile; if `region` is
/// set, uses it; otherwise defaults to `us-east-1` (where Bedrock is
/// universally available).
/// Test: Indirectly via `bedrock_smoke_test` — unit-testing the AWS config
/// loader requires live credentials.
pub async fn build_client(profile: Option<&str>, region: Option<&str>) -> Result<BedrockClient> {
    let region_str = region.unwrap_or("us-east-1").to_string();
    let region_provider = aws_config::meta::region::RegionProviderChain::first_try(
        aws_types::region::Region::new(region_str),
    );

    let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(region_provider);

    if let Some(p) = profile {
        loader = loader.profile_name(p);
    }

    let config = loader.load().await;
    Ok(BedrockClient::new(&config))
}

/// Convert a `serde_json::Value` to an `aws_smithy_types::Document`.
///
/// Why: Bedrock tool input schemas and tool-call inputs use Smithy's
/// protocol-agnostic `Document` type; our tool registry speaks JSON. A
/// faithful conversion preserves nested objects/arrays without lossy string
/// coercion.
/// What: Recursive walk; numbers map to `Number::PosInt`/`NegInt`/`Float`
/// based on the JSON number shape.
/// Test: `serde_json_to_document_roundtrip`.
fn json_to_document(v: &Value) -> Document {
    match v {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(Number::NegInt(i))
            } else if let Some(f) = n.as_f64() {
                Document::Number(Number::Float(f))
            } else {
                Document::Null
            }
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(a) => Document::Array(a.iter().map(json_to_document).collect()),
        Value::Object(o) => {
            let map: HashMap<String, Document> = o
                .iter()
                .map(|(k, v)| (k.clone(), json_to_document(v)))
                .collect();
            Document::Object(map)
        }
    }
}

/// Convert an `aws_smithy_types::Document` back to a `serde_json::Value`.
///
/// Why: The model returns tool-call inputs as `Document`; the agent harness
/// dispatches tools using `serde_json::Value`.
/// What: Inverse of `json_to_document`.
/// Test: `serde_json_to_document_roundtrip`.
fn document_to_json(d: &Document) -> Value {
    match d {
        Document::Null => Value::Null,
        Document::Bool(b) => Value::Bool(*b),
        Document::Number(n) => match n {
            Number::PosInt(u) => Value::Number((*u).into()),
            Number::NegInt(i) => Value::Number((*i).into()),
            Number::Float(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        },
        Document::String(s) => Value::String(s.clone()),
        Document::Array(a) => Value::Array(a.iter().map(document_to_json).collect()),
        Document::Object(o) => {
            let mut map = serde_json::Map::new();
            for (k, v) in o {
                map.insert(k.clone(), document_to_json(v));
            }
            Value::Object(map)
        }
    }
}

/// Build a Bedrock `ToolConfiguration` from OpenAI-format tool definitions.
///
/// Why: Our `ToolRegistry` exposes tools in OpenAI function-calling JSON
/// shape — `{"type":"function","function":{"name","description","parameters"}}`.
/// Bedrock wants `Vec<Tool::ToolSpec(ToolSpecification)>` with the schema as
/// a Smithy `Document`. Translating once here keeps every agent's tools
/// usable on Bedrock without per-tool changes.
/// What: For each tool, extracts `function.name`, `function.description`,
/// and `function.parameters` (defaulting to `{"type":"object"}` when absent),
/// converts the parameters JSON to a `Document`, and wraps everything in a
/// `ToolSpecification`.
/// Test: `build_tool_config_translates_openai_schema`.
fn build_tool_config(tools: &[Value]) -> Result<Option<ToolConfiguration>> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut tool_specs: Vec<Tool> = Vec::with_capacity(tools.len());
    for tool_def in tools {
        // Support both wrapped {function:{...}} and flat {name,description,parameters}.
        let func = tool_def.get("function").unwrap_or(tool_def);
        let name = func
            .get("name")
            .and_then(|v| v.as_str())
            .context("tool definition missing function.name")?
            .to_string();
        let description = func
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let params = func
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type":"object","properties":{}}));
        let schema_doc = json_to_document(&params);
        let spec = ToolSpecification::builder()
            .name(name)
            .description(description)
            .input_schema(ToolInputSchema::Json(schema_doc))
            .build()
            .context("failed to build Bedrock ToolSpecification")?;
        tool_specs.push(Tool::ToolSpec(spec));
    }
    let cfg = ToolConfiguration::builder()
        .set_tools(Some(tool_specs))
        .build()
        .context("failed to build Bedrock ToolConfiguration")?;
    Ok(Some(cfg))
}

/// Pull all `Text` blocks out of a Converse response and join with newlines.
fn extract_text_from_output(resp: &ConverseOutput) -> Option<String> {
    let msg = resp.output().and_then(|o| o.as_message().ok())?;
    let mut out = String::new();
    for block in msg.content() {
        if let ContentBlock::Text(t) = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Tool-use blocks exposed to the chat loop as a normalized record.
#[derive(Debug, Clone)]
pub struct BedrockToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Pull `ToolUse` blocks out of a Converse response.
fn extract_tool_uses(resp: &ConverseOutput) -> Vec<BedrockToolUse> {
    let Some(msg) = resp.output().and_then(|o| o.as_message().ok()) else {
        return vec![];
    };
    let mut out = Vec::new();
    for block in msg.content() {
        if let ContentBlock::ToolUse(tu) = block {
            out.push(BedrockToolUse {
                id: tu.tool_use_id().to_string(),
                name: tu.name().to_string(),
                input: document_to_json(tu.input()),
            });
        }
    }
    out
}

/// Pull a `TokenUsage` out of a Converse response.
fn parse_usage(resp: &ConverseOutput) -> TokenUsage {
    if let Some(u) = resp.usage() {
        let p = u.input_tokens().max(0) as u32;
        let c = u.output_tokens().max(0) as u32;
        return TokenUsage::new(p, c, 0, 0);
    }
    TokenUsage::default()
}

/// Single-turn chat via the Bedrock Converse API.
///
/// Why: Mirrors `crate::llm::chat` for the OpenAI-compatible path so callers
/// that don't need tool calling can hit Bedrock with the same shape.
/// What: Sends a single user message with a system prompt, returns the
/// assistant's joined text and token usage.
/// Test: `bedrock_smoke_test` (#[ignore], requires AWS credentials).
/// Single-turn chat via Bedrock Converse with optional tool definitions, returning
/// any `ToolUse` blocks WITHOUT executing them.
///
/// Why: Some callers (the CTRL coordinator, the PM REPL one-shot path) want to
/// inspect and dispatch tool calls themselves so they can emit per-tool UI
/// events between the LLM call and the tool execution. The multi-turn
/// `chat_with_tools` loop hides that boundary; this helper exposes it.
/// What: Builds a `Converse` request with optional `tool_config`, returns the
/// assistant's text content (if any), the list of tool_use blocks, and the
/// usage tally. Callers are responsible for any follow-up turn.
/// Test: Indirectly via callers in `llm::chat_oneshot_adapter_aware`; full
/// path covered by manual `bedrock_smoke_test`.
pub async fn chat_oneshot(
    client: &BedrockClient,
    model_id: &str,
    system_prompt: &str,
    user_message: &str,
    temperature: f32,
    max_tokens: u32,
    tools: Vec<Value>,
) -> Result<(Option<String>, Vec<BedrockToolUse>, TokenUsage)> {
    let system = vec![SystemContentBlock::Text(system_prompt.to_string())];
    let user = Message::builder()
        .role(ConversationRole::User)
        .content(ContentBlock::Text(user_message.to_string()))
        .build()
        .context("failed to build Bedrock user message")?;
    let inference = InferenceConfiguration::builder()
        .temperature(temperature)
        .max_tokens(max_tokens as i32)
        .build();
    let tool_config = build_tool_config(&tools)?;

    let mut req = client
        .converse()
        .model_id(model_id)
        .set_system(Some(system))
        .messages(user)
        .inference_config(inference);
    if let Some(cfg) = tool_config {
        req = req.tool_config(cfg);
    }

    let resp = req
        .send()
        .await
        .context("Bedrock Converse one-shot request failed")?;

    let usage = parse_usage(&resp);
    let text = extract_text_from_output(&resp);
    let tool_uses = extract_tool_uses(&resp);
    Ok((text, tool_uses, usage))
}

pub async fn chat(
    client: &BedrockClient,
    model_id: &str,
    system_prompt: &str,
    user_message: &str,
    temperature: f32,
    max_tokens: u32,
) -> Result<(Option<String>, TokenUsage)> {
    let system = vec![SystemContentBlock::Text(system_prompt.to_string())];
    let user = Message::builder()
        .role(ConversationRole::User)
        .content(ContentBlock::Text(user_message.to_string()))
        .build()
        .context("failed to build Bedrock user message")?;
    let inference = InferenceConfiguration::builder()
        .temperature(temperature)
        .max_tokens(max_tokens as i32)
        .build();

    let resp = client
        .converse()
        .model_id(model_id)
        .set_system(Some(system))
        .messages(user)
        .inference_config(inference)
        .send()
        .await
        .context("Bedrock Converse single-turn request failed")?;

    let usage = parse_usage(&resp);
    let text = extract_text_from_output(&resp);
    Ok((text, usage))
}

/// Multi-turn chat with tool calling via the Bedrock Converse API.
///
/// Why: Bedrock's tool-use loop is structurally identical to Anthropic's
/// (assistant emits ToolUse blocks → caller dispatches each tool →
/// reply includes one or more ToolResult blocks under role=user). This
/// function owns that loop end-to-end so the rest of the harness only
/// needs to know about the final `(content, usage)` tuple.
/// What: Builds a `ToolConfiguration` from the OpenAI-format tools,
/// repeatedly calls `Converse` and dispatches any tool-use blocks via
/// `registry.dispatch_gated`, exits when the model returns plain text or
/// `max_turns` is exhausted.
/// Test: Indirectly via `bedrock_smoke_test` and the agent integration
/// tests that exercise a Bedrock agent end-to-end.
#[allow(clippy::too_many_arguments)]
pub async fn chat_with_tools(
    client: &BedrockClient,
    model_id: &str,
    system_prompt: &str,
    user_message: &str,
    temperature: f32,
    max_tokens: u32,
    tools: Vec<Value>,
    registry: Arc<ToolRegistry>,
    allowed_tools: Option<Vec<String>>,
    max_turns: u32,
    stop_sequences: &[String],
) -> Result<(String, TokenUsage)> {
    let system = vec![SystemContentBlock::Text(system_prompt.to_string())];
    let mut messages: Vec<Message> = vec![
        Message::builder()
            .role(ConversationRole::User)
            .content(ContentBlock::Text(user_message.to_string()))
            .build()
            .context("failed to build initial user message")?,
    ];
    // #297: Forward agent-configured stop sequences via Bedrock's
    // InferenceConfiguration.stop_sequences. The Bedrock Converse API supports
    // up to 4 stop sequences depending on the model.
    let mut inference_builder = InferenceConfiguration::builder()
        .temperature(temperature)
        .max_tokens(max_tokens as i32);
    for seq in stop_sequences {
        inference_builder = inference_builder.stop_sequences(seq.clone());
    }
    let inference = inference_builder.build();
    let tool_config = build_tool_config(&tools)?;

    let mut total_usage = TokenUsage::default();

    for turn in 0..max_turns {
        let mut req = client
            .converse()
            .model_id(model_id)
            .set_system(Some(system.clone()))
            .set_messages(Some(messages.clone()))
            .inference_config(inference.clone());
        if let Some(cfg) = tool_config.clone() {
            req = req.tool_config(cfg);
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("Bedrock Converse turn {turn} failed"))?;
        total_usage.add(&parse_usage(&resp));

        let tool_uses = extract_tool_uses(&resp);
        let text = extract_text_from_output(&resp);

        if tool_uses.is_empty() {
            // Plain-text turn — terminate with whatever the model produced.
            return Ok((text.unwrap_or_default(), total_usage));
        }

        // Append assistant message (text + tool_use blocks) for the next
        // turn's context.
        let assistant_blocks: Vec<ContentBlock> = {
            let mut blocks: Vec<ContentBlock> = Vec::new();
            if let Some(t) = &text
                && !t.is_empty()
            {
                blocks.push(ContentBlock::Text(t.clone()));
            }
            for tu in &tool_uses {
                let tub = ToolUseBlock::builder()
                    .tool_use_id(tu.id.clone())
                    .name(tu.name.clone())
                    .input(json_to_document(&tu.input))
                    .build()
                    .context("failed to build ToolUseBlock for assistant turn")?;
                blocks.push(ContentBlock::ToolUse(tub));
            }
            blocks
        };
        let assistant_msg = Message::builder()
            .role(ConversationRole::Assistant)
            .set_content(Some(assistant_blocks))
            .build()
            .context("failed to build assistant message")?;
        messages.push(assistant_msg);

        // Dispatch each tool call and collect ToolResult blocks under a
        // single user-role message (Bedrock convention).
        let mut result_blocks: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        for tu in &tool_uses {
            let result = registry
                .dispatch_gated(&tu.name, tu.input.clone(), allowed_tools.as_deref())
                .await;
            let is_err = result.is_error();
            let content_str = match &result {
                ToolResult::Success(s) => s.clone(),
                ToolResult::Error { message, .. } => format!("ERROR: {message}"),
            };
            let mut builder = ToolResultBlock::builder()
                .tool_use_id(tu.id.clone())
                .content(ToolResultContentBlock::Text(content_str));
            if is_err {
                builder = builder.status(aws_sdk_bedrockruntime::types::ToolResultStatus::Error);
            }
            let trb = builder.build().context("failed to build ToolResultBlock")?;
            result_blocks.push(ContentBlock::ToolResult(trb));
        }
        let user_msg = Message::builder()
            .role(ConversationRole::User)
            .set_content(Some(result_blocks))
            .build()
            .context("failed to build tool-result user message")?;
        messages.push(user_msg);
    }

    anyhow::bail!(
        "Bedrock chat_with_tools exceeded max_turns ({max_turns}) without a final text response"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serde_json_to_document_roundtrip() {
        let original = json!({
            "name": "alice",
            "age": 30,
            "balance": -1.5,
            "active": true,
            "tags": ["a", "b"],
            "meta": null,
        });
        let doc = json_to_document(&original);
        let back = document_to_json(&doc);
        assert_eq!(back["name"], "alice");
        assert_eq!(back["age"], 30);
        assert_eq!(back["active"], true);
        assert_eq!(back["tags"][1], "b");
        assert!(back["meta"].is_null());
    }

    #[test]
    fn build_tool_config_translates_openai_schema() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "search",
                "description": "search the web",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"}
                    },
                    "required": ["query"]
                }
            }
        })];
        let cfg = build_tool_config(&tools).unwrap().unwrap();
        let specs = cfg.tools();
        assert_eq!(specs.len(), 1);
        if let Tool::ToolSpec(spec) = &specs[0] {
            assert_eq!(spec.name(), "search");
            assert_eq!(spec.description(), Some("search the web"));
        } else {
            panic!("expected ToolSpec variant");
        }
    }

    #[test]
    fn build_tool_config_empty_returns_none() {
        let cfg = build_tool_config(&[]).unwrap();
        assert!(cfg.is_none());
    }

    #[test]
    fn build_tool_config_supports_flat_schema() {
        // Tools provided without the {function:{...}} wrapper should still parse.
        let tools = vec![json!({
            "name": "echo",
            "description": "echoes input",
            "parameters": {"type": "object"}
        })];
        let cfg = build_tool_config(&tools).unwrap().unwrap();
        assert_eq!(cfg.tools().len(), 1);
    }

    /// Smoke test against a real AWS Bedrock endpoint. Gated with #[ignore]
    /// because it requires credentials. Run manually with:
    ///   cargo test bedrock_smoke_test -- --ignored
    /// using the `cto` profile (us-east-1) per project conventions.
    #[tokio::test]
    #[ignore = "requires AWS credentials (profile=cto, region=us-east-1)"]
    async fn bedrock_smoke_test() {
        let client = build_client(Some("cto"), Some("us-east-1"))
            .await
            .expect("build_client failed");
        // Use the cross-region inference profile id ("us." prefix) — the
        // raw foundation model id rejects on-demand throughput in us-east-1.
        let (text, usage) = chat(
            &client,
            "us.anthropic.claude-3-5-haiku-20241022-v1:0",
            "You are a concise assistant. Reply in plain text.",
            "Say hello in exactly 3 words.",
            0.0,
            64,
        )
        .await
        .expect("Bedrock chat failed");
        let body = text.expect("expected non-empty response text");
        assert!(!body.is_empty(), "empty response body");
        assert!(
            usage.prompt_tokens > 0 || usage.completion_tokens > 0,
            "expected non-zero usage: {usage:?}"
        );
        eprintln!("bedrock_smoke_test response: {body:?} usage={usage:?}");
    }
}
