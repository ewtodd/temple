//! Open WebUI memory client: temple's memory store bridges to Open WebUI
//! through its REST API, making the web UI's Memories page and temple's
//! save/recall the same store. One API key owns one Open WebUI user — all
//! memories land under that account; the local SQLite cache preserves the
//! per-scope (per-user) distinction temple keeps.
//!
//! Endpoints verified against open-webui 0.11.0 (`routers/memories.py`):
//! list/add/search/query are all scoped to the authenticated user.

use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::config::OpenWebUiConfig;

/// One memory as Open WebUI stores it.
#[derive(Debug, Clone, Deserialize)]
pub struct OwMemory {
    pub id: String,
    pub content: String,
}

/// Semantic-query hit: vector search returns documents without ids.
#[derive(Debug, Clone)]
pub struct OwMemoryHit {
    pub content: String,
}

/// Open WebUI REST client. `base_url` is the API root (no `/api` suffix);
/// all requests go to `/api/v1/...` with a Bearer token.
pub struct OpenWebUi {
    base_url: String,
    api_key: String,
    client: HttpClient,
}

/// Build the client from config, or `None` when disabled or the API key
/// env var is unset (the credential must never be a config value).
pub fn openwebui_from_config(cfg: &OpenWebUiConfig) -> Option<Arc<OpenWebUi>> {
    if !cfg.enabled {
        return None;
    }
    let key = std::env::var(&cfg.api_key_env).unwrap_or_default();
    if key.is_empty() {
        tracing::warn!(
            "openwebui: enabled but ${} is unset — memory stays local-only",
            cfg.api_key_env
        );
        return None;
    }
    Some(Arc::new(OpenWebUi {
        base_url: cfg.base_url.trim_end_matches('/').to_string(),
        api_key: key,
        client: HttpClient::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client"),
    }))
}

impl OpenWebUi {
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}/api/v1{}", self.base_url, path))
            .bearer_auth(&self.api_key)
    }

    fn post(&self, path: &str, body: &impl Serialize) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/api/v1{}", self.base_url, path))
            .bearer_auth(&self.api_key)
            .json(body)
    }

    fn delete(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .delete(format!("{}/api/v1{}", self.base_url, path))
            .bearer_auth(&self.api_key)
    }

    /// All of the key owner's memories.
    pub async fn list_memories(&self) -> Result<Vec<OwMemory>, String> {
        let resp = self
            .get("/memories")
            .send()
            .await
            .map_err(|e| format!("openwebui list: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("openwebui list: HTTP {}", resp.status()));
        }
        resp.json::<Vec<OwMemory>>()
            .await
            .map_err(|e| format!("openwebui list parse: {e}"))
    }

    /// Create a memory for the key owner.
    pub async fn add_memory(&self, content: &str) -> Result<OwMemory, String> {
        let resp = self
            .post("/memories/add", &serde_json::json!({ "content": content }))
            .send()
            .await
            .map_err(|e| format!("openwebui add: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("openwebui add: HTTP {}", resp.status()));
        }
        resp.json::<OwMemory>()
            .await
            .map_err(|e| format!("openwebui add parse: {e}"))
    }

    /// Update one of the key owner's memories by id.
    pub async fn update_memory(&self, id: &str, content: &str) -> Result<(), String> {
        let resp = self
            .post(
                &format!("/memories/{id}/update"),
                &serde_json::json!({ "content": content }),
            )
            .send()
            .await
            .map_err(|e| format!("openwebui update: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("openwebui update: HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// Delete one of the key owner's memories by id.
    pub async fn delete_memory(&self, id: &str) -> Result<(), String> {
        let resp = self
            .delete(&format!("/memories/{id}"))
            .send()
            .await
            .map_err(|e| format!("openwebui delete: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("openwebui delete: HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// Semantic recall (needs the embedding backend configured). Returns
    /// the top-k memory contents for a query.
    pub async fn query_memories(&self, query: &str, k: u32) -> Result<Vec<String>, String> {
        let resp = self
            .post(
                "/memories/query",
                &serde_json::json!({ "content": query, "k": k }),
            )
            .send()
            .await
            .map_err(|e| format!("openwebui query: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new()); // "No memories found for user"
        }
        if !resp.status().is_success() {
            return Err(format!("openwebui query: HTTP {}", resp.status()));
        }
        #[derive(Deserialize)]
        struct QueryResult {
            documents: Vec<Vec<String>>,
        }
        let parsed: QueryResult = resp
            .json()
            .await
            .map_err(|e| format!("openwebui query parse: {e}"))?;
        Ok(parsed.documents.into_iter().flatten().collect())
    }

    /// Keyword search fallback that works without an embedding backend.
    pub async fn search_memories(&self, query: &str, limit: u32) -> Result<Vec<OwMemory>, String> {
        let resp = self
            .post(
                "/memories/search",
                &serde_json::json!({ "query": query, "limit": limit }),
            )
            .send()
            .await
            .map_err(|e| format!("openwebui search: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("openwebui search: HTTP {}", resp.status()));
        }
        resp.json::<Vec<OwMemory>>()
            .await
            .map_err(|e| format!("openwebui search parse: {e}"))
    }
}

/// Format one temple KV memory as an Open WebUI free-text line. Global
/// scope stays plain `key: value`; per-user scopes carry a marker so
/// entries from different temple users don't collide in one account.
pub fn memory_line(key: &str, value: &str, scope: &str) -> String {
    if scope.is_empty() || scope == "global" {
        format!("{key}: {value}")
    } else {
        format!("[{scope}] {key}: {value}")
    }
}

/// The stable prefix of a memory line written by {@link memory_line} —
/// everything up to and including `": "` (scope marker included, so
/// per-scope lines stay distinct). Matching remote entries by this
/// prefix makes set_memory an upsert.
pub fn line_prefix(line: &str) -> String {
    let (key, _) = line.split_once(": ").unwrap_or((line, ""));
    format!("{key}: ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_line_formats() {
        assert_eq!(memory_line("name", "ethan", "global"), "name: ethan");
        assert_eq!(memory_line("name", "ethan", ""), "name: ethan");
        assert_eq!(
            memory_line("name", "ethan", "e-play"),
            "[e-play] name: ethan"
        );
    }

    #[test]
    fn line_prefix_round_trip() {
        assert_eq!(line_prefix("name: ethan"), "name: ");
        assert_eq!(line_prefix("plain"), "plain: ");
        // Scoped matching must keep the marker so users don't collide.
        assert_ne!(
            line_prefix("[e-play] name: ethan"),
            line_prefix("[e-work] name: ethan")
        );
        assert_eq!(
            line_prefix("[e-play] name: ethan"),
            line_prefix("[e-play] name: val")
        );
    }
}
