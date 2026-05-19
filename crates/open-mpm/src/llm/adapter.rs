//! Model-provider adapter.
//!
//! Why: Different LLM providers reached via OpenRouter (Anthropic, OpenAI,
//! Google, ...) disagree on the exact wire shape of several fields that the
//! agent harness cares about:
//!   - `tool_choice`: Anthropic expects `{"type":"any"}` to force a tool call
//!     while OpenAI expects the string `"required"`.
//!   - `cache_control`: only Anthropic understands the
//!     `{"type":"ephemeral"}` prompt-caching field.
//!   - `usage`: Anthropic emits `input_tokens`/`output_tokens` plus
//!     `cache_read_input_tokens`/`cache_creation_input_tokens`; OpenAI emits
//!     `prompt_tokens`/`completion_tokens` with no cache fields.
//! Centralizing these differences behind a single `ModelAdapter` trait keeps
//! scattered `if model_is_anthropic(...)` branches out of the chat loop (#57).
//! What: Defines the `ModelAdapter` trait with three concrete impls —
//! `AnthropicAdapter`, `OpenAiAdapter`, `GenericAdapter` — and a factory
//! `adapter_for_model(model_str)` that picks one based on the model string.
//! Test: `adapter_for_model_routes_anthropic`, `adapter_for_model_routes_openai`,
//! `anthropic_tool_choice_any_shape`, `openai_tool_choice_any_shape`,
//! `anthropic_inject_cache_control_on_string_system`.

use serde_json::{Value, json};

use crate::perf::TokenUsage;

/// Which auth mechanism was used to resolve an `ApiEndpoint`.
///
/// Why: Startup logging should tell the operator exactly which credential
/// is active for each agent so auth issues are diagnosed without reading
/// env vars manually.
/// What: Three variants — OAuth token (reserved for ClaudeCodeAgentRunner only),
/// pay-as-you-go Anthropic API key, and the OpenRouter fallback.
/// Note: `ClaudeMaxOAuth` is NOT used by `AnthropicAdapter::api_endpoint()`.
/// `CLAUDE_CODE_OAUTH_TOKEN` (sk-ant-oat01-*) tokens are rejected by
/// api.anthropic.com with 401. They are only valid for runner="claude-code"
/// agents via ClaudeCodeAgentRunner. For direct API mode, use ANTHROPIC_API_KEY
/// from console.anthropic.com.
/// Test: `oauth_token_is_not_used_for_direct_api_routing`,
/// `api_key_used_when_no_oauth_token`,
/// `falls_back_to_openrouter_when_use_direct_false`.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthSource {
    /// OAuth Bearer token — reserved for ClaudeCodeAgentRunner subprocess path only.
    /// NOT used by `AnthropicAdapter::api_endpoint()` — api.anthropic.com rejects
    /// sk-ant-oat01-* tokens with 401.
    #[allow(dead_code)]
    ClaudeMaxOAuth,
    /// `ANTHROPIC_API_KEY` — pay-as-you-go direct API access via x-api-key header.
    AnthropicApiKey,
    /// OpenRouter (`OPENROUTER_API_KEY`) — the default fallback routing path.
    OpenRouter,
    /// AWS SigV4 — credentials resolved via the AWS SDK default chain
    /// (env vars, `~/.aws/credentials`, instance metadata, etc.). Used
    /// exclusively by `BedrockAdapter`.
    Bedrock,
}

impl std::fmt::Display for AuthSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaudeMaxOAuth => write!(f, "claude-max-oauth"),
            Self::AnthropicApiKey => write!(f, "anthropic-api-key"),
            Self::OpenRouter => write!(f, "openrouter"),
            Self::Bedrock => write!(f, "aws-bedrock"),
        }
    }
}

/// Endpoint routing information for a chat completion request.
///
/// Why: Different providers (and different auth modes within the same provider)
/// require different base URLs, auth headers, and extra headers (e.g.
/// `anthropic-version`). Centralizing this in a struct returned from
/// `ModelAdapter::api_endpoint` keeps the routing logic out of the chat loop.
/// What: Base URL, auth header name + value, extra headers, and the
/// `AuthSource` tag used for startup logging.
/// Test: `anthropic_api_endpoint_direct_when_key_set`,
/// `anthropic_api_endpoint_falls_back_to_openrouter`.
#[derive(Debug, Clone)]
pub struct ApiEndpoint {
    /// Base URL without trailing slash (e.g. `https://api.anthropic.com/v1`).
    pub base_url: String,
    /// Header name for auth — `Authorization` (Bearer) or `x-api-key`.
    pub auth_header_name: String,
    /// Header value (already includes `Bearer ` prefix when needed).
    pub auth_header_value: String,
    /// Additional headers required by the provider (e.g. `anthropic-version`).
    pub extra_headers: Vec<(String, String)>,
    /// Which credential resolved this endpoint — used for startup logging.
    pub auth_source: AuthSource,
}

/// Which provider family a `ModelAdapter` represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Provider {
    Anthropic,
    OpenAI,
    Generic,
    /// AWS Bedrock Converse API — uses the `aws-sdk-bedrockruntime` SDK
    /// instead of an OpenAI-compatible HTTP request. Activated when the
    /// model string starts with `bedrock/`.
    Bedrock,
}

/// Centralizes all model-provider-specific behavior for the chat loop.
///
/// Why: One implementation per provider family, selected at agent load time
/// from the model string. See module docs for the concrete differences
/// being abstracted.
/// What: Five methods — provider tag, the two `tool_choice` shapes
/// (forced / auto), cache-control injection on a raw request body, and
/// usage parsing from a raw response body.
/// Test: See per-adapter unit tests at the bottom of this module.
pub trait ModelAdapter: Send + Sync + std::fmt::Debug {
    /// Identity tag for the adapter (used mostly in logs/tests).
    fn provider(&self) -> Provider;

    /// The `tool_choice` value that forces the model to call SOME tool.
    /// `None` means omit the field — some providers do not support it.
    fn tool_choice_any(&self) -> Option<Value>;

    /// The `tool_choice` value for normal ("auto") mode.
    fn tool_choice_auto(&self) -> Option<Value>;

    /// Inject Anthropic `cache_control` onto the system message of a raw
    /// request body. No-op for non-Anthropic providers.
    fn inject_cache_control(&self, request_body: &mut Value, enabled: bool);

    /// Extract token usage (including provider-specific cache fields) from
    /// a raw response JSON body.
    fn parse_usage(&self, response: &Value) -> TokenUsage;

    /// Whether this provider supports extended thinking / reasoning tokens.
    #[allow(dead_code)]
    fn supports_thinking(&self) -> bool {
        false
    }

    /// Returns the API endpoint (base URL + auth) for this provider.
    ///
    /// Why: #59 — when `use_direct=true` and the relevant direct-API key
    /// (e.g. `ANTHROPIC_API_KEY`) is set, we bypass OpenRouter and call the
    /// provider's native endpoint for lower latency/cost. For every other
    /// case (including unknown providers) this returns the OpenRouter
    /// endpoint.
    /// What: Returns `ApiEndpoint` describing `{base_url, auth headers,
    /// extra headers}`. Default impl returns OpenRouter; Anthropic overrides
    /// to honor `use_direct`.
    /// Test: `anthropic_api_endpoint_direct_when_key_set`,
    /// `anthropic_api_endpoint_falls_back_to_openrouter`,
    /// `openai_api_endpoint_always_openrouter`.
    fn api_endpoint(&self, _use_direct: bool) -> ApiEndpoint {
        openrouter_endpoint()
    }

    /// Whether this adapter's native API wire format differs from the
    /// OpenAI-compatible shape (OpenRouter's default).
    ///
    /// Why: #59 — when we route to `api.anthropic.com` directly we must emit
    /// Anthropic's native `/v1/messages` request shape (top-level `system`,
    /// `input_schema` for tools, content-block `tool_result`s, etc.) instead
    /// of OpenAI's `/v1/chat/completions`. Generic/OpenAI adapters never use
    /// a native format.
    /// What: Returns `true` only for Anthropic.
    /// Test: `anthropic_uses_native_format_true`, `openai_uses_native_format_false`.
    fn uses_native_format(&self) -> bool {
        false
    }
}

/// The default OpenRouter endpoint used by every non-direct path.
///
/// Why: Keeps the single source of truth for base URL + env-var name in one
/// place so tests and adapters agree on the fallback shape.
/// What: Reads `OPENROUTER_BASE_URL` (override for tests) and
/// `OPENROUTER_API_KEY`; builds `Authorization: Bearer <key>`.
/// Test: Indirectly via `anthropic_api_endpoint_falls_back_to_openrouter`.
pub fn openrouter_endpoint() -> ApiEndpoint {
    let base_url = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    ApiEndpoint {
        base_url,
        auth_header_name: "Authorization".to_string(),
        auth_header_value: format!("Bearer {key}"),
        extra_headers: vec![],
        auth_source: AuthSource::OpenRouter,
    }
}

/// Pick an adapter for the given model string.
///
/// Why: Deciding provider family from the model name (OpenRouter-style
/// `vendor/model` or loose `claude-...`/`gpt-...`) keeps callers from
/// knowing the concrete adapter types.
/// What: Heuristic substring match — Anthropic first (most common), then
/// OpenAI / o-series, else `GenericAdapter` as a safe default.
/// Test: `adapter_for_model_routes_anthropic`, `adapter_for_model_routes_openai`.
pub fn adapter_for_model(model: &str) -> Box<dyn ModelAdapter> {
    // Bedrock prefix takes precedence — the model id after `bedrock/` may
    // contain `anthropic` (e.g. `bedrock/anthropic.claude-3-5-haiku-...`),
    // and we must route those to the Bedrock SDK, not the Anthropic adapter.
    if model.starts_with("bedrock/") {
        let id = model.strip_prefix("bedrock/").unwrap_or(model).to_string();
        return Box::new(BedrockAdapter { model_id: id });
    }
    // Ollama prefix routes to a locally-running ollama server. The adapter
    // is OpenAI-compatible (ollama exposes `/v1/chat/completions` natively),
    // so the wire format matches GenericAdapter; only the base URL differs.
    if model.starts_with("ollama/") {
        let id = model.strip_prefix("ollama/").unwrap_or(model).to_string();
        return Box::new(OllamaAdapter { model_id: id });
    }
    let m = model.to_ascii_lowercase();
    if m.contains("claude") || m.contains("anthropic") {
        Box::new(AnthropicAdapter)
    } else if m.contains("gpt") || m.contains("openai") || m.contains("o1") || m.contains("o3") {
        Box::new(OpenAiAdapter)
    } else {
        Box::new(GenericAdapter)
    }
}

/// Anthropic-family adapter (Claude models).
#[derive(Debug)]
pub struct AnthropicAdapter;

/// OpenAI-family adapter (GPT, o1, o3, ...).
#[derive(Debug)]
pub struct OpenAiAdapter;

/// Provider-agnostic fallback adapter.
#[derive(Debug)]
pub struct GenericAdapter;

/// Local ollama adapter (OpenAI-compatible endpoint).
///
/// Why: `/provider local` lets the user route LLM calls to a locally-running
/// ollama instance with no auth. ollama exposes an OpenAI-compatible
/// `/v1/chat/completions` endpoint, so the wire format matches GenericAdapter;
/// only the base URL changes.
/// What: Activated by `adapter_for_model("ollama/<name>")`. The model id after
/// the prefix is forwarded as `model` in the request body. `OLLAMA_HOST` env
/// var overrides the default `http://localhost:11434`.
/// Test: `adapter_for_model_routes_ollama`, `ollama_api_endpoint_uses_host`.
#[derive(Debug)]
pub struct OllamaAdapter {
    pub model_id: String,
}

/// AWS Bedrock Converse API adapter.
///
/// Why: Bedrock is not OpenAI-compatible — it speaks SigV4 auth and a native
/// `Converse` request/response shape. We carry the `model_id` (everything
/// after the `bedrock/` prefix, e.g. `anthropic.claude-3-5-haiku-20241022-v1:0`)
/// so the Bedrock client can pass it through unchanged.
/// What: Activated by `adapter_for_model("bedrock/...")`. The chat loop
/// detects `Provider::Bedrock` and routes through `crate::llm::bedrock`
/// instead of the OpenRouter/Anthropic-native paths.
/// Test: `adapter_for_model_routes_bedrock`, `bedrock_adapter_strips_prefix`.
#[derive(Debug)]
pub struct BedrockAdapter {
    /// The raw Bedrock model id (everything after `bedrock/`).
    pub model_id: String,
}

impl ModelAdapter for AnthropicAdapter {
    fn provider(&self) -> Provider {
        Provider::Anthropic
    }

    fn tool_choice_any(&self) -> Option<Value> {
        Some(json!({"type": "any"}))
    }

    fn tool_choice_auto(&self) -> Option<Value> {
        Some(json!("auto"))
    }

    fn inject_cache_control(&self, request_body: &mut Value, enabled: bool) {
        if !enabled {
            return;
        }

        // 1. Top-level `system` field (Anthropic-native shape). Some callers
        //    send the system prompt here directly.
        if let Some(system) = request_body.get_mut("system") {
            match system {
                Value::String(s) => {
                    let text = s.clone();
                    *system = json!([{
                        "type": "text",
                        "text": text,
                        "cache_control": {"type": "ephemeral"}
                    }]);
                }
                Value::Array(parts) => {
                    if let Some(last) = parts.last_mut()
                        && let Some(obj) = last.as_object_mut()
                    {
                        obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
                    }
                }
                _ => {}
            }
            return;
        }

        // 2. OpenAI-style messages array — patch the first system message.
        if let Some(msgs) = request_body
            .get_mut("messages")
            .and_then(|v| v.as_array_mut())
        {
            for m in msgs.iter_mut() {
                if m.get("role").and_then(|v| v.as_str()) != Some("system") {
                    continue;
                }
                let text_val = match m.get("content").cloned() {
                    Some(Value::String(s)) => s,
                    Some(Value::Array(_)) => {
                        if let Some(arr) = m.get_mut("content").and_then(|v| v.as_array_mut())
                            && let Some(first) = arr.first_mut()
                            && let Some(obj) = first.as_object_mut()
                        {
                            obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
                        }
                        return;
                    }
                    _ => return,
                };
                m["content"] = json!([{
                    "type": "text",
                    "text": text_val,
                    "cache_control": {"type": "ephemeral"}
                }]);
                return;
            }
        }
    }

    fn parse_usage(&self, response: &Value) -> TokenUsage {
        let usage = &response["usage"];
        // Anthropic may respond with either input_tokens/output_tokens OR the
        // OpenAI-compatible prompt_tokens/completion_tokens when routed via
        // OpenRouter — try both.
        let prompt = usage["input_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens"].as_u64())
            .unwrap_or(0) as u32;
        let completion = usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64())
            .unwrap_or(0) as u32;
        let cache_read = usage["cache_read_input_tokens"]
            .as_u64()
            .or_else(|| {
                usage["prompt_tokens_details"]
                    .get("cached_tokens")
                    .and_then(|v| v.as_u64())
            })
            .unwrap_or(0) as u32;
        let cache_creation = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0) as u32;
        TokenUsage::new(prompt, completion, cache_read, cache_creation)
    }

    fn supports_thinking(&self) -> bool {
        true
    }

    fn api_endpoint(&self, use_direct: bool) -> ApiEndpoint {
        // #62: Direct API mode requires ANTHROPIC_API_KEY from console.anthropic.com.
        // CLAUDE_CODE_OAUTH_TOKEN (sk-ant-oat01-*) is only valid for
        // runner="claude-code" agents (ClaudeCodeAgentRunner subprocess path).
        // Anthropic's REST API rejects OAuth tokens with 401 — do NOT attempt
        // direct API calls with them.
        //
        // Priority 1: ANTHROPIC_API_KEY → api.anthropic.com with x-api-key header.
        // Fallback: OpenRouter — preserves existing deployments unchanged.
        if use_direct
            && let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
            && !key.is_empty()
        {
            return ApiEndpoint {
                base_url: std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string()),
                auth_header_name: "x-api-key".to_string(),
                auth_header_value: key,
                extra_headers: vec![("anthropic-version".to_string(), "2023-06-01".to_string())],
                auth_source: AuthSource::AnthropicApiKey,
            };
        }
        openrouter_endpoint()
    }

    fn uses_native_format(&self) -> bool {
        true
    }
}

impl ModelAdapter for OpenAiAdapter {
    fn provider(&self) -> Provider {
        Provider::OpenAI
    }
    fn tool_choice_any(&self) -> Option<Value> {
        Some(json!("required"))
    }
    fn tool_choice_auto(&self) -> Option<Value> {
        Some(json!("auto"))
    }
    fn inject_cache_control(&self, _: &mut Value, _: bool) {
        // OpenAI has no equivalent; silently no-op.
    }
    fn parse_usage(&self, response: &Value) -> TokenUsage {
        let usage = &response["usage"];
        let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let completion = usage["completion_tokens"].as_u64().unwrap_or(0) as u32;
        let cache_read = usage["prompt_tokens_details"]
            .get("cached_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        TokenUsage::new(prompt, completion, cache_read, 0)
    }
}

impl ModelAdapter for BedrockAdapter {
    fn provider(&self) -> Provider {
        Provider::Bedrock
    }

    fn tool_choice_any(&self) -> Option<Value> {
        // Bedrock Converse expresses tool choice via a `ToolChoice` struct on
        // the request, not a JSON `tool_choice` field on the message body.
        // The Bedrock client constructs that struct from the agent's
        // `ToolChoice` enum directly, so we return None here.
        None
    }

    fn tool_choice_auto(&self) -> Option<Value> {
        None
    }

    fn inject_cache_control(&self, _: &mut Value, _: bool) {
        // Bedrock has its own `CachePoint` block type; unused for now.
    }

    fn parse_usage(&self, response: &Value) -> TokenUsage {
        // Used only when Bedrock results are normalized through a JSON
        // response (currently never — the SDK returns typed values which
        // are translated in `crate::llm::bedrock`). Keep a defensive impl.
        let usage = &response["usage"];
        let prompt = usage["inputTokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens"].as_u64())
            .unwrap_or(0) as u32;
        let completion = usage["outputTokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64())
            .unwrap_or(0) as u32;
        TokenUsage::new(prompt, completion, 0, 0)
    }

    fn api_endpoint(&self, _use_direct: bool) -> ApiEndpoint {
        // Bedrock uses SigV4 + the SDK; no HTTP base URL/auth-header
        // mechanism applies. Return a sentinel whose `auth_source` flags
        // the chat loop to take the SDK path.
        ApiEndpoint {
            base_url: "https://bedrock-runtime.amazonaws.com".to_string(),
            auth_header_name: String::new(),
            auth_header_value: String::new(),
            extra_headers: vec![],
            auth_source: AuthSource::Bedrock,
        }
    }
}

impl ModelAdapter for OllamaAdapter {
    fn provider(&self) -> Provider {
        Provider::Generic
    }
    fn tool_choice_any(&self) -> Option<Value> {
        // ollama's OpenAI-compat layer accepts "auto"/"required" but not all
        // models honor it. Return None to keep the request payload minimal.
        None
    }
    fn tool_choice_auto(&self) -> Option<Value> {
        None
    }
    fn inject_cache_control(&self, _: &mut Value, _: bool) {
        // ollama has no cache_control concept.
    }
    fn parse_usage(&self, response: &Value) -> TokenUsage {
        let usage = &response["usage"];
        let prompt = usage["prompt_tokens"]
            .as_u64()
            .or_else(|| usage["input_tokens"].as_u64())
            .unwrap_or(0) as u32;
        let completion = usage["completion_tokens"]
            .as_u64()
            .or_else(|| usage["output_tokens"].as_u64())
            .unwrap_or(0) as u32;
        TokenUsage::new(prompt, completion, 0, 0)
    }
    fn api_endpoint(&self, _use_direct: bool) -> ApiEndpoint {
        let host =
            std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());
        ApiEndpoint {
            base_url: format!("{}/v1", host.trim_end_matches('/')),
            // ollama needs no auth — leave the header empty so the LLM HTTP
            // layer skips Authorization (or sends an empty header which ollama
            // ignores).
            auth_header_name: "Authorization".to_string(),
            auth_header_value: String::new(),
            extra_headers: vec![],
            // Tag with OpenRouter so the existing routing/credential paths
            // treat it as a generic OpenAI-compatible endpoint.
            auth_source: AuthSource::OpenRouter,
        }
    }
}

impl ModelAdapter for GenericAdapter {
    fn provider(&self) -> Provider {
        Provider::Generic
    }
    fn tool_choice_any(&self) -> Option<Value> {
        None
    }
    fn tool_choice_auto(&self) -> Option<Value> {
        None
    }
    fn inject_cache_control(&self, _: &mut Value, _: bool) {}
    fn parse_usage(&self, response: &Value) -> TokenUsage {
        let usage = &response["usage"];
        let prompt = usage["prompt_tokens"]
            .as_u64()
            .or_else(|| usage["input_tokens"].as_u64())
            .unwrap_or(0) as u32;
        let completion = usage["completion_tokens"]
            .as_u64()
            .or_else(|| usage["output_tokens"].as_u64())
            .unwrap_or(0) as u32;
        TokenUsage::new(prompt, completion, 0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_for_model_routes_anthropic() {
        let a = adapter_for_model("anthropic/claude-sonnet-4-6");
        assert_eq!(a.provider(), Provider::Anthropic);
        let a = adapter_for_model("claude-haiku-4");
        assert_eq!(a.provider(), Provider::Anthropic);
    }

    #[test]
    fn adapter_for_model_routes_openai() {
        assert_eq!(
            adapter_for_model("openai/gpt-4.1").provider(),
            Provider::OpenAI
        );
        assert_eq!(
            adapter_for_model("openai/o3-mini").provider(),
            Provider::OpenAI
        );
        assert_eq!(adapter_for_model("gpt-4o").provider(), Provider::OpenAI);
    }

    #[test]
    fn adapter_for_model_routes_bedrock() {
        let a = adapter_for_model("bedrock/anthropic.claude-3-5-haiku-20241022-v1:0");
        assert_eq!(a.provider(), Provider::Bedrock);
        // Even an Anthropic-flavored model id under bedrock/ must NOT route
        // to AnthropicAdapter — the SDK path is required.
        let a = adapter_for_model("bedrock/anthropic.claude-3-opus-20240229-v1:0");
        assert_eq!(a.provider(), Provider::Bedrock);
    }

    #[test]
    fn bedrock_adapter_strips_prefix() {
        let a = BedrockAdapter {
            model_id: "anthropic.claude-3-5-haiku-20241022-v1:0".to_string(),
        };
        assert_eq!(a.model_id, "anthropic.claude-3-5-haiku-20241022-v1:0");
        let ep = a.api_endpoint(true);
        assert_eq!(ep.auth_source, AuthSource::Bedrock);
    }

    #[test]
    fn adapter_for_model_routes_generic_for_unknown() {
        assert_eq!(
            adapter_for_model("google/gemini-2.5-flash").provider(),
            Provider::Generic
        );
    }

    #[test]
    fn anthropic_tool_choice_any_shape() {
        let a = AnthropicAdapter;
        assert_eq!(a.tool_choice_any().unwrap(), json!({"type": "any"}));
    }

    #[test]
    fn openai_tool_choice_any_shape() {
        let a = OpenAiAdapter;
        assert_eq!(a.tool_choice_any().unwrap(), json!("required"));
    }

    #[test]
    fn generic_tool_choice_any_is_none() {
        let a = GenericAdapter;
        assert!(a.tool_choice_any().is_none());
    }

    #[test]
    fn tool_choice_auto_shapes() {
        assert_eq!(AnthropicAdapter.tool_choice_auto().unwrap(), json!("auto"));
        assert_eq!(OpenAiAdapter.tool_choice_auto().unwrap(), json!("auto"));
        assert!(GenericAdapter.tool_choice_auto().is_none());
    }

    #[test]
    fn anthropic_inject_cache_control_on_string_system() -> anyhow::Result<()> {
        // When the request body has a top-level `system: "text"`, it should be
        // expanded into a content-block array carrying cache_control.
        let mut body = json!({
            "model": "anthropic/claude-sonnet-4-6",
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "hi"}]
        });
        AnthropicAdapter.inject_cache_control(&mut body, true);
        let sys = body
            .get("system")
            .ok_or_else(|| anyhow::anyhow!("expected `system` key on body, got: {:?}", body))?;
        let arr = sys.as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "expected system to be array after injection, got: {:?}",
                sys
            )
        })?;
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "You are helpful.");
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral");
        Ok(())
    }

    #[test]
    fn anthropic_inject_cache_control_on_messages_system() -> anyhow::Result<()> {
        // OpenAI-style body: system lives inside messages.
        let mut body = json!({
            "model": "anthropic/claude-sonnet-4-6",
            "messages": [
                {"role": "system", "content": "sys prompt"},
                {"role": "user", "content": "hi"}
            ]
        });
        AnthropicAdapter.inject_cache_control(&mut body, true);
        let msgs = body["messages"].as_array().ok_or_else(|| {
            anyhow::anyhow!("expected messages to be array, got: {:?}", body["messages"])
        })?;
        let sys = &msgs[0];
        let content = sys["content"].as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "expected system message content to be array after injection, got: {:?}",
                sys["content"]
            )
        })?;
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        Ok(())
    }

    #[test]
    fn anthropic_inject_cache_control_noop_when_disabled() {
        let mut body = json!({"system": "hi"});
        AnthropicAdapter.inject_cache_control(&mut body, false);
        assert_eq!(body["system"], json!("hi"));
    }

    #[test]
    fn openai_inject_cache_control_is_noop() {
        let mut body = json!({"messages":[{"role":"system","content":"x"}]});
        let before = body.clone();
        OpenAiAdapter.inject_cache_control(&mut body, true);
        assert_eq!(before, body);
    }

    #[test]
    fn anthropic_parse_usage_native_shape() {
        let resp = json!({
            "usage": {
                "input_tokens": 1000,
                "output_tokens": 200,
                "cache_read_input_tokens": 400,
                "cache_creation_input_tokens": 100
            }
        });
        let u = AnthropicAdapter.parse_usage(&resp);
        assert_eq!(u.prompt_tokens, 1000);
        assert_eq!(u.completion_tokens, 200);
        assert_eq!(u.cache_read_tokens, 400);
        assert_eq!(u.cache_creation_tokens, 100);
    }

    #[test]
    fn openai_parse_usage_shape() {
        let resp = json!({
            "usage": {
                "prompt_tokens": 900,
                "completion_tokens": 150,
                "prompt_tokens_details": {"cached_tokens": 300}
            }
        });
        let u = OpenAiAdapter.parse_usage(&resp);
        assert_eq!(u.prompt_tokens, 900);
        assert_eq!(u.completion_tokens, 150);
        assert_eq!(u.cache_read_tokens, 300);
        assert_eq!(u.cache_creation_tokens, 0);
    }

    #[test]
    fn generic_parse_usage_tries_both_shapes() {
        let openai_shape = json!({"usage":{"prompt_tokens":10,"completion_tokens":5}});
        let u = GenericAdapter.parse_usage(&openai_shape);
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 5);

        let anthropic_shape = json!({"usage":{"input_tokens":30,"output_tokens":7}});
        let u = GenericAdapter.parse_usage(&anthropic_shape);
        assert_eq!(u.prompt_tokens, 30);
        assert_eq!(u.completion_tokens, 7);
    }

    #[test]
    fn supports_thinking_only_anthropic() {
        assert!(AnthropicAdapter.supports_thinking());
        assert!(!OpenAiAdapter.supports_thinking());
        assert!(!GenericAdapter.supports_thinking());
    }

    // The api_endpoint tests mutate process-global env. Serialize them behind
    // a mutex so they don't race with each other or with `agents::mod::tests`.
    use std::sync::Mutex;
    static ENDPOINT_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(kvs: &[(&str, Option<&str>)], f: F) {
        let _g = ENDPOINT_ENV_LOCK.lock().unwrap();
        // Snapshot previous values to restore afterwards.
        let prev: Vec<_> = kvs
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        // SAFETY: test helper, single-threaded under ENDPOINT_ENV_LOCK.
        unsafe {
            for (k, v) in kvs {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
        f();
        // SAFETY: same lock still held.
        unsafe {
            for (k, v) in prev {
                match v {
                    Some(val) => std::env::set_var(&k, val),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }

    #[test]
    fn anthropic_api_endpoint_direct_when_key_set() {
        with_env(
            &[
                ("CLAUDE_CODE_OAUTH_TOKEN", None),
                ("ANTHROPIC_API_KEY", Some("sk-ant-test123")),
                ("ANTHROPIC_BASE_URL", None),
            ],
            || {
                let ep = AnthropicAdapter.api_endpoint(true);
                assert!(
                    ep.base_url.contains("api.anthropic.com"),
                    "unexpected base_url: {}",
                    ep.base_url
                );
                assert_eq!(ep.auth_header_name, "x-api-key");
                assert_eq!(ep.auth_header_value, "sk-ant-test123");
                assert!(
                    ep.extra_headers
                        .iter()
                        .any(|(k, v)| k == "anthropic-version" && v == "2023-06-01"),
                    "missing anthropic-version header: {:?}",
                    ep.extra_headers
                );
                assert_eq!(ep.auth_source, AuthSource::AnthropicApiKey);
            },
        );
    }

    #[test]
    fn anthropic_api_endpoint_use_direct_false_goes_to_openrouter() {
        with_env(
            &[
                ("CLAUDE_CODE_OAUTH_TOKEN", None),
                ("ANTHROPIC_API_KEY", Some("sk-ant-test123")),
                ("OPENROUTER_API_KEY", Some("sk-or-v1-test")),
                ("OPENROUTER_BASE_URL", None),
            ],
            || {
                let ep = AnthropicAdapter.api_endpoint(false);
                assert!(
                    ep.base_url.contains("openrouter.ai"),
                    "unexpected base_url: {}",
                    ep.base_url
                );
                assert_eq!(ep.auth_header_name, "Authorization");
                assert!(ep.auth_header_value.starts_with("Bearer "));
            },
        );
    }

    #[test]
    fn anthropic_api_endpoint_falls_back_to_openrouter_without_key() {
        with_env(
            &[
                ("CLAUDE_CODE_OAUTH_TOKEN", None),
                ("ANTHROPIC_API_KEY", None),
                ("OPENROUTER_API_KEY", Some("sk-or-v1-test")),
                ("OPENROUTER_BASE_URL", None),
            ],
            || {
                let ep = AnthropicAdapter.api_endpoint(true);
                assert!(
                    ep.base_url.contains("openrouter.ai"),
                    "unexpected base_url: {}",
                    ep.base_url
                );
                assert_eq!(ep.auth_header_name, "Authorization");
            },
        );
    }

    #[test]
    fn openai_api_endpoint_always_openrouter() {
        with_env(
            &[
                ("ANTHROPIC_API_KEY", Some("sk-ant-test")),
                ("OPENROUTER_API_KEY", Some("sk-or-v1-test")),
                ("OPENROUTER_BASE_URL", None),
            ],
            || {
                let ep = OpenAiAdapter.api_endpoint(true);
                assert!(
                    ep.base_url.contains("openrouter.ai"),
                    "OpenAI should never route direct-Anthropic: got {}",
                    ep.base_url
                );
                assert_eq!(ep.auth_header_name, "Authorization");
            },
        );
    }

    #[test]
    fn generic_api_endpoint_is_openrouter() {
        with_env(
            &[
                ("OPENROUTER_API_KEY", Some("sk-or-v1-test")),
                ("OPENROUTER_BASE_URL", None),
            ],
            || {
                let ep = GenericAdapter.api_endpoint(true);
                assert!(ep.base_url.contains("openrouter.ai"));
            },
        );
    }

    #[test]
    fn anthropic_uses_native_format_true() {
        assert!(AnthropicAdapter.uses_native_format());
    }

    #[test]
    fn openai_uses_native_format_false() {
        assert!(!OpenAiAdapter.uses_native_format());
        assert!(!GenericAdapter.uses_native_format());
    }

    // --- #62: Direct API auth tests ---
    // CLAUDE_CODE_OAUTH_TOKEN must NOT be used for direct API routing.
    // sk-ant-oat01-* tokens are rejected by api.anthropic.com with 401.
    // OAuth tokens are only valid for runner="claude-code" agents.

    #[test]
    fn oauth_token_is_not_used_for_direct_api_routing() {
        // Even when CLAUDE_CODE_OAUTH_TOKEN is set, it must NOT route to
        // api.anthropic.com. The token is for ClaudeCodeAgentRunner only.
        // With no ANTHROPIC_API_KEY, we expect OpenRouter fallback.
        with_env(
            &[
                ("CLAUDE_CODE_OAUTH_TOKEN", Some("sk-ant-oat01-fake-oauth")),
                ("ANTHROPIC_API_KEY", None),
                ("OPENROUTER_API_KEY", Some("sk-or-v1-test")),
                ("OPENROUTER_BASE_URL", None),
            ],
            || {
                let ep = AnthropicAdapter.api_endpoint(true);
                assert_eq!(
                    ep.auth_source,
                    AuthSource::OpenRouter,
                    "OAuth token must NOT produce direct API endpoint"
                );
                assert!(
                    ep.base_url.contains("openrouter.ai"),
                    "expected openrouter.ai fallback, got: {}",
                    ep.base_url
                );
            },
        );
    }

    #[test]
    fn oauth_token_present_with_api_key_uses_api_key() {
        // When both CLAUDE_CODE_OAUTH_TOKEN and ANTHROPIC_API_KEY are set,
        // the OAuth token is ignored and ANTHROPIC_API_KEY is used for direct API.
        with_env(
            &[
                (
                    "CLAUDE_CODE_OAUTH_TOKEN",
                    Some("sk-ant-oat01-should-be-ignored"),
                ),
                ("ANTHROPIC_API_KEY", Some("sk-ant-real-key")),
                ("ANTHROPIC_BASE_URL", None),
            ],
            || {
                let ep = AnthropicAdapter.api_endpoint(true);
                assert_eq!(ep.auth_source, AuthSource::AnthropicApiKey);
                assert_eq!(ep.auth_header_name, "x-api-key");
                assert_eq!(ep.auth_header_value, "sk-ant-real-key");
                assert!(
                    ep.base_url.contains("api.anthropic.com"),
                    "expected api.anthropic.com, got: {}",
                    ep.base_url
                );
            },
        );
    }

    #[test]
    fn api_key_used_when_no_oauth_token() {
        // When only ANTHROPIC_API_KEY is set (no OAuth token), the adapter
        // must use the API-key path with x-api-key header.
        with_env(
            &[
                ("CLAUDE_CODE_OAUTH_TOKEN", None),
                ("ANTHROPIC_API_KEY", Some("sk-ant-only-key")),
                ("ANTHROPIC_BASE_URL", None),
            ],
            || {
                let ep = AnthropicAdapter.api_endpoint(true);
                assert_eq!(ep.auth_source, AuthSource::AnthropicApiKey);
                assert_eq!(ep.auth_header_name, "x-api-key");
                assert_eq!(ep.auth_header_value, "sk-ant-only-key");
            },
        );
    }

    #[test]
    fn falls_back_to_openrouter_when_use_direct_false() {
        // Regardless of which Anthropic credentials are present, use_direct=false
        // must route through OpenRouter.
        with_env(
            &[
                ("CLAUDE_CODE_OAUTH_TOKEN", Some("oauth-tok-abc")),
                ("ANTHROPIC_API_KEY", Some("sk-ant-test")),
                ("OPENROUTER_API_KEY", Some("sk-or-v1-test")),
                ("OPENROUTER_BASE_URL", None),
            ],
            || {
                let ep = AnthropicAdapter.api_endpoint(false);
                assert_eq!(ep.auth_source, AuthSource::OpenRouter);
                assert!(
                    ep.base_url.contains("openrouter.ai"),
                    "expected openrouter.ai, got: {}",
                    ep.base_url
                );
            },
        );
    }

    #[test]
    fn empty_oauth_token_with_api_key_uses_api_key() {
        // An empty CLAUDE_CODE_OAUTH_TOKEN is irrelevant; ANTHROPIC_API_KEY is used.
        with_env(
            &[
                ("CLAUDE_CODE_OAUTH_TOKEN", Some("")),
                ("ANTHROPIC_API_KEY", Some("sk-ant-fallback")),
                ("ANTHROPIC_BASE_URL", None),
            ],
            || {
                let ep = AnthropicAdapter.api_endpoint(true);
                assert_eq!(ep.auth_source, AuthSource::AnthropicApiKey);
                assert_eq!(ep.auth_header_name, "x-api-key");
                assert_eq!(ep.auth_header_value, "sk-ant-fallback");
            },
        );
    }
}
