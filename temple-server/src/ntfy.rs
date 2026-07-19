use crate::config::NtfyConfig;
use reqwest::Client;

pub struct Ntfy {
    client: Client,
    config: NtfyConfig,
}

impl Ntfy {
    pub fn new(config: &NtfyConfig) -> Self {
        Self {
            client: Client::new(),
            config: config.clone(),
        }
    }

    /// Publish a notification to the out-topic
    pub async fn notify(&self, title: &str, body: &str) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }
        let url = format!(
            "{}/{}",
            self.config.server_url.trim_end_matches('/'),
            self.config.out_topic
        );
        let resp = self
            .client
            .post(&url)
            .header("Title", title)
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("ntfy publish failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("ntfy publish error: {}", resp.status()));
        }
        Ok(())
    }

    /// Poll for incoming messages (two-way communication).
    /// In production this would use SSE /subscribe; here we poll.
    pub async fn poll_incoming(&self) -> Result<Vec<String>, String> {
        if !self.config.enabled {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}/{}/json?poll",
            self.config.server_url.trim_end_matches('/'),
            self.config.control_topic
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("ntfy poll failed: {e}"))?;
        let messages: Vec<String> = resp
            .json::<Vec<serde_json::Value>>()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                m.get("message")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        Ok(messages)
    }
}
