//! Native memory tools (#133, #137) — typed store/retrieve/list for agent memory.
//!
//! Why: Replaces ad-hoc shell access to memory backends with strongly-typed
//! per-operation tools. When a real `MemoryStore` + `Embedder` are injected,
//! these tools persist key/value entries into `Segment::AgentMemory` so they
//! survive process restarts. When no backend is wired (default constructor),
//! they degrade gracefully — `store_memory` / `retrieve_memory` /
//! `list_memory_keys` all return a structured `"memory store not available"`
//! payload so the agent can continue.
//! What: Three tools — `store_memory`, `retrieve_memory`, `list_memory_keys`.
//! Keys are prefixed with `kv:` internally so they don't collide with the
//! session/edge rows written by `MemoryGraph`. A sibling `kv-index` row
//! tracks the set of known keys (as a JSON array) so `list_memory_keys` can
//! enumerate them without a full-segment scan.
//! Test: Name / schema / happy-path / missing-input / graceful-degradation
//! cases in `tests` below.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::identity::CallerIdentity;
use crate::memory::Embedder;
use crate::memory::store::{MemoryStore, Segment};
use crate::tools::traits::{ToolExecutor, ToolResult};

/// Prefix applied to user-visible keys so kv entries don't collide with the
/// session / edge rows written by `MemoryGraph`.
const KV_PREFIX: &str = "kv:";
/// Reserved key that stores the JSON-array manifest of live kv keys.
const KV_INDEX_KEY: &str = "kv-index";

/// Backend bundle shared across the three memory tools.
///
/// Why: `store_memory` writes, `retrieve_memory` reads, and `list_memory_keys`
/// enumerates — all three want the same `Arc<dyn MemoryStore>` + `Arc<dyn
/// Embedder>`. Bundling them in one `Arc` keeps the ToolRegistry wiring flat.
/// The optional `session_id` is stamped into every stored payload so memories
/// can later be scoped to the originating session (workflow run / CTRL turn /
/// docs seed). When absent, payloads carry no `session_id` field.
#[derive(Clone)]
pub struct MemoryBackend {
    pub store: Arc<dyn MemoryStore>,
    pub embedder: Arc<dyn Embedder>,
    pub session_id: Option<String>,
}

impl MemoryBackend {
    /// Public constructor so production callers (PM loop / subprocess runner)
    /// can assemble a `MemoryBackend` outside the tools module once the
    /// session/code stores are initialized. Currently only tests invoke it;
    /// `#[allow(dead_code)]` keeps the strict build clean until the wiring
    /// in `main.rs` lands.
    #[allow(dead_code)]
    pub fn new(store: Arc<dyn MemoryStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            store,
            embedder,
            session_id: None,
        }
    }

    /// Builder: attach a session_id that will be stamped into every payload
    /// written through this backend. Used by `StoreMemoryTool` and inspected
    /// by `MemoryRecallTool` to resolve the `"current"` magic filter value.
    #[allow(dead_code)]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    fn zero_vec(&self) -> Vec<f32> {
        vec![0.0; self.embedder.dimension()]
    }

    fn kv_key(user_key: &str) -> String {
        format!("{KV_PREFIX}{user_key}")
    }

    /// Read the live kv-key manifest (JSON array of strings).
    async fn load_keys(&self) -> anyhow::Result<Vec<String>> {
        let raw = self.store.get(Segment::AgentMemory, KV_INDEX_KEY).await?;
        let Some(v) = raw else {
            return Ok(Vec::new());
        };
        let keys: Vec<String> = serde_json::from_value(v).unwrap_or_default();
        Ok(keys)
    }

    /// Persist the kv-key manifest.
    async fn save_keys(&self, keys: &[String]) -> anyhow::Result<()> {
        let vec = self.zero_vec();
        let payload = serde_json::to_value(keys)?;
        self.store
            .insert(Segment::AgentMemory, KV_INDEX_KEY, &vec, payload)
            .await
    }
}

/// `store_memory` — persist a memory entry keyed by a caller-chosen string.
pub struct StoreMemoryTool {
    backend: Option<MemoryBackend>,
    /// Test-only identity override. When `None`, the tool reads
    /// `CallerIdentity::from_env()` at execute time. (#193)
    identity_override: Option<CallerIdentity>,
}

impl StoreMemoryTool {
    pub fn new() -> Self {
        Self {
            backend: None,
            identity_override: None,
        }
    }

    pub fn with_backend(backend: MemoryBackend) -> Self {
        Self {
            backend: Some(backend),
            identity_override: None,
        }
    }

    /// Pin an explicit identity (test-only).
    #[allow(dead_code)]
    pub fn with_identity(mut self, identity: Option<CallerIdentity>) -> Self {
        self.identity_override = identity;
        self
    }
}

impl Default for StoreMemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for StoreMemoryTool {
    fn name(&self) -> &str {
        "store_memory"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "store_memory",
                "description": "Persist a memory entry keyed by a caller-chosen string. Idempotent (re-store overwrites). Optional `tag` selects a node in the memory taxonomy tree (default `memories/session`); use sub-tags like `memories/decision` or `memories/observation` to classify.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "content": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "tag": {
                            "type": "string",
                            "description": "Hierarchical taxonomy tag (default `memories/session`). Examples: `memories/session`, `memories/observation`, `memories/decision`."
                        },
                        "segment": {
                            "type": "string",
                            "description": "Memory tier to write into: context (stable architecture facts), brief (active goals), history (past decisions), or agent_memory (default catch-all).",
                            "enum": ["context", "brief", "history", "agent_memory"]
                        }
                    },
                    "required": ["key", "content"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(key) = args.get("key").and_then(Value::as_str) else {
            return ToolResult::err("store_memory: missing 'key'");
        };
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return ToolResult::err("store_memory: missing 'content'");
        };
        let mut tags: Vec<String> = args
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // #193: Auto-merge caller-identity tags so future scope filters can
        // reliably attribute the memory to its origin. Agents writing without
        // these tags would otherwise be invisible to their own scope-Agent
        // recall queries. Identity is read from the env-var bridge populated
        // by the harness at spawn time; callers that don't set OPEN_MPM_CALLER
        // (legacy/tests) get no auto-tags so behavior is unchanged.
        let identity = self
            .identity_override
            .clone()
            .or_else(CallerIdentity::from_env);
        if let Some(id) = identity.as_ref() {
            for t in id.auto_tags() {
                if !tags.contains(&t) {
                    tags.push(t);
                }
            }
        }
        // Hierarchical taxonomy tag (default `memories/session`). Agents can
        // pass subtags like `memories/decision` to classify within the tree.
        let tag = args
            .get("tag")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("memories/session")
            .to_string();

        // #277: optional `segment` arg routes the write to a specific memory
        // tier (Context / Brief / History / AgentMemory). Unknown values fall
        // back to `AgentMemory` so a typo doesn't fail the tool call. The
        // kv-index manifest stays on `AgentMemory` regardless so
        // `list_memory_keys` / `retrieve_memory` continue to work.
        let target_segment = args
            .get("segment")
            .and_then(Value::as_str)
            .and_then(Segment::from_name)
            .unwrap_or(Segment::AgentMemory);

        let Some(backend) = self.backend.as_ref() else {
            let out = json!({
                "stored": false,
                "key": key,
                "error": "memory store not available — StoreMemoryTool was constructed without a MemoryBackend."
            });
            return ToolResult::ok(out.to_string());
        };

        // Embed the content so later semantic searches can surface it.
        let vec = match backend.embedder.embed_single(content) {
            Ok(v) => v,
            Err(e) => {
                let out = json!({
                    "stored": false,
                    "key": key,
                    "error": format!("embed failed: {e}")
                });
                return ToolResult::ok(out.to_string());
            }
        };
        let storage_key = MemoryBackend::kv_key(key);
        let mut payload = json!({
            "key": key,
            "content": content,
            "tags": tags,
            "tag": tag,
            "created_at": chrono::Utc::now().to_rfc3339(),
        });
        if let Some(sid) = backend.session_id.as_deref() {
            payload["session_id"] = json!(sid);
        }
        if let Err(e) = backend
            .store
            .insert(target_segment, &storage_key, &vec, payload)
            .await
        {
            let out = json!({
                "stored": false,
                "key": key,
                "error": format!("insert failed: {e}")
            });
            return ToolResult::ok(out.to_string());
        }

        // Update the manifest so list_memory_keys can enumerate this key.
        let mut keys = backend.load_keys().await.unwrap_or_default();
        if !keys.iter().any(|k| k == key) {
            keys.push(key.to_string());
            if let Err(e) = backend.save_keys(&keys).await {
                // Don't fail the write — surface the degradation to the caller
                // so they know list_memory_keys may be stale.
                let out = json!({
                    "stored": true,
                    "key": key,
                    "warning": format!("key saved but manifest update failed: {e}")
                });
                return ToolResult::ok(out.to_string());
            }
        }

        let out = json!({
            "stored": true,
            "key": key,
        });
        ToolResult::ok(out.to_string())
    }
}

/// `retrieve_memory` — fetch a memory entry by key.
pub struct RetrieveMemoryTool {
    backend: Option<MemoryBackend>,
}

impl RetrieveMemoryTool {
    pub fn new() -> Self {
        Self { backend: None }
    }

    pub fn with_backend(backend: MemoryBackend) -> Self {
        Self {
            backend: Some(backend),
        }
    }
}

impl Default for RetrieveMemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for RetrieveMemoryTool {
    fn name(&self) -> &str {
        "retrieve_memory"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "retrieve_memory",
                "description": "Fetch a memory entry by key. Returns null content if not found.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"}
                    },
                    "required": ["key"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(key) = args.get("key").and_then(Value::as_str) else {
            return ToolResult::err("retrieve_memory: missing 'key'");
        };

        let Some(backend) = self.backend.as_ref() else {
            let out = json!({
                "key": key,
                "content": Value::Null,
                "error": "memory store not available — RetrieveMemoryTool was constructed without a MemoryBackend."
            });
            return ToolResult::ok(out.to_string());
        };

        let storage_key = MemoryBackend::kv_key(key);
        match backend.store.get(Segment::AgentMemory, &storage_key).await {
            Ok(Some(payload)) => {
                let out = json!({
                    "key": key,
                    "content": payload.get("content").cloned().unwrap_or(Value::Null),
                    "tags": payload.get("tags").cloned().unwrap_or_else(|| json!([])),
                });
                ToolResult::ok(out.to_string())
            }
            Ok(None) => {
                let out = json!({
                    "key": key,
                    "content": Value::Null,
                });
                ToolResult::ok(out.to_string())
            }
            Err(e) => {
                let out = json!({
                    "key": key,
                    "content": Value::Null,
                    "error": format!("retrieve failed: {e}")
                });
                ToolResult::ok(out.to_string())
            }
        }
    }
}

/// `list_memory_keys` — enumerate available memory keys, optionally filtered.
pub struct ListMemoryKeysTool {
    backend: Option<MemoryBackend>,
}

impl ListMemoryKeysTool {
    pub fn new() -> Self {
        Self { backend: None }
    }

    pub fn with_backend(backend: MemoryBackend) -> Self {
        Self {
            backend: Some(backend),
        }
    }
}

impl Default for ListMemoryKeysTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for ListMemoryKeysTool {
    fn name(&self) -> &str {
        "list_memory_keys"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "list_memory_keys",
                "description": "List memory keys, optionally filtered by a prefix.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prefix": {"type": "string", "description": "Optional key prefix filter."}
                    },
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let prefix = args
            .get("prefix")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let Some(backend) = self.backend.as_ref() else {
            let empty: Vec<String> = Vec::new();
            let out = json!({
                "keys": empty,
                "prefix": prefix,
                "error": "memory store not available — ListMemoryKeysTool was constructed without a MemoryBackend."
            });
            return ToolResult::ok(out.to_string());
        };

        match backend.load_keys().await {
            Ok(keys) => {
                let filtered: Vec<String> = keys
                    .into_iter()
                    .filter(|k| k.starts_with(&prefix))
                    .collect();
                let out = json!({
                    "keys": filtered,
                    "prefix": prefix,
                });
                ToolResult::ok(out.to_string())
            }
            Err(e) => {
                let empty: Vec<String> = Vec::new();
                let out = json!({
                    "keys": empty,
                    "prefix": prefix,
                    "error": format!("list failed: {e}")
                });
                ToolResult::ok(out.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;
    use crate::memory::store::MemoryResult;

    // Mock store that behaves as a simple key-value map (vector ignored).
    struct KvMockStore {
        inner: Mutex<HashMap<String, Value>>,
    }

    impl KvMockStore {
        fn new() -> Self {
            Self {
                inner: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl MemoryStore for KvMockStore {
        async fn insert(
            &self,
            _segment: Segment,
            id: &str,
            _vector: &[f32],
            payload: Value,
        ) -> anyhow::Result<()> {
            self.inner.lock().unwrap().insert(id.to_string(), payload);
            Ok(())
        }

        async fn search(
            &self,
            _segment: Segment,
            _query_vec: &[f32],
            _top_k: usize,
        ) -> anyhow::Result<Vec<MemoryResult>> {
            Ok(Vec::new())
        }

        async fn get(&self, _segment: Segment, id: &str) -> anyhow::Result<Option<Value>> {
            Ok(self.inner.lock().unwrap().get(id).cloned())
        }

        async fn delete(&self, _segment: Segment, id: &str) -> anyhow::Result<()> {
            self.inner.lock().unwrap().remove(id);
            Ok(())
        }
    }

    struct FixedEmbedder;
    impl Embedder for FixedEmbedder {
        fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.1_f32; 4]).collect())
        }
        fn embed_single(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.1_f32; 4])
        }
        fn dimension(&self) -> usize {
            4
        }
    }

    fn make_backend() -> MemoryBackend {
        let store: Arc<dyn MemoryStore> = Arc::new(KvMockStore::new());
        let embedder: Arc<dyn Embedder> = Arc::new(FixedEmbedder);
        MemoryBackend::new(store, embedder)
    }

    // ------- store_memory -------

    #[tokio::test]
    async fn store_memory_missing_key_errors() {
        let t = StoreMemoryTool::new();
        assert!(t.execute(json!({"content": "x"})).await.is_error());
    }

    #[tokio::test]
    async fn store_memory_missing_content_errors() {
        let t = StoreMemoryTool::new();
        assert!(t.execute(json!({"key": "k"})).await.is_error());
    }

    #[tokio::test]
    async fn store_memory_degrades_gracefully() {
        let t = StoreMemoryTool::new();
        let out = t.execute(json!({"key": "k", "content": "v"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["stored"], false);
        assert!(v["error"].is_string());
    }

    #[tokio::test]
    async fn store_memory_writes_to_backend() {
        let backend = make_backend();
        let t = StoreMemoryTool::with_backend(backend.clone());
        let out = t
            .execute(json!({"key": "hello", "content": "world", "tags": ["greeting"]}))
            .await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["stored"], true);
        assert_eq!(v["key"], "hello");

        // Directly verify the payload landed under the kv: prefix.
        let got = backend
            .store
            .get(Segment::AgentMemory, "kv:hello")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got["content"], "world");
        assert_eq!(got["tags"][0], "greeting");
    }

    #[tokio::test]
    async fn store_memory_stamps_session_id_when_set() {
        // When the backend has session_id configured, every stored payload
        // must include that session_id so future recall can filter by it.
        let backend = make_backend().with_session_id("session-xyz");
        let t = StoreMemoryTool::with_backend(backend.clone());
        t.execute(json!({"key": "k1", "content": "v1"})).await;

        let payload = backend
            .store
            .get(Segment::AgentMemory, "kv:k1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            payload.get("session_id").and_then(|v| v.as_str()),
            Some("session-xyz")
        );
        assert_eq!(
            payload.get("tag").and_then(|v| v.as_str()),
            Some("memories/session")
        );
        assert!(payload.get("created_at").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn store_memory_accepts_custom_taxonomy_tag() {
        // Agents can override the default `memories/session` with a sub-tag.
        let backend = make_backend();
        let t = StoreMemoryTool::with_backend(backend.clone());
        t.execute(json!({
            "key": "decision-1",
            "content": "use redb for storage",
            "tag": "memories/decision",
        }))
        .await;

        let payload = backend
            .store
            .get(Segment::AgentMemory, "kv:decision-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            payload.get("tag").and_then(|v| v.as_str()),
            Some("memories/decision")
        );
    }

    #[tokio::test]
    async fn store_memory_auto_tags_from_identity() {
        // #193: When the caller is an Agent, `store_memory` must merge the
        // identity's auto_tags (session/, project/, agent/) into the stored
        // payload. Tags supplied by the caller are preserved.
        let backend = make_backend();
        let identity = crate::identity::CallerIdentity::Agent {
            session_id: "sess-7".into(),
            project_id: "myproj".into(),
            agent_id: "research-agent".into(),
        };
        let t = StoreMemoryTool::with_backend(backend.clone()).with_identity(Some(identity));
        t.execute(json!({"key": "k1", "content": "c1", "tags": ["mine"]}))
            .await;

        let payload = backend
            .store
            .get(Segment::AgentMemory, "kv:k1")
            .await
            .unwrap()
            .unwrap();
        let tags: Vec<String> = serde_json::from_value(payload["tags"].clone()).unwrap();
        assert!(tags.contains(&"mine".to_string()));
        assert!(tags.contains(&"session/sess-7".to_string()));
        assert!(tags.contains(&"project/myproj".to_string()));
        assert!(tags.contains(&"agent/research-agent".to_string()));
    }

    #[tokio::test]
    async fn store_memory_auto_tags_for_ctrl_identity() {
        // CTRL identity contributes a single `scope/user` tag.
        let backend = make_backend();
        let t = StoreMemoryTool::with_backend(backend.clone())
            .with_identity(Some(crate::identity::CallerIdentity::Ctrl));
        t.execute(json!({"key": "k1", "content": "c1"})).await;

        let payload = backend
            .store
            .get(Segment::AgentMemory, "kv:k1")
            .await
            .unwrap()
            .unwrap();
        let tags: Vec<String> = serde_json::from_value(payload["tags"].clone()).unwrap();
        assert!(tags.contains(&"scope/user".to_string()));
    }

    #[tokio::test]
    async fn store_memory_omits_session_id_when_unset() {
        // Without a session_id on the backend, payloads should not carry one
        // (avoids accidentally tagging memories with empty strings).
        let backend = make_backend();
        let t = StoreMemoryTool::with_backend(backend.clone());
        t.execute(json!({"key": "k1", "content": "v1"})).await;

        let payload = backend
            .store
            .get(Segment::AgentMemory, "kv:k1")
            .await
            .unwrap()
            .unwrap();
        assert!(
            payload.get("session_id").is_none(),
            "no session_id should be stamped when backend has none"
        );
    }

    // ------- retrieve_memory -------

    #[tokio::test]
    async fn retrieve_memory_missing_key_errors() {
        let t = RetrieveMemoryTool::new();
        assert!(t.execute(json!({})).await.is_error());
    }

    #[tokio::test]
    async fn retrieve_memory_degrades_gracefully() {
        let t = RetrieveMemoryTool::new();
        let out = t.execute(json!({"key": "k"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["key"], "k");
        assert!(v["content"].is_null());
        assert!(v["error"].is_string());
    }

    #[tokio::test]
    async fn retrieve_memory_returns_stored_value() {
        let backend = make_backend();
        let store_tool = StoreMemoryTool::with_backend(backend.clone());
        store_tool
            .execute(json!({"key": "color", "content": "blue"}))
            .await;

        let retrieve = RetrieveMemoryTool::with_backend(backend);
        let out = retrieve.execute(json!({"key": "color"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["key"], "color");
        assert_eq!(v["content"], "blue");
    }

    #[tokio::test]
    async fn retrieve_memory_returns_null_for_unknown_key() {
        let backend = make_backend();
        let retrieve = RetrieveMemoryTool::with_backend(backend);
        let out = retrieve.execute(json!({"key": "missing"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["key"], "missing");
        assert!(v["content"].is_null());
    }

    // ------- list_memory_keys -------

    #[tokio::test]
    async fn list_memory_keys_degrades_gracefully() {
        let t = ListMemoryKeysTool::new();
        let out = t.execute(json!({})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert!(v["keys"].is_array());
        assert_eq!(v["keys"].as_array().unwrap().len(), 0);
        assert!(v["error"].is_string());
    }

    #[tokio::test]
    async fn list_memory_keys_enumerates_stored_keys() {
        let backend = make_backend();
        let store_tool = StoreMemoryTool::with_backend(backend.clone());
        store_tool
            .execute(json!({"key": "alpha", "content": "a"}))
            .await;
        store_tool
            .execute(json!({"key": "beta", "content": "b"}))
            .await;
        store_tool
            .execute(json!({"key": "alchemy", "content": "c"}))
            .await;

        let list = ListMemoryKeysTool::with_backend(backend);
        let out = list.execute(json!({})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        let keys: Vec<String> = serde_json::from_value(v["keys"].clone()).unwrap();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"alpha".to_string()));
        assert!(keys.contains(&"beta".to_string()));
        assert!(keys.contains(&"alchemy".to_string()));
    }

    #[tokio::test]
    async fn list_memory_keys_filters_by_prefix() {
        let backend = make_backend();
        let store_tool = StoreMemoryTool::with_backend(backend.clone());
        store_tool
            .execute(json!({"key": "alpha", "content": "a"}))
            .await;
        store_tool
            .execute(json!({"key": "beta", "content": "b"}))
            .await;
        store_tool
            .execute(json!({"key": "alchemy", "content": "c"}))
            .await;

        let list = ListMemoryKeysTool::with_backend(backend);
        let out = list.execute(json!({"prefix": "al"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        let keys: Vec<String> = serde_json::from_value(v["keys"].clone()).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|k| k.starts_with("al")));
        assert_eq!(v["prefix"], "al");
    }

    // ------- name/schema smoke tests -------

    /// Segment-tracking mock store (#277). Unlike `KvMockStore`, this records
    /// the `Segment` each insert lands in so tests can verify routing.
    struct SegmentTrackingStore {
        // (segment, id) -> payload
        inner: Mutex<HashMap<(Segment, String), Value>>,
    }

    impl SegmentTrackingStore {
        fn new() -> Self {
            Self {
                inner: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl MemoryStore for SegmentTrackingStore {
        async fn insert(
            &self,
            segment: Segment,
            id: &str,
            _vector: &[f32],
            payload: Value,
        ) -> anyhow::Result<()> {
            self.inner
                .lock()
                .unwrap()
                .insert((segment, id.to_string()), payload);
            Ok(())
        }
        async fn search(
            &self,
            _segment: Segment,
            _q: &[f32],
            _k: usize,
        ) -> anyhow::Result<Vec<MemoryResult>> {
            Ok(Vec::new())
        }
        async fn get(&self, segment: Segment, id: &str) -> anyhow::Result<Option<Value>> {
            Ok(self
                .inner
                .lock()
                .unwrap()
                .get(&(segment, id.to_string()))
                .cloned())
        }
        async fn delete(&self, segment: Segment, id: &str) -> anyhow::Result<()> {
            self.inner
                .lock()
                .unwrap()
                .remove(&(segment, id.to_string()));
            Ok(())
        }
    }

    fn make_segment_backend() -> (MemoryBackend, Arc<SegmentTrackingStore>) {
        let store = Arc::new(SegmentTrackingStore::new());
        let store_dyn: Arc<dyn MemoryStore> = store.clone();
        let embedder: Arc<dyn Embedder> = Arc::new(FixedEmbedder);
        (MemoryBackend::new(store_dyn, embedder), store)
    }

    /// #277: No `segment` arg → write lands in `AgentMemory` (legacy default).
    #[tokio::test]
    async fn store_memory_defaults_to_agent_memory_segment() {
        let (backend, raw) = make_segment_backend();
        let t = StoreMemoryTool::with_backend(backend);
        t.execute(json!({"key": "k1", "content": "v1"})).await;

        let map = raw.inner.lock().unwrap();
        assert!(
            map.contains_key(&(Segment::AgentMemory, "kv:k1".to_string())),
            "default segment must be AgentMemory; got keys: {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }

    /// #277: `segment: "context"` routes the write to `Segment::Context`.
    #[tokio::test]
    async fn store_memory_routes_to_context_segment() {
        let (backend, raw) = make_segment_backend();
        let t = StoreMemoryTool::with_backend(backend);
        t.execute(json!({
            "key": "arch-1",
            "content": "redb is the storage engine",
            "segment": "context"
        }))
        .await;

        let map = raw.inner.lock().unwrap();
        assert!(
            map.contains_key(&(Segment::Context, "kv:arch-1".to_string())),
            "segment=context should route to Context; got keys: {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }

    /// #277: `segment: "brief"` routes to `Segment::Brief`.
    #[tokio::test]
    async fn store_memory_routes_to_brief_segment() {
        let (backend, raw) = make_segment_backend();
        let t = StoreMemoryTool::with_backend(backend);
        t.execute(json!({
            "key": "goal-1",
            "content": "ship #277",
            "segment": "brief"
        }))
        .await;

        let map = raw.inner.lock().unwrap();
        assert!(
            map.contains_key(&(Segment::Brief, "kv:goal-1".to_string())),
            "segment=brief should route to Brief; got keys: {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }

    /// #277: Unknown segment value falls back to `AgentMemory` (graceful).
    #[tokio::test]
    async fn store_memory_unknown_segment_falls_back_to_agent_memory() {
        let (backend, raw) = make_segment_backend();
        let t = StoreMemoryTool::with_backend(backend);
        let out = t
            .execute(json!({
                "key": "k1",
                "content": "v1",
                "segment": "no_such_tier"
            }))
            .await;
        assert!(!out.is_error());

        let map = raw.inner.lock().unwrap();
        assert!(
            map.contains_key(&(Segment::AgentMemory, "kv:k1".to_string())),
            "unknown segment should fall back to AgentMemory; got keys: {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn store_memory_schema_names_tool() {
        let t = StoreMemoryTool::new();
        assert_eq!(t.name(), "store_memory");
        assert_eq!(t.schema()["function"]["name"], "store_memory");
    }

    #[test]
    fn retrieve_memory_schema_names_tool() {
        let t = RetrieveMemoryTool::new();
        assert_eq!(t.name(), "retrieve_memory");
        assert_eq!(t.schema()["function"]["name"], "retrieve_memory");
    }

    #[test]
    fn list_memory_keys_schema_names_tool() {
        let t = ListMemoryKeysTool::new();
        assert_eq!(t.name(), "list_memory_keys");
        assert_eq!(t.schema()["function"]["name"], "list_memory_keys");
    }
}
