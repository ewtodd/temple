use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// A single MCP server process connected over stdio JSON-RPC 2.0.
pub struct McpClient {
    name: String,
    child: Child,
    next_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: u64,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

impl McpClient {
    pub async fn connect(name: &str, command: &str, args: &[String]) -> Result<Self, String> {
        let child = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("mcp/{name}: spawn failed: {e}"))?;

        let mut client = Self {
            name: name.to_string(),
            child,
            next_id: 1,
        };

        let init_result = client.initialize().await;
        if let Err(e) = init_result {
            let _ = client.child.kill().await;
            return Err(e);
        }

        tracing::info!("mcp/{name}: connected");
        Ok(client)
    }

    async fn initialize(&mut self) -> Result<Value, String> {
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "temple",
                "version": "0.1.0"
            }
        });
        self.call("initialize", Some(params)).await
    }

    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        let result = self.call("tools/list", None).await?;
        let tools: Vec<McpTool> = serde_json::from_value(
            result
                .get("tools")
                .cloned()
                .unwrap_or(Value::Array(Vec::new())),
        )
        .map_err(|e| format!("mcp/{}: parse tools/list: {e}", self.name))?;
        Ok(tools)
    }

    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<String, String> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        });
        let result = self.call("tools/call", Some(params)).await?;
        let content = result
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| format!("mcp/{}: {tool_name}: no content in response", self.name))?;
        let mut out = String::new();
        for item in content {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                out.push_str(text);
            }
        }
        Ok(out)
    }

    async fn call(&mut self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;

        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };

        let mut req_json = serde_json::to_string(&req)
            .map_err(|e| format!("mcp/{}: serialize: {e}", self.name))?;
        req_json.push('\n');

        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| format!("mcp/{}: stdin not available", self.name))?;
        stdin
            .write_all(req_json.as_bytes())
            .await
            .map_err(|e| format!("mcp/{}: write: {e}", self.name))?;

        let stdout = self
            .child
            .stdout
            .take()
            .ok_or_else(|| format!("mcp/{}: stdout not available", self.name))?;
        let mut reader = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(Duration::from_secs(60), reader.next_line())
            .await
            .map_err(|_| format!("mcp/{}: {method}: timeout", self.name))?
            .map_err(|e| format!("mcp/{}: read: {e}", self.name))?
            .ok_or_else(|| format!("mcp/{}: {method}: EOF", self.name))?;

        // Put stdout back for next call
        self.child.stdout = Some(reader.into_inner().into_inner());

        let resp: JsonRpcResponse = serde_json::from_str(&line)
            .map_err(|e| format!("mcp/{}: parse: {e} (line: {line:.200})", self.name))?;

        if let Some(err) = resp.error {
            return Err(format!("mcp/{}: {method}: {}", self.name, err.message));
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }
}
