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

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Send a read receipt for a message timestamp. This marks the message
    /// as "read" on the sender's phone.
    pub async fn send_read_receipt(&self, recipient: &str, timestamp: u64) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }

        let mut stream = TcpStream::connect(&self.config.socket_addr)
            .await
            .map_err(|e| format!("signal connect: {e}"))?;

        let req = json!({
            "jsonrpc": "2.0",
            "method": "sendReadReceipt",
            "params": {
                "recipient": [recipient],
                "timestamps": [timestamp],
            },
            "id": 1,
        });
        let line = format!("{}\n", req.to_string());
        stream.write_all(line.as_bytes())
            .await
            .map_err(|e| format!("signal write: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line)
            .await
            .map_err(|e| format!("signal read: {e}"))?;

        Ok(())
    }

    /// Send a typing indicator to a recipient.
    pub async fn send_typing(&self, recipient: &str) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }

        let mut stream = TcpStream::connect(&self.config.socket_addr)
            .await
            .map_err(|e| format!("signal connect: {e}"))?;

        let req = json!({
            "jsonrpc": "2.0",
            "method": "sendTypingMessage",
            "params": {
                "recipient": [recipient],
            },
            "id": 1,
        });
        let line = format!("{}\n", req.to_string());
        stream.write_all(line.as_bytes())
            .await
            .map_err(|e| format!("signal write: {e}"))?;

        // Don't bother reading response — fire and forget
        Ok(())
    }

    /// Send a message to a recipient. Opens a fresh TCP connection, sends
    /// the JSON-RPC `send` command, reads the response, closes.
    pub async fn send(&self, recipient: &str, message: &str) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }

        let mut stream = TcpStream::connect(&self.config.socket_addr)
            .await
            .map_err(|e| format!("signal connect: {e}"))?;

        let req = json!({
            "jsonrpc": "2.0",
            "method": "send",
            "params": {
                "recipient": [recipient],
                "message": message,
            },
            "id": 1,
        });
        let line = format!("{}\n", req.to_string());
        stream.write_all(line.as_bytes())
            .await
            .map_err(|e| format!("signal write: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line)
            .await
            .map_err(|e| format!("signal read: {e}"))?;

        if let Ok(resp) = serde_json::from_str::<Value>(&response_line) {
            if let Some(err) = resp.get("error") {
                return Err(format!("signal send error: {err}"));
            }
        }

        Ok(())
    }

    /// Send a message, splitting into multiple messages if it's long.
    /// Signal handles long messages but splitting gives better UX on mobile.
    /// Splits on paragraph boundaries when possible, ~1800 chars per message.
    pub async fn send_multi(&self, recipient: &str, message: &str) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }

        const MAX_LEN: usize = 1800;
        let chars: Vec<char> = message.chars().collect();

        if chars.len() <= MAX_LEN {
            return self.send(recipient, message).await;
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
            self.send(recipient, chunk).await?;
        }
        Ok(())
    }

    /// Convenience: send a notification with a title prefix.
    /// Sends to the default recipient (or all recipients if multiple are set).
    pub async fn notify(&self, title: &str, body: &str) -> Result<(), String> {
        let msg = if title.is_empty() {
            body.to_string()
        } else {
            format!("*{title}*\n\n{body}")
        };
        self.send(&self.config.default_recipient.clone(), &msg).await
    }

    /// Send a notification to a specific recipient.
    pub async fn notify_recipient(&self, recipient: &str, title: &str, body: &str) -> Result<(), String> {
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
    ///
    /// Returns when the connection is permanently lost (after retry backoff).
    pub async fn receive_loop(
        &self,
        handler: Arc<dyn Fn(String, String, u64) + Send + Sync>,
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
        handler: Arc<dyn Fn(String, String, u64) + Send + Sync>,
    ) -> Result<(), String> {
        let stream = TcpStream::connect(&self.config.socket_addr)
            .await
            .map_err(|e| format!("signal connect: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader.read_line(&mut line)
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
            let source_number = envelope.get("sourceNumber")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let source_uuid = envelope.get("sourceUuid")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let source_name = envelope.get("sourceName")
                .and_then(|s| s.as_str())
                .unwrap_or("");

            let message = envelope.get("dataMessage")
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();

            // Extract timestamp for read receipts
            let timestamp = envelope.get("dataMessage")
                .and_then(|d| d.get("timestamp"))
                .or_else(|| envelope.get("timestamp"))
                .and_then(|t| t.as_u64())
                .unwrap_or(0);

            if message.is_empty() {
                continue;
            }

            // Whitelist check: match against phone number, UUID, or name.
            let matched = self.config.allowed_senders.iter().any(|allowed| {
                allowed == source_number
                    || allowed == source_uuid
                    || allowed == source_name
            });
            if !self.config.allowed_senders.is_empty() && !matched {
                tracing::warn!(
                    "signal: ignoring message from non-whitelisted sender: number={source_number} uuid={source_uuid} name={source_name}"
                );
                continue;
            }

            let source = if !source_number.is_empty() {
                source_number
            } else if !source_uuid.is_empty() {
                source_uuid
            } else {
                source_name
            };
            handler(source.to_string(), message, timestamp);
        }
    }
}

/// Shared signal client wrapped in a Mutex for concurrent access.
pub type SharedSignal = Arc<Mutex<Signal>>;
