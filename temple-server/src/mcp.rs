use reqwest::Client as HttpClient;
use serde_json::{json, Value};
use std::time::Duration;

/// MCP client that talks to litellm's streamable-HTTP MCP endpoints.
/// Each configured server (fetch, searxng, nixos, arxiv, context7) is
/// exposed at /mcp/{server} and speaks JSON-RPC 2.0 over SSE.
pub struct McpClient {
    client: HttpClient,
    base_url: String,
    api_key: String,
}

impl McpClient {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let client = HttpClient::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("http client");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }

    /// Execute an MCP tool. `tool_name` is the namespaced name as exposed
    /// by litellm, e.g. "fetch-fetch", "searxng-web_search", "nixos-nix".
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<String, String> {
        let (server, bare_name) = tool_name
            .split_once('-')
            .ok_or_else(|| format!("malformed MCP tool name: {tool_name}"))?;

        let endpoint = format!("{}/mcp/{}", self.base_url, server);

        // 1. initialize
        let (session_id, _) = self
            .rpc(
                &endpoint,
                None,
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "temple", "version": "0.1"},
                }),
            )
            .await?;

        // 2. initialized notification (no id → notification)
        self.notify(
            &endpoint,
            session_id.as_deref(),
            "notifications/initialized",
        )
        .await?;

        // 3. tools/call
        let (_, result) = self
            .rpc(
                &endpoint,
                session_id.as_deref(),
                "tools/call",
                json!({
                    "name": bare_name,
                    "arguments": arguments,
                }),
            )
            .await?;

        // Extract text content from result
        if let Some(result) = result {
            if result.get("isError").and_then(|e| e.as_bool()) == Some(true) {
                let msg = extract_text(&result);
                return Err(format!("MCP tool error: {msg}"));
            }
            return Ok(extract_text(&result));
        }
        Err("MCP: empty result".into())
    }

    async fn rpc(
        &self,
        endpoint: &str,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<(Option<String>, Option<Value>), String> {
        let id: u64 = rand::random();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut req = self
            .client
            .post(endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&body);

        if let Some(sid) = session_id {
            req = req.header("mcp-session-id", sid);
        }

        let resp = req.send().await.map_err(|e| format!("MCP request: {e}"))?;

        let new_session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if !resp.status().is_success() {
            return Err(format!("MCP HTTP {}", resp.status()));
        }

        let text = resp.text().await.map_err(|e| format!("MCP read: {e}"))?;
        let parsed = parse_sse_json_for_id(&text, id)?;

        if let Some(err) = parsed.get("error") {
            return Err(format!("MCP RPC error: {err}"));
        }

        Ok((
            new_session.or_else(|| session_id.map(|s| s.to_string())),
            parsed.get("result").cloned(),
        ))
    }

    async fn notify(
        &self,
        endpoint: &str,
        session_id: Option<&str>,
        method: &str,
    ) -> Result<(), String> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
        });

        let mut req = self
            .client
            .post(endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&body);

        if let Some(sid) = session_id {
            req = req.header("mcp-session-id", sid);
        }

        req.send().await.map_err(|e| format!("MCP notify: {e}"))?;
        Ok(())
    }
}

/// Parse an SSE body and return the JSON-RPC message whose `id` matches
/// our request. Streamable-HTTP MCP servers can emit progress
/// notifications and other messages before the actual result — taking the
/// first parseable line returns the WRONG payload (e.g. a notification
/// with no `result`, yielding a spurious "empty result" error).
fn parse_sse_json_for_id(text: &str, id: u64) -> Result<Value, String> {
    // SSE format: "event: message\ndata: {...}\n\n"
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                    return Ok(v);
                }
            }
        }
    }
    // Maybe it's raw JSON without SSE framing
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
            return Ok(v);
        }
    }
    Err(format!(
        "MCP: no response for request id in body: {}",
        text.chars().take(200).collect::<String>()
    ))
}

/// Extract human-readable text from an MCP tool result.
fn extract_text(result: &Value) -> String {
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        let mut out = String::new();
        for item in content {
            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
        if !out.is_empty() {
            return out.trim_end().to_string();
        }
    }
    // Fallback: dump whole result
    serde_json::to_string_pretty(result).unwrap_or_default()
}
