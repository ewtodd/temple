use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::config::SignalConfig;

/// Signal bot client that talks to signal-cli's JSON-RPC daemon over TCP.
///
/// signal-cli daemon mode (`signal-cli -a +NUMBER daemon --tcp=HOST:PORT`)
/// accepts JSON-RPC 2.0 requests and pushes incoming message notifications
/// over the same socket. Each TCP connection is independent — outbound
/// sends open a fresh connection, while the inbound loop maintains a
/// persistent connection to receive notifications.
pub struct Signal {
    config: SignalConfig,
}

impl Signal {
    pub fn new(config: &SignalConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }

    #[allow(dead_code)] // Used by main to gate the receive loop at startup.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// One JSON-RPC call over a fresh TCP connection: write the request
    /// line, read the response line, surface any error field.
    async fn rpc_call(&self, method: &str, params: Value) -> Result<(), String> {
        self.rpc_call_returning(method, params).await.map(|_| ())
    }

    /// Same as `rpc_call` but returns the parsed `result` field. Used by
    /// methods like `getAttachment` that return data to the caller.
    ///
    /// signal-cli pushes `receive` notifications to EVERY connected client,
    /// so a fresh outbound connection can receive a notification before the
    /// actual RPC response. We read lines until the response with OUR id
    /// arrives — anything else (notifications, other ids) is skipped.
    async fn rpc_call_returning(&self, method: &str, params: Value) -> Result<Value, String> {
        if !self.config.enabled {
            return Ok(Value::Null);
        }

        let id: u64 = rand::random();
        let mut stream = TcpStream::connect(&self.config.socket_addr)
            .await
            .map_err(|e| format!("signal connect: {e}"))?;

        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });
        let line = format!("{}\n", req);
        stream
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("signal write: {e}"))?;

        let mut reader = BufReader::new(stream);
        // Cap the hunt at a few lines — a healthy daemon answers promptly;
        // don't loop forever if the response never comes.
        for _ in 0..64 {
            let mut response_line = String::new();
            let n = tokio::time::timeout(
                Duration::from_secs(30),
                reader.read_line(&mut response_line),
            )
            .await
            .map_err(|_| format!("{method}: timed out waiting for response"))?
            .map_err(|e| format!("signal read: {e}"))?;
            if n == 0 {
                return Err(format!("{method}: connection closed before response"));
            }
            let Ok(resp) = serde_json::from_str::<Value>(&response_line) else {
                continue; // unparseable line — not our response
            };
            // Notifications have a `method` field and no matching `id`.
            if resp.get("id").and_then(|v| v.as_u64()) != Some(id) {
                continue;
            }
            if let Some(err) = resp.get("error") {
                return Err(format!("{method} error: {err}"));
            }
            return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
        }
        Err(format!("{method}: no response after 64 lines"))
    }

    /// Send a read receipt for a message timestamp. This marks the message
    /// as "read" on the sender's phone.
    pub async fn send_read_receipt(&self, recipient: &str, timestamp: u64) -> Result<(), String> {
        self.rpc_call(
            "sendReceipt",
            json!({
                "recipient": recipient,
                "targetTimestamp": [timestamp],
                "type": "read",
            }),
        )
        .await
    }

    /// Fetch an already-downloaded attachment from signal-cli's storage by
    /// its id (the same `id` field that appears in the `receive`
    /// notification's attachment metadata). Returns the raw file bytes.
    ///
    /// signal-cli's `getAttachment` JSON-RPC method reads the file from
    /// its own attachment store (`<data-dir>/attachments/<sanitized-id>`)
    /// and returns it base64-encoded. We decode to bytes here so callers
    /// don't have to. This avoids any need to SSH to the signal host or
    /// guess the on-disk path — signal-cli knows where it put the file.
    pub async fn get_attachment(&self, id: &str) -> Result<Vec<u8>, String> {
        use base64::Engine;
        /// Hard cap on decoded attachment size — an unbounded base64
        /// decode of a huge attachment OOMs the server. 20 MiB is far
        /// beyond any photo Signal will deliver for vision description.
        const MAX_ATTACHMENT_BYTES: usize = 20 << 20;
        let result = self
            .rpc_call_returning(
                "getAttachment",
                json!({
                    "id": id,
                }),
            )
            .await?;
        let b64 = result
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| format!("getAttachment: missing 'data' field in response: {result}"))?;
        // Reject early on encoded length (≈4/3 of decoded) before decoding.
        if b64.len() > MAX_ATTACHMENT_BYTES * 4 / 3 + 8 {
            return Err(format!(
                "getAttachment: attachment too large (>{} MiB)",
                MAX_ATTACHMENT_BYTES >> 20
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("getAttachment: base64 decode failed: {e}"))?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "getAttachment: attachment too large (>{} MiB)",
                MAX_ATTACHMENT_BYTES >> 20
            ));
        }
        Ok(bytes)
    }

    /// Send a typing indicator. Shows the typing bubble for ~15 seconds;
    /// re-send periodically to keep it alive during long generations.
    pub async fn send_typing(&self, recipient: &str) -> Result<(), String> {
        self.rpc_call(
            "sendTyping",
            json!({
                "recipient": [recipient],
            }),
        )
        .await
    }

    /// Typing indicator for a group chat.
    pub async fn send_typing_group(&self, group_id: &str) -> Result<(), String> {
        self.rpc_call(
            "sendTyping",
            json!({
                "groupId": group_id,
            }),
        )
        .await
    }

    /// Send a message to a recipient.
    pub async fn send(&self, recipient: &str, message: &str) -> Result<(), String> {
        self.rpc_call(
            "send",
            json!({
                "recipient": [recipient],
                "message": message,
            }),
        )
        .await
    }

    /// Send a message to a group.
    pub async fn send_group(&self, group_id: &str, message: &str) -> Result<(), String> {
        self.rpc_call(
            "send",
            json!({
                "groupId": group_id,
                "message": message,
            }),
        )
        .await
    }

    /// Send a message, splitting into multiple messages if it's long.
    /// Signal handles long messages but splitting gives better UX on mobile.
    /// Splits on paragraph boundaries when possible, ~1800 chars per message.
    pub async fn send_multi(&self, recipient: &str, message: &str) -> Result<(), String> {
        self.send_multi_impl(recipient, message, false).await
    }

    /// send_multi for a group chat.
    pub async fn send_multi_group(&self, group_id: &str, message: &str) -> Result<(), String> {
        self.send_multi_impl(group_id, message, true).await
    }

    async fn send_multi_impl(
        &self,
        target: &str,
        message: &str,
        is_group: bool,
    ) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }

        const MAX_LEN: usize = 1800;
        let chars: Vec<char> = message.chars().collect();

        if chars.len() <= MAX_LEN {
            return if is_group {
                self.send_group(target, message).await
            } else {
                self.send(target, message).await
            };
        }

        // Split on paragraph boundaries (\n\n) near MAX_LEN
        let mut chunks: Vec<String> = Vec::new();
        let mut start = 0;
        while start < chars.len() {
            let end = (start + MAX_LEN).min(chars.len());
            if end == chars.len() {
                chunks.push(chars[start..end].iter().collect());
                break;
            }
            // Look for \n\n near the split point (search backwards)
            let mut split_at = end;
            for i in (start.max(end.saturating_sub(200))..end).rev() {
                if i > 0 && chars[i - 1] == '\n' && chars[i] == '\n' {
                    split_at = i + 1;
                    break;
                }
            }
            chunks.push(chars[start..split_at].iter().collect());
            start = split_at;
        }

        for (i, chunk) in chunks.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            if is_group {
                self.send_group(target, chunk).await?;
            } else {
                self.send(target, chunk).await?;
            }
        }
        Ok(())
    }

    /// Convenience: send a notification with a title prefix.
    /// Sends to the default recipient (or all recipients if multiple are set).
    #[allow(dead_code)] // Reserved for admin notifications.
    pub async fn notify(&self, title: &str, body: &str) -> Result<(), String> {
        let msg = if title.is_empty() {
            body.to_string()
        } else {
            format!("*{title}*\n\n{body}")
        };
        self.send(&self.config.default_recipient.clone(), &msg)
            .await
    }

    /// Send a notification to a specific recipient.
    pub async fn notify_recipient(
        &self,
        recipient: &str,
        title: &str,
        body: &str,
    ) -> Result<(), String> {
        let msg = if title.is_empty() {
            body.to_string()
        } else {
            format!("*{title}*\n\n{body}")
        };
        self.send(recipient, &msg).await
    }

    /// Run the inbound receive loop. Maintains a persistent TCP connection
    /// to signal-cli daemon, reads incoming message notifications, and calls
    /// the handler for each message from an allowed sender.
    /// Handler args: (sender, message, timestamp, group_id).
    ///
    /// Returns when the connection is permanently lost (after retry backoff).
    pub async fn receive_loop(
        &self,
        handler: SignalHandler,
    ) {
        if !self.config.enabled {
            return;
        }

        loop {
            match self.receive_once(handler.clone()).await {
                Ok(()) => {
                    // Connection closed normally — shouldn't happen, but reconnect
                    tracing::info!("signal: connection closed, reconnecting");
                }
                Err(e) => {
                    tracing::warn!("signal receive: {e}");
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }

    /// One receive cycle: connect, read notifications until disconnect.
    async fn receive_once(
        &self,
        handler: SignalHandler,
    ) -> Result<(), String> {
        let stream = TcpStream::connect(&self.config.socket_addr)
            .await
            .map_err(|e| format!("signal connect: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| format!("signal read: {e}"))?;

            if n == 0 {
                return Err("signal: connection closed by daemon".into());
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };

            // Only process incoming message notifications
            if msg.get("method").and_then(|m| m.as_str()) != Some("receive") {
                // This is a response to a request (has "id") — ignore, we don't
                // send requests on this connection.
                continue;
            }

            let params = match msg.get("params") {
                Some(p) => p,
                None => continue,
            };

            // Extract sender and message text from the envelope
            let envelope = match params.get("envelope") {
                Some(e) => e,
                None => continue,
            };

            // Extract sender identifiers — Signal may use phone number, UUID,
            // or profile name depending on privacy settings and protocol version.
            let source_number = envelope
                .get("sourceNumber")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let source_uuid = envelope
                .get("sourceUuid")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let source_name = envelope
                .get("sourceName")
                .and_then(|s| s.as_str())
                .unwrap_or("");

            let message = envelope
                .get("dataMessage")
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();

            // Group messages carry the group id — replies must target it.
            let group_id = envelope
                .get("dataMessage")
                .and_then(|d| d.get("groupInfo"))
                .and_then(|g| g.get("groupId"))
                .and_then(|g| g.as_str())
                .map(|s| s.to_string());

            // Extract timestamp for read receipts
            let timestamp = envelope
                .get("dataMessage")
                .and_then(|d| d.get("timestamp"))
                .or_else(|| envelope.get("timestamp"))
                .and_then(|t| t.as_u64())
                .unwrap_or(0);

            // Extract image attachments. The JSON-RPC notification carries
            // only metadata (id, contentType, size, ...); the bytes are
            // fetched on demand via the `getAttachment` JSON-RPC method,
            // which reads from signal-cli's own attachment store. We pass
            // the id through to the handler unchanged.
            let attachment_ids: Vec<(String, String)> = envelope
                .get("dataMessage")
                .and_then(|d| d.get("attachments"))
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|att| {
                            let content_type = att
                                .get("contentType")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !content_type.starts_with("image/") {
                                return None;
                            }
                            let id = att
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string();
                            if id.is_empty() {
                                return None;
                            }
                            Some((id, content_type))
                        })
                        .collect()
                })
                .unwrap_or_default();

            if !attachment_ids.is_empty() {
                tracing::info!(
                    "signal: got {} image attachment(s): {:?}",
                    attachment_ids.len(),
                    attachment_ids,
                );
            }

            if message.is_empty() && attachment_ids.is_empty() {
                continue;
            }

            // Whitelist check: match against phone number or UUID only —
            // profile names are self-asserted and prove nothing. When the
            // whitelist is EMPTY, messages pass through to the handler,
            // which applies the token-auth gate (find_signal_user +
            // /verify). The whitelist is an optional ADDITIONAL
            // restriction, not the primary auth mechanism.
            if !self.config.allowed_senders.is_empty() {
                let matched = self.config.allowed_senders.iter().any(|allowed| {
                    (!source_number.is_empty() && allowed == source_number)
                        || (!source_uuid.is_empty() && allowed == source_uuid)
                });
                if !matched {
                    tracing::warn!(
                        "signal: ignoring message from non-whitelisted sender: number={source_number} uuid={source_uuid} name={source_name}"
                    );
                    continue;
                }
            }

            let source = if !source_number.is_empty() {
                source_number
            } else if !source_uuid.is_empty() {
                source_uuid
            } else {
                source_name
            };
            handler(
                source.to_string(),
                message,
                timestamp,
                group_id,
                attachment_ids,
            );
        }
    }
}

/// Shared signal client wrapped in a Mutex for concurrent access.
#[allow(dead_code)] // Used by integrations wiring their own SharedSignal.
pub type SharedSignal = Arc<Mutex<Signal>>;

/// The inbound message handler signature: (sender, message, timestamp,
/// group_id, attachment_ids).
pub type SignalHandler =
    Arc<dyn Fn(String, String, u64, Option<String>, Vec<(String, String)>) + Send + Sync>;

/// Convert common markdown to Signal's inline formatting. Signal renders
/// *bold*, _italic_, ~strikethrough~, `inline code`, and triple-backtick
/// monospace blocks — but not **bold**, ~~strike~~, headers, links, or
/// tables.
pub fn markdown_to_signal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = false;
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            // Keep the fences — Signal clients render triple-backtick
            // blocks as monospace. Drop the language tag (unsupported).
            if in_code {
                out.push_str("```\n");
                in_code = false;
            } else {
                out.push_str("```\n");
                in_code = true;
            }
            continue;
        }
        if in_code {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // Pipe tables: wrap each line in inline-code so columns stay
        // aligned on proportional fonts. A run of table lines becomes one
        // monospace block.
        let is_table_line = trimmed.starts_with('|') && trimmed.ends_with('|');
        if is_table_line && !in_table {
            out.push_str("```\n");
            in_table = true;
        } else if !is_table_line && in_table {
            out.push_str("```\n");
            in_table = false;
        }
        if is_table_line {
            // Skip separator rows (|---|---|) — noise on Signal.
            let inner = trimmed.trim_matches('|').trim();
            if inner
                .chars()
                .all(|c| c == '-' || c == ' ' || c == ':' || c == '|')
            {
                continue;
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // ## Header → *Header*
        let mut is_header = false;
        let mut l = line;
        if trimmed.starts_with('#') {
            let after = trimmed.trim_start_matches('#');
            if after.starts_with(' ') && !after.trim().is_empty() {
                is_header = true;
                l = after.trim();
            }
        }

        let l = convert_links(l);
        let l = l.replace("**", "*").replace("~~", "~");
        if is_header {
            out.push_str(&format!("*{}*\n", l.trim_matches('*')));
        } else {
            out.push_str(&l);
            out.push('\n');
        }
    }
    if in_table {
        out.push_str("```\n");
    }
    if in_code {
        out.push_str("```\n");
    }
    let trimmed_len = out.trim_end_matches('\n').len();
    out.truncate(trimmed_len);
    out
}

/// [text](url) → text (url)
fn convert_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('[') {
        let Some(mid) = rest[start..].find("](") else {
            break;
        };
        let Some(end) = rest[start + mid + 2..].find(')') else {
            break;
        };
        let text = &rest[start + 1..start + mid];
        let url = &rest[start + mid + 2..start + mid + 2 + end];
        out.push_str(&rest[..start]);
        out.push_str(text);
        out.push_str(" (");
        out.push_str(url);
        out.push(')');
        rest = &rest[start + mid + 2 + end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_code_becomes_signal_monospace() {
        let out = markdown_to_signal("here:\n```rust\nfn main() {}\n```\ndone");
        assert!(
            out.contains("```\n"),
            "code fences preserved for Signal monospace: {out}"
        );
        assert!(out.contains("fn main() {}"));
        assert!(out.ends_with("done"));
    }

    #[test]
    fn headers_become_bold() {
        let out = markdown_to_signal("## Results");
        assert_eq!(out, "*Results*");
    }

    #[test]
    fn links_convert() {
        let out = markdown_to_signal("see [the docs](https://example.com)");
        assert_eq!(out, "see the docs (https://example.com)");
    }

    #[test]
    fn double_markers_collapse() {
        let out = markdown_to_signal("**bold** and ~~strike~~");
        assert_eq!(out, "*bold* and ~strike~");
    }

    #[test]
    fn tables_become_monospace_and_skip_separators() {
        let out = markdown_to_signal("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(out.contains("```\n"), "table wrapped in monospace: {out}");
        assert!(out.contains("| 1 | 2 |"));
        assert!(!out.contains("|---"), "separator rows dropped: {out}");
    }
}
