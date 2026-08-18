use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

/// Timeout for draining error response bodies from a hung upstream.
const ERROR_BODY_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct ModelBackend {
    client: HttpClient,
    endpoints: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Message content — either plain text (the common case) or a multipart
/// array (used for vision requests with image_url parts).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// One part of a multipart message. Matches the OpenAI chat content-part
/// schema: `{"type":"text","text":"..."}` or
/// `{"type":"image_url","image_url":{"url":"..."}}`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant(content: Option<String>, tool_calls: Option<Vec<ToolCall>>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.map(MessageContent::Text),
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: String,
    ) -> Self {
        Self {
            role: "tool".into(),
            content: Some(MessageContent::Text(content)),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
        }
    }

    /// Return the message's text content if it is plain text. Returns None
    /// for multipart (vision) messages, which have no single text body.
    pub fn content_text(&self) -> Option<&str> {
        match self.content.as_ref()? {
            MessageContent::Text(s) => Some(s),
            MessageContent::Parts(_) => None,
        }
    }

    /// Owned-text convenience — same semantics as `content_text` but
    /// returns a cloned `String` for callers that need ownership.
    pub fn content_text_owned(&self) -> Option<String> {
        self.content_text().map(|s| s.to_string())
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    pub type_field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
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
    #[allow(dead_code)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Tokens served from the provider's prefix cache.
    /// Normalised during deserialization from DeepSeek (top-level
    /// `prompt_cache_hit_tokens`) and OpenAI (nested
    /// `prompt_tokens_details.cached_tokens`).
    #[serde(default)]
    pub cache_hit_tokens: u32,
    /// Tokens NOT in cache — full-recompute prompt tokens.
    #[serde(default)]
    pub cache_miss_tokens: u32,
}

/// Provider-specific usage details that live inside `usage.prompt_tokens_details`.
#[derive(Debug, Clone, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

/// Raw deserialization target — captures every cache-related field from
/// every provider format so `Usage` can normalise them.
#[derive(Debug, Clone, Deserialize)]
struct RawUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    #[serde(default)]
    prompt_cache_hit_tokens: u32,
    #[serde(default)]
    prompt_cache_miss_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

impl<'de> Deserialize<'de> for Usage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawUsage::deserialize(deserializer)?;
        // DeepSeek puts cache hit/miss at top level; OpenAI/MiMo nest them.
        // Prefer the DeepSeek fields when present; otherwise fall back to
        // the nested cached_tokens (which represents cache hits).
        let has_top_level = raw.prompt_cache_hit_tokens > 0 || raw.prompt_cache_miss_tokens > 0;
        let (hit, miss) = if has_top_level {
            (raw.prompt_cache_hit_tokens, raw.prompt_cache_miss_tokens)
        } else if let Some(details) = &raw.prompt_tokens_details {
            let cached = details.cached_tokens;
            (cached, raw.prompt_tokens.saturating_sub(cached))
        } else {
            (0, 0)
        };
        Ok(Usage {
            prompt_tokens: raw.prompt_tokens,
            completion_tokens: raw.completion_tokens,
            total_tokens: raw.total_tokens,
            cache_hit_tokens: hit,
            cache_miss_tokens: miss,
        })
    }
}

// ── Streaming types ──

/// Accumulated result of one streaming completion round.
pub struct StreamResult {
    pub content: String,
    #[allow(dead_code)]
    pub reasoning_content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    #[allow(dead_code)]
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
    #[serde(default)]
    reasoning_content: Option<String>,
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
    /// Reasoning/thinking delta (DeepSeek-R1 style)
    Reasoning(String),
    /// Stream finished — full accumulated result
    Done(StreamResult),
    /// Error mid-stream
    Error(String),
}

impl ModelBackend {
    pub fn new(endpoints: HashMap<String, String>) -> Self {
        let client = HttpClient::builder()
            .timeout(Duration::from_secs(1800))
            .build()
            .expect("http client");
        Self { client, endpoints }
    }

    fn get_endpoint(&self, model: &str) -> Result<&str, String> {
        self.endpoints
            .get(model)
            .map(|s| s.as_str())
            .ok_or_else(|| format!("model {model} not found in backends"))
    }

    /// Drain an error response body with a deadline, so a hung upstream
    /// doesn't wedge the retry loop. Returns the (possibly truncated) body.
    async fn drain_error_body(resp: reqwest::Response) -> String {
        match tokio::time::timeout(ERROR_BODY_DRAIN_TIMEOUT, resp.text()).await {
            Ok(Ok(body)) => body,
            _ => "(error body unavailable)".to_string(),
        }
    }

    /// Non-streaming completion (titles, summarization, cron jobs, health probes).
    pub async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, String> {
        let endpoint = self.get_endpoint(&req.model)?.to_string();
        let resp = self
            .client
            .post(format!("{endpoint}/chat/completions"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = Self::drain_error_body(resp).await;
            return Err(format!(
                "{status}: {}",
                body.chars().take(500).collect::<String>()
            ));
        }
        let parsed = resp
            .json::<ChatResponse>()
            .await
            .map_err(|e| format!("parse error: {e}"))?;
        Ok(parsed)
    }

    /// Streaming completion. Sends events through the channel as they arrive.
    /// Accumulates tool calls from delta fragments. Returns when stream closes.
    pub async fn chat_stream(
        &self,
        mut req: ChatRequest,
        tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) {
        let endpoint = match self.get_endpoint(&req.model) {
            Ok(ep) => ep.to_string(),
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(e));
                return;
            }
        };
        req.stream = Some(true);
        req.stream_options = Some(StreamOptions {
            include_usage: true,
        });

        let result = self.stream_inner(&endpoint, req, tx.clone()).await;
        if let Err(e) = result {
            let _ = tx.send(StreamEvent::Error(e));
        }
    }

    async fn stream_inner(
        &self,
        endpoint: &str,
        req: ChatRequest,
        tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), String> {
        use futures_util::StreamExt;

        let resp = self
            .client
            .post(format!("{endpoint}/chat/completions"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("stream request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = Self::drain_error_body(resp).await;
            return Err(format!(
                "{status}: {}",
                body.chars().take(500).collect::<String>()
            ));
        }

        let mut content = String::new();
        let mut reasoning_content = String::new();
        let mut usage: Option<Usage> = None;
        let mut finish_reason: Option<String> = None;
        let mut tool_accum: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut saw_done_marker = false;

        let mut buffer: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();

        const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
        loop {
            let chunk = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                Ok(Some(chunk)) => chunk.map_err(|e| format!("stream read: {e}"))?,
                Ok(None) => break,
                Err(_) => {
                    return Err(format!(
                        "stream idle for {}s — backend hung",
                        IDLE_TIMEOUT.as_secs()
                    ))
                }
            };
            let bytes = chunk;
            buffer.extend_from_slice(&bytes);

            loop {
                let end = buffer
                    .windows(2)
                    .position(|w| w == b"\n\n")
                    .map(|p| (p, 2usize))
                    .or_else(|| {
                        buffer
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                            .map(|p| (p, 4usize))
                    });
                let Some((pos, sep_len)) = end else { break };
                let event_bytes: Vec<u8> = buffer.drain(..pos + sep_len).collect();
                let event_text = String::from_utf8_lossy(&event_bytes);

                for line in event_text.lines() {
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        saw_done_marker = true;
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
                        if let Some(r) = choice.delta.reasoning_content {
                            if !r.is_empty() {
                                reasoning_content.push_str(&r);
                                let _ = tx.send(StreamEvent::Reasoning(r));
                            }
                        }
                        if let Some(tcs) = choice.delta.tool_calls {
                            for tc in tcs {
                                let idx = tc.index.unwrap_or(0);
                                let entry = tool_accum.entry(idx).or_insert_with(|| {
                                    (String::new(), String::new(), String::new())
                                });
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

        // A stream that ends with no completion marker was truncated —
        // treat it as an error so the retry loop regenerates instead of
        // shipping partial content as final.
        if !saw_done_marker && finish_reason.is_none() {
            return Err(format!(
                "stream ended without [DONE]/finish_reason (truncated — {} bytes of content, {} tool call(s) accumulated)",
                content.len(),
                tool_accum.len()
            ));
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
            reasoning_content,
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

    pub fn list_models(&self) -> Result<Vec<String>, String> {
        Ok(self.endpoints.keys().cloned().collect())
    }

    /// Quick complexity classification. Sends a tiny prompt (~200 tokens in,
    /// ~10 tokens out) to determine Simple/Medium/Complex/Critical.
    /// Expects the Supra Router model to respond with domain/complexity/route.
    pub async fn classify_query(
        &self,
        model: &str,
        query: &str,
    ) -> Option<temple_protocol::ComplexityClass> {
        use temple_protocol::ComplexityClass;

        let req = ChatRequest {
            model: model.to_string(),
            messages: vec![ChatMessage::user(format!("Task: {query}\nAnalysis: "))],
            tools: None,
            stream: Some(false),
            stream_options: None,
            max_tokens: Some(128),
            temperature: Some(0.0),
            chat_template_kwargs: Some(serde_json::json!({"enable_thinking": false})),
            ..Default::default()
        };

        let resp = self.chat(req).await.ok()?;
        let choice = resp.choices.first()?;
        let content = choice.message.content_text()?.trim().to_lowercase();

        // Supra Router output: "Domain: ... | Complexity: N | Route: small model/big model"
        if let Some(cap) = content
            .split("complexity:")
            .nth(1)
            .or_else(|| content.split("Complexity:").nth(1))
        {
            let level = cap.trim().chars().next().and_then(|c| c.to_digit(10));
            match level {
                Some(1..=2) => Some(ComplexityClass::Simple),
                Some(3) => Some(ComplexityClass::Medium),
                Some(4) => Some(ComplexityClass::Complex),
                Some(5) => Some(ComplexityClass::Critical),
                _ => {
                    if content.contains("Route: small model")
                        || content.contains("route: small model")
                    {
                        Some(ComplexityClass::Simple)
                    } else {
                        Some(ComplexityClass::Medium)
                    }
                }
            }
        } else {
            // Fallback on route field
            if content.contains("Route: small model") || content.contains("route: small model") {
                Some(ComplexityClass::Simple)
            } else {
                Some(ComplexityClass::Medium)
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SamplingPreset {
    Deterministic,
    Coding,
    General,
    Creative,
}

pub struct SamplingParams {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
}

impl std::str::FromStr for SamplingPreset {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "deterministic" => Self::Deterministic,
            "coding" => Self::Coding,
            "creative" => Self::Creative,
            _ => Self::General,
        })
    }
}

impl SamplingPreset {
    pub fn params(&self) -> SamplingParams {
        match self {
            Self::Deterministic => SamplingParams {
                temperature: 0.0,
                top_p: 1.0,
                top_k: 1,
            },
            Self::Coding => SamplingParams {
                temperature: 0.1,
                top_p: 0.95,
                top_k: 40,
            },
            Self::General => SamplingParams {
                temperature: 0.7,
                top_p: 0.95,
                top_k: 40,
            },
            Self::Creative => SamplingParams {
                temperature: 1.0,
                top_p: 0.95,
                top_k: 80,
            },
        }
    }
}
