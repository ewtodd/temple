use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct LiteLLM {
    client: HttpClient,
    base_url: String,
    api_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant(content: Option<String>, tool_calls: Option<Vec<ToolCall>>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, name: impl Into<String>, content: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub type_field: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub type_field: String,
    pub function: ToolFunctionDef,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatResponse {
    pub id: Option<String>,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

// ── Streaming types ──

/// Accumulated result of one streaming completion round.
pub struct StreamResult {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    index: Option<usize>,
    id: Option<String>,
    function: Option<DeltaToolFunction>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolFunction {
    name: Option<String>,
    arguments: Option<String>,
}

/// Events emitted while streaming.
pub enum StreamEvent {
    /// Content delta for live display
    Delta(String),
    /// Stream finished — full accumulated result
    Done(StreamResult),
    /// Error mid-stream
    Error(String),
}

#[derive(Debug, Deserialize)]
pub struct ModelListResponse {
    pub data: Vec<ModelListItem>,
}

#[derive(Debug, Deserialize)]
pub struct ModelListItem {
    pub id: String,
}

impl LiteLLM {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let client = HttpClient::builder()
            .timeout(Duration::from_secs(1800))
            .build()
            .expect("http client");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header;
        let mut h = header::HeaderMap::new();
        h.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", self.api_key).parse().unwrap(),
        );
        h
    }

    /// Non-streaming completion (used for keep-alive pings).
    pub async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, String> {
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .headers(self.headers())
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("litellm {status}: {}", body.chars().take(500).collect::<String>()));
        }
        resp.json::<ChatResponse>()
            .await
            .map_err(|e| format!("parse error: {e}"))
    }

    /// Streaming completion. Sends events through the channel as they arrive.
    /// Accumulates tool calls from delta fragments. Returns when stream closes.
    pub async fn chat_stream(
        &self,
        mut req: ChatRequest,
        tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) {
        req.stream = Some(true);
        req.stream_options = Some(StreamOptions { include_usage: true });

        let result = self.stream_inner(req, tx.clone()).await;
        if let Err(e) = result {
            let _ = tx.send(StreamEvent::Error(e));
        }
    }

    async fn stream_inner(
        &self,
        req: ChatRequest,
        tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), String> {
        use futures_util::StreamExt;

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .headers(self.headers())
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("stream request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("litellm {status}: {}", body.chars().take(500).collect::<String>()));
        }

        let mut content = String::new();
        let mut usage: Option<Usage> = None;
        let mut finish_reason: Option<String> = None;
        // tool call accumulators: index -> (id, name, args)
        let mut tool_accum: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();

        let mut buffer: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();

        // Idle timeout per chunk: a hung backend (model host down, proxy
        // stuck) otherwise holds the connection for the full 1800s client
        // timeout — and with the global request queue, wedges everyone.
        // Timing out errors the attempt so the caller's retry loop runs.
        const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
        loop {
            let chunk = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                Ok(Some(chunk)) => chunk.map_err(|e| format!("stream read: {e}"))?,
                Ok(None) => break,
                Err(_) => return Err(format!(
                    "stream idle for {}s — backend hung",
                    IDLE_TIMEOUT.as_secs()
                )),
            };
            let bytes = chunk;
            buffer.extend_from_slice(&bytes);

            // Process complete SSE events (separated by \n\n). Split on raw
            // bytes — decoding each chunk with from_utf8_lossy corrupts
            // multi-byte UTF-8 that straddles a chunk boundary (which also
            // breaks tool-call argument JSON).
            while let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
                let event_bytes: Vec<u8> = buffer.drain(..end + 2).collect();
                let event_text = String::from_utf8_lossy(&event_bytes);

                for line in event_text.lines() {
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        continue;
                    }
                    let Ok(parsed) = serde_json::from_str::<StreamChunk>(data) else {
                        continue;
                    };

                    if let Some(u) = parsed.usage {
                        usage = Some(u);
                    }

                    for choice in parsed.choices {
                        if let Some(fr) = choice.finish_reason {
                            finish_reason = Some(fr);
                        }
                        if let Some(c) = choice.delta.content {
                            if !c.is_empty() {
                                content.push_str(&c);
                                let _ = tx.send(StreamEvent::Delta(c));
                            }
                        }
                        if let Some(tcs) = choice.delta.tool_calls {
                            for tc in tcs {
                                let idx = tc.index.unwrap_or(0);
                                let entry = tool_accum
                                    .entry(idx)
                                    .or_insert_with(|| (String::new(), String::new(), String::new()));
                                if let Some(id) = tc.id {
                                    entry.0 = id;
                                }
                                if let Some(f) = tc.function {
                                    if let Some(n) = f.name {
                                        entry.1 = n;
                                    }
                                    if let Some(a) = f.arguments {
                                        entry.2.push_str(&a);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let tool_calls: Vec<ToolCall> = tool_accum
            .into_iter()
            .map(|(_, (id, name, args))| ToolCall {
                id: if id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4().simple())
                } else {
                    id
                },
                type_field: "function".into(),
                function: ToolCallFunction {
                    name,
                    arguments: args,
                },
            })
            .collect();

        let _ = tx.send(StreamEvent::Done(StreamResult {
            content,
            tool_calls,
            usage,
            finish_reason,
        }));

        Ok(())
    }

    /// Recover possibly-mangled tool call arguments.
    pub fn recover_tool_call(arguments: &str) -> Option<Value> {
        if let Ok(v) = serde_json::from_str::<Value>(arguments) {
            return Some(v);
        }
        if let Some(start) = arguments.find('{') {
            if let Some(end) = arguments[start..].rfind('}') {
                if let Ok(v) = serde_json::from_str::<Value>(&arguments[start..=start + end]) {
                    return Some(v);
                }
            }
        }
        let cleaned = arguments.trim().trim_matches('"');
        if !cleaned.is_empty() {
            return Some(json!({ "input": cleaned }));
        }
        None
    }

    pub async fn list_models(&self) -> Result<Vec<String>, String> {
        let resp = self
            .client
            .get(format!("{}/v1/models", self.base_url))
            .headers(self.headers())
            .send()
            .await
            .map_err(|e| format!("models request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("models error: {}", resp.status()));
        }
        let data: ModelListResponse = resp
            .json()
            .await
            .map_err(|e| format!("models parse: {e}"))?;
        Ok(data.data.into_iter().map(|m| m.id).collect())
    }

    /// List MCP tools exposed by the proxy (OpenAI tool schema format).
    pub async fn list_mcp_tools(&self) -> Result<Vec<ToolDefinition>, String> {
        let resp = self
            .client
            .get(format!("{}/v1/mcp/tools", self.base_url))
            .headers(self.headers())
            .send()
            .await
            .map_err(|e| format!("mcp tools request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("mcp tools error: {}", resp.status()));
        }

        let body: Value = resp.json().await.map_err(|e| format!("mcp parse: {e}"))?;
        let mut out = Vec::new();
        if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
            for t in tools {
                let name = t.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                let desc = t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default();
                let params = t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}}));
                if !name.is_empty() {
                    out.push(ToolDefinition {
                        type_field: "function".into(),
                        function: ToolFunctionDef {
                            name: name.to_string(),
                            description: desc.chars().take(1024).collect(),
                            parameters: params,
                        },
                    });
                }
            }
        }
        Ok(out)
    }
}
