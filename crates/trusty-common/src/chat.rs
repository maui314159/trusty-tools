//! Provider-agnostic streaming chat abstraction with tool-use support.
//!
//! Why: trusty-memory and trusty-search both want to support more than one
//! upstream LLM (OpenRouter for cloud, Ollama / LM Studio for local). Rather
//! than each crate re-implementing the dispatch, we expose a small
//! [`ChatProvider`] trait plus two concrete implementations and an
//! auto-detector for a running local model server. The trait also surfaces
//! OpenAI-style tool/function calling so downstream agents can let the model
//! invoke tools (search, memory recall, shell, etc.).
//!
//! What: defines the [`ChatProvider`] trait, [`ToolDef`] / [`ToolCall`] /
//! [`ChatEvent`] tool-use types, an [`OpenRouterProvider`] and an
//! [`OllamaProvider`] that both speak OpenAI-compatible
//! `/v1/chat/completions` with SSE streaming (including the streamed
//! `tool_calls` shape), and [`auto_detect_local_provider`] which probes
//! `{base_url}/v1/models` with a 1-second timeout.
//!
//! Test: `cargo test -p trusty-common` covers default config values, the
//! unreachable-server path of `auto_detect_local_provider`, SSE delta
//! streaming, and accumulation of streamed tool-call fragments.

use crate::ChatMessage;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;

const LOCAL_PROBE_TIMEOUT_SECS: u64 = 1;
const LOCAL_REQUEST_TIMEOUT_SECS: u64 = 120;
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const OPENROUTER_CONNECT_TIMEOUT_SECS: u64 = 10;
const OPENROUTER_REQUEST_TIMEOUT_SECS: u64 = 120;
const HTTP_REFERER: &str = "https://github.com/bobmatnyc/trusty-common";
const X_TITLE: &str = "trusty-common";

/// Configuration for a local OpenAI-compatible model server (Ollama, LM
/// Studio, llama.cpp's server, etc.).
///
/// Why: callers want a single struct they can deserialize from config files
/// and pass to [`auto_detect_local_provider`] without juggling defaults.
/// What: holds an enable flag, the server's base URL (no trailing slash),
/// and the default model to request. Defaults target Ollama's standard
/// localhost binding.
/// Test: `local_model_config_defaults` asserts the default values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalModelConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
}

impl Default for LocalModelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: "http://localhost:11434".to_string(),
            model: "qwen3:30b".to_string(),
        }
    }
}

// ─── Tool-use types ───────────────────────────────────────────────────────

/// JSON-Schema description of a callable tool, in OpenAI function-calling
/// shape.
///
/// Why: downstream agents (trusty-memory, trusty-search) expose tools like
/// `memory_recall` or `web_search` to the LLM. The OpenAI tool format is the
/// de-facto common denominator across OpenRouter, Ollama, LM Studio, and
/// most cloud providers.
/// What: `name` and `description` are passed verbatim; `parameters` is a
/// JSON Schema object (typically `{"type":"object","properties":{...}}`).
/// Test: `tool_def_serializes_as_function` checks the wire shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A tool invocation the model wants the host to perform.
///
/// Why: the streaming chat API emits `tool_calls` in fragments — first an
/// `id` + `function.name`, then a string of `function.arguments` deltas.
/// We accumulate fragments and surface one fully-formed [`ToolCall`] per
/// invocation to the caller.
/// What: `id` is the upstream's call id (echoed back in subsequent
/// `role:"tool"` messages); `name` is the function name; `arguments` is a
/// JSON string (NOT a parsed value — many models emit malformed JSON and
/// callers want the raw text for error reporting / repair).
/// Test: `accumulates_streamed_tool_call_fragments`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Streaming chat event.
///
/// Why: replaces the previous "string-only" channel so callers can
/// distinguish text deltas from tool invocations and from terminal
/// success/error without parsing magic markers out of the text stream.
/// What: `Delta` is a content chunk; `ToolCall` is a fully-accumulated tool
/// invocation; `Done` signals the upstream stream terminated normally;
/// `Error` carries a human-readable message for stream-mid failures (the
/// provider also returns `Err` from `chat_stream`, but `Error` lets the
/// caller display partial-stream failures inline).
/// Test: `ollama_provider_streams_sse_deltas`.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    Delta(String),
    ToolCall(ToolCall),
    Done,
    Error(String),
}

/// Streaming chat provider abstraction.
///
/// Why: downstream crates (trusty-memory, trusty-search) want to support
/// multiple LLM backends without hard-coding which one to call. Providers
/// expose a uniform streaming interface so the caller can swap them at
/// runtime based on configuration / availability.
/// What: implementors stream [`ChatEvent`]s into `tx`. Pass an empty
/// `tools` vec to disable tool use entirely (the provider MUST then omit
/// the `tools` field from the upstream request — some models error on an
/// empty array). Returning `Ok(())` means the stream completed normally;
/// the caller should also expect a final [`ChatEvent::Done`].
/// Test: implementations are covered by their own unit tests in this
/// module plus integration tests in downstream crates.
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Human-readable provider name (e.g. `"openrouter"`, `"ollama"`).
    fn name(&self) -> &str;
    /// Model identifier sent on every request.
    fn model(&self) -> &str;
    /// Stream chat events into `tx`. `tools` empty disables tool use.
    async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
        tx: Sender<ChatEvent>,
    ) -> Result<()>;
}

// ─── Shared SSE / request types ────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OpenAiToolWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiFunctionWire<'a>,
}

#[derive(Debug, Serialize)]
struct OpenAiFunctionWire<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ChatRequestWire<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiToolWire<'a>>>,
}

fn tools_wire(tools: &[ToolDef]) -> Option<Vec<OpenAiToolWire<'_>>> {
    if tools.is_empty() {
        None
    } else {
        Some(
            tools
                .iter()
                .map(|t| OpenAiToolWire {
                    kind: "function",
                    function: OpenAiFunctionWire {
                        name: &t.name,
                        description: &t.description,
                        parameters: &t.parameters,
                    },
                })
                .collect(),
        )
    }
}

/// Accumulator for streamed tool-call fragments.
///
/// Why: OpenAI-style streaming sends each tool call across multiple SSE
/// frames: the first frame at a given `index` carries `id` and
/// `function.name`; subsequent frames append to `function.arguments`. We
/// accumulate by `index` and emit fully-formed [`ToolCall`]s only after the
/// stream terminates (or we see `finish_reason: tool_calls`).
/// What: a vector slot per index, growing as needed; merge logic is in
/// `apply_delta`. `finalize` drops slots that never received an id (defensive
/// — shouldn't happen but avoids emitting half-baked calls).
/// Test: `accumulates_streamed_tool_call_fragments`.
#[derive(Debug, Default)]
struct ToolCallAccumulator {
    // index -> (id, name, args)
    slots: Vec<Option<(String, String, String)>>,
}

impl ToolCallAccumulator {
    fn apply_delta(&mut self, tool_calls: &serde_json::Value) {
        let Some(arr) = tool_calls.as_array() else {
            return;
        };
        for tc in arr {
            let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            while self.slots.len() <= idx {
                self.slots.push(None);
            }
            let slot = self.slots[idx]
                .get_or_insert_with(|| (String::new(), String::new(), String::new()));
            if let Some(id) = tc.get("id").and_then(|v| v.as_str())
                && !id.is_empty()
            {
                slot.0 = id.to_string();
            }
            if let Some(func) = tc.get("function") {
                if let Some(name) = func.get("name").and_then(|v| v.as_str())
                    && !name.is_empty()
                {
                    slot.1 = name.to_string();
                }
                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                    slot.2.push_str(args);
                }
            }
        }
    }

    fn finalize(self) -> Vec<ToolCall> {
        self.slots
            .into_iter()
            .filter_map(|opt| {
                opt.and_then(|(id, name, arguments)| {
                    if name.is_empty() {
                        None
                    } else {
                        Some(ToolCall {
                            id,
                            name,
                            arguments,
                        })
                    }
                })
            })
            .collect()
    }
}

/// Drive one OpenAI-compatible SSE stream into the caller's [`ChatEvent`]
/// channel.
///
/// Why: OpenRouter and Ollama both speak the same wire format; sharing the
/// loop keeps the two providers in lock-step.
/// What: reads `resp.bytes_stream()`, splits on newlines, parses `data:`
/// frames, forwards `delta.content` as [`ChatEvent::Delta`], accumulates
/// `delta.tool_calls`, and on `[DONE]` (or upstream EOF) emits one
/// [`ChatEvent::ToolCall`] per accumulated call followed by
/// [`ChatEvent::Done`].
/// Test: covered by `ollama_provider_streams_sse_deltas` and
/// `accumulates_streamed_tool_call_fragments`.
async fn pump_openai_sse(resp: reqwest::Response, tx: Sender<ChatEvent>) -> Result<()> {
    use futures_util::StreamExt;

    let mut acc = ToolCallAccumulator::default();
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("read chat stream chunk")?;
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        buf.push_str(text);

        while let Some(idx) = buf.find('\n') {
            let line: String = buf.drain(..=idx).collect();
            let line = line.trim();
            let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
                continue;
            };
            if payload.is_empty() {
                continue;
            }
            if payload == "[DONE]" {
                // Flush accumulated tool calls and finish.
                for call in std::mem::take(&mut acc).finalize() {
                    if tx.send(ChatEvent::ToolCall(call)).await.is_err() {
                        return Ok(());
                    }
                }
                let _ = tx.send(ChatEvent::Done).await;
                return Ok(());
            }
            let v: serde_json::Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let delta = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"));
            if let Some(delta) = delta {
                if let Some(content) = delta.get("content").and_then(|c| c.as_str())
                    && !content.is_empty()
                    && tx
                        .send(ChatEvent::Delta(content.to_string()))
                        .await
                        .is_err()
                {
                    return Ok(());
                }
                if let Some(tc) = delta.get("tool_calls") {
                    acc.apply_delta(tc);
                }
            }
        }
    }

    // Upstream EOF without a [DONE] sentinel — still flush and finish.
    for call in acc.finalize() {
        if tx.send(ChatEvent::ToolCall(call)).await.is_err() {
            return Ok(());
        }
    }
    let _ = tx.send(ChatEvent::Done).await;
    Ok(())
}

// ─── OpenRouter ───────────────────────────────────────────────────────────

/// Cloud chat provider backed by OpenRouter.
///
/// Why: lets callers pick OpenRouter or a local model uniformly through
/// the [`ChatProvider`] trait.
/// What: stores an API key and model id; POSTs OpenAI-compatible streaming
/// chat completions with bearer auth and trusty-common branding headers.
/// Test: shape covered by `openrouter_provider_reports_metadata`; the
/// streaming and tool-call paths are covered by integration tests in
/// downstream crates plus the SSE-pump unit tests in this module.
pub struct OpenRouterProvider {
    pub api_key: String,
    pub model: String,
}

impl OpenRouterProvider {
    /// Construct a provider from an API key and model id.
    ///
    /// Why: keeps callers from poking the public fields directly so the
    /// struct can grow optional knobs without breaking call sites.
    /// What: stores both fields verbatim.
    /// Test: trivially exercised by `openrouter_provider_reports_metadata`.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl ChatProvider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
        tx: Sender<ChatEvent>,
    ) -> Result<()> {
        if self.api_key.is_empty() {
            return Err(anyhow!("openrouter api key is empty"));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(
                OPENROUTER_CONNECT_TIMEOUT_SECS,
            ))
            .timeout(std::time::Duration::from_secs(
                OPENROUTER_REQUEST_TIMEOUT_SECS,
            ))
            .build()
            .context("build reqwest client for OpenRouterProvider::chat_stream")?;

        let tools_wire = tools_wire(&tools);
        let body = ChatRequestWire {
            model: &self.model,
            messages: &messages,
            stream: true,
            tools: tools_wire,
        };
        let resp = client
            .post(OPENROUTER_URL)
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", HTTP_REFERER)
            .header("X-Title", X_TITLE)
            .json(&body)
            .send()
            .await
            .context("POST openrouter chat completions (stream)")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("openrouter HTTP {status}: {text}"));
        }

        pump_openai_sse(resp, tx).await
    }
}

// ─── Ollama / OpenAI-compatible local ─────────────────────────────────────

/// Local chat provider for OpenAI-compatible servers (Ollama, LM Studio,
/// llama.cpp's `server`, vLLM, etc.).
///
/// Why: developers increasingly run a local model server during dev to avoid
/// API costs and latency. The OpenAI-compatible `/v1/chat/completions`
/// endpoint with SSE streaming is the de-facto common denominator.
/// What: stores the server's base URL and the model id to request.
/// `chat_stream` POSTs `{model, messages, tools?, stream: true}` and parses
/// SSE `data:` frames identically to the OpenRouter path.
/// Test: shape covered by `ollama_provider_reports_metadata`; streaming and
/// tool-call accumulation by `ollama_provider_streams_sse_deltas` and
/// `accumulates_streamed_tool_call_fragments`.
pub struct OllamaProvider {
    pub base_url: String,
    pub model: String,
}

impl OllamaProvider {
    /// Construct a provider from a base URL and model id.
    ///
    /// Why: parallel to [`OpenRouterProvider::new`] so callers see a
    /// consistent shape across providers.
    /// What: stores both fields verbatim; the base URL should NOT have a
    /// trailing slash — the implementation appends `/v1/chat/completions`.
    /// Test: covered by `ollama_provider_reports_metadata`.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl ChatProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
        tx: Sender<ChatEvent>,
    ) -> Result<()> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(LOCAL_PROBE_TIMEOUT_SECS))
            .timeout(std::time::Duration::from_secs(LOCAL_REQUEST_TIMEOUT_SECS))
            .build()
            .context("build reqwest client for OllamaProvider::chat_stream")?;

        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
        let tools_wire = tools_wire(&tools);
        let body = ChatRequestWire {
            model: &self.model,
            messages: &messages,
            stream: true,
            tools: tools_wire,
        };
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("local chat HTTP {status}: {text}"));
        }

        pump_openai_sse(resp, tx).await
    }
}

/// Probe a local model server and return an [`OllamaProvider`] if reachable.
///
/// Why: at startup, downstream daemons want to know whether a local model
/// server is running before falling back to a cloud provider. The OpenAI
/// `/v1/models` endpoint is a cheap, side-effect-free liveness check that
/// Ollama, LM Studio, and llama.cpp's server all implement.
/// What: GETs `{base_url}/v1/models` with a 1-second total timeout. Returns
/// `Some(OllamaProvider { base_url, model: "" })` on any 2xx response.
/// Returns `None` on network errors, timeouts, or non-2xx status. Never
/// returns an error — the caller treats absence as "no local provider
/// available" and is responsible for setting the model id afterwards (e.g.
/// from [`LocalModelConfig::model`]).
/// Test: `auto_detect_returns_none_on_unreachable` points at a closed port
/// and asserts `None` within the 1-second budget;
/// `auto_detect_returns_some_on_200` spins up an in-process server and
/// asserts a provider is returned.
pub async fn auto_detect_local_provider(base_url: &str) -> Option<OllamaProvider> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(LOCAL_PROBE_TIMEOUT_SECS))
        .timeout(std::time::Duration::from_secs(LOCAL_PROBE_TIMEOUT_SECS))
        .build()
        .ok()?;

    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            Some(OllamaProvider::new(base_url.to_string(), String::new()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_model_config_defaults() {
        let cfg = LocalModelConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.base_url, "http://localhost:11434");
        assert_eq!(cfg.model, "qwen3:30b");
    }

    #[test]
    fn openrouter_provider_reports_metadata() {
        let p = OpenRouterProvider::new("sk-xxx", "anthropic/claude-3.5-sonnet");
        assert_eq!(p.name(), "openrouter");
        assert_eq!(p.model(), "anthropic/claude-3.5-sonnet");
    }

    #[test]
    fn ollama_provider_reports_metadata() {
        let p = OllamaProvider::new("http://localhost:11434", "llama3.2");
        assert_eq!(p.name(), "ollama");
        assert_eq!(p.model(), "llama3.2");
    }

    #[test]
    fn tool_def_serializes_as_function() {
        // When passed through `tools_wire`, a ToolDef should produce a JSON
        // object that matches the OpenAI function-calling shape.
        let tools = vec![ToolDef {
            name: "search".into(),
            description: "Search the web".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
            }),
        }];
        let wire = tools_wire(&tools).expect("expected Some");
        let v = serde_json::to_value(&wire).unwrap();
        assert_eq!(v[0]["type"], "function");
        assert_eq!(v[0]["function"]["name"], "search");
        assert_eq!(v[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn empty_tools_serializes_to_none() {
        // Empty tools must omit the field entirely so models that error on
        // empty arrays still work.
        assert!(tools_wire(&[]).is_none());
    }

    #[test]
    fn accumulates_streamed_tool_call_fragments() {
        // Simulate three SSE deltas for a single tool call: id+name, then
        // two args fragments. After finalize, we should see one fully-formed
        // ToolCall with concatenated arguments.
        let mut acc = ToolCallAccumulator::default();
        acc.apply_delta(&serde_json::json!([{
            "index": 0,
            "id": "call_abc",
            "function": { "name": "search", "arguments": "" }
        }]));
        acc.apply_delta(&serde_json::json!([{
            "index": 0,
            "function": { "arguments": "{\"query\":\"" }
        }]));
        acc.apply_delta(&serde_json::json!([{
            "index": 0,
            "function": { "arguments": "rust\"}" }
        }]));
        let calls = acc.finalize();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments, "{\"query\":\"rust\"}");
    }

    #[tokio::test]
    async fn auto_detect_returns_none_on_unreachable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let base = format!("http://127.0.0.1:{port}");
        let start = std::time::Instant::now();
        let got = auto_detect_local_provider(&base).await;
        let elapsed = start.elapsed();
        assert!(got.is_none(), "expected None for unreachable server");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "auto-detect took too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn auto_detect_returns_some_on_200() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = b"{\"data\":[]}";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.shutdown().await;
            }
        });

        let got = auto_detect_local_provider(&base).await;
        assert!(got.is_some(), "expected Some for reachable 200 server");
        let p = got.unwrap();
        assert_eq!(p.name(), "ollama");
        assert_eq!(p.base_url, base);
    }

    #[test]
    fn local_model_config_deserializes_from_toml() {
        let toml_src = r#"
            enabled = true
            base_url = "http://localhost:1234"
            model = "qwen2.5-coder"
        "#;
        let cfg: LocalModelConfig = toml::from_str(toml_src).expect("parse TOML");
        assert!(cfg.enabled);
        assert_eq!(cfg.base_url, "http://localhost:1234");
        assert_eq!(cfg.model, "qwen2.5-coder");
    }

    #[tokio::test]
    async fn ollama_provider_streams_sse_deltas() {
        // Inline server replies with two content deltas plus [DONE]. We
        // expect two Delta events followed by Done.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;

                let sse_body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\n\n",
                    "data: [DONE]\n\n",
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_body.len(),
                    sse_body
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        let provider = OllamaProvider::new(base, "test-model");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatEvent>(8);
        let handle = tokio::spawn(async move {
            provider
                .chat_stream(
                    vec![ChatMessage {
                        role: "user".into(),
                        content: "hi".into(),
                        tool_call_id: None,
                        tool_calls: None,
                    }],
                    vec![],
                    tx,
                )
                .await
        });

        let mut deltas = Vec::new();
        let mut saw_done = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                ChatEvent::Delta(s) => deltas.push(s),
                ChatEvent::Done => saw_done = true,
                ChatEvent::ToolCall(_) => panic!("unexpected tool call"),
                ChatEvent::Error(e) => panic!("stream error: {e}"),
            }
        }
        let result = handle.await.expect("task panicked");
        assert!(result.is_ok(), "chat_stream errored: {result:?}");
        assert_eq!(deltas, vec!["hello ".to_string(), "world".to_string()]);
        assert!(saw_done, "expected ChatEvent::Done");
    }

    #[tokio::test]
    async fn ollama_provider_emits_tool_call() {
        // SSE stream that delivers one tool call across two fragments.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;

                let sse_body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"rust\\\"}\"}}]}}]}\n\n",
                    "data: [DONE]\n\n",
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_body.len(),
                    sse_body
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        let provider = OllamaProvider::new(base, "test-model");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatEvent>(8);
        let handle = tokio::spawn(async move {
            provider
                .chat_stream(
                    vec![ChatMessage {
                        role: "user".into(),
                        content: "search rust".into(),
                        tool_call_id: None,
                        tool_calls: None,
                    }],
                    vec![ToolDef {
                        name: "search".into(),
                        description: "search the web".into(),
                        parameters: serde_json::json!({"type":"object"}),
                    }],
                    tx,
                )
                .await
        });

        let mut tool_calls = Vec::new();
        let mut saw_done = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                ChatEvent::ToolCall(tc) => tool_calls.push(tc),
                ChatEvent::Done => saw_done = true,
                ChatEvent::Delta(_) => {}
                ChatEvent::Error(e) => panic!("stream error: {e}"),
            }
        }
        let result = handle.await.expect("task panicked");
        assert!(result.is_ok(), "chat_stream errored: {result:?}");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].name, "search");
        assert_eq!(tool_calls[0].arguments, "{\"q\":\"rust\"}");
        assert!(saw_done);
    }
}
