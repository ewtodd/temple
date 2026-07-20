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

    /// Convenience: send a notification with a title prefix to all recipients.
    pub async fn notify(&self, title: &str, body: &str) -> Result<(), String> {
        let msg = if title.is_empty() {
            body.to_string()
        } else {
            format!("*{title}*\n\n{body}")
        };
        // Send to default recipient, plus any extra allowed senders
        // (so notifications reach everyone authorized to command renco).
        let mut recipients = vec![self.config.default_recipient.clone()];
        for s in &self.config.allowed_senders {
            if !recipients.contains(s) {
                recipients.push(s.clone());
            }
        }
        let mut errors = Vec::new();
        for r in &recipients {
            if let Err(e) = self.send(r, &msg).await {
                tracing::warn!("signal send to {r} failed: {e}");
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Run the inbound receive loop. Maintains a persistent TCP connection
    /// to signal-cli daemon, reads incoming message notifications, and calls
    /// the handler for each message from an allowed sender.
    ///
    /// Returns when the connection is permanently lost (after retry backoff).
    pub async fn receive_loop(
        &self,
        handler: Arc<dyn Fn(String, String) + Send + Sync>,
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
        handler: Arc<dyn Fn(String, String) + Send + Sync>,
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

            if message.is_empty() {
                continue;
            }

            // Whitelist check: match against phone number, UUID, or name.
            // This handles cases where Signal redacts the phone number and
            // only provides a UUID (increasingly common for privacy).
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

            // Use whichever identifier matched as the source for replies
            let source = if !source_number.is_empty() {
                source_number
            } else if !source_uuid.is_empty() {
                source_uuid
            } else {
                source_name
            };
            handler(source.to_string(), message);
        }
    }
}

/// Shared signal client wrapped in a Mutex for concurrent access.
pub type SharedSignal = Arc<Mutex<Signal>>;
