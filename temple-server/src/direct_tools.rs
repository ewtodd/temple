use reqwest::Client as HttpClient;
use serde_json::Value;
use std::time::Duration;

/// Direct HTTP tool implementations replacing the LiteLLM MCP gateway.
/// Each function takes arguments and returns a result string.
pub struct DirectTools {
    client: HttpClient,
}

impl DirectTools {
    pub fn new() -> Self {
        Self {
            client: HttpClient::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("temple/0.1")
                .build()
                .expect("http client"),
        }
    }

    pub async fn fetch(&self, url: &str) -> Result<String, String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("fetch: {e}"))?;
        let body = resp.text().await.map_err(|e| format!("fetch read: {e}"))?;
        // Strip HTML tags for plain-text extraction
        Ok(strip_html(&body))
    }

    pub async fn searxng_search(&self, query: &str, count: usize) -> Result<String, String> {
        let resp = self
            .client
            .get("http://localhost:8081/search")
            .query(&[("q", query), ("format", "json"), ("categories", "general")])
            .send()
            .await
            .map_err(|e| format!("searxng: {e}"))?;
        let json: Value = resp
            .json()
            .await
            .map_err(|e| format!("searxng parse: {e}"))?;
        let results = json["results"].as_array().ok_or("searxng: no results")?;
        let mut out = String::new();
        for (i, r) in results.iter().take(count).enumerate() {
            let title = r["title"].as_str().unwrap_or("");
            let url = r["url"].as_str().unwrap_or("");
            let snippet = r["content"].as_str().unwrap_or("");
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                title,
                url,
                snippet
            ));
        }
        if out.is_empty() {
            return Ok("No results found.".into());
        }
        Ok(out.trim_end().to_string())
    }

    pub async fn nixos_nix(
        &self,
        action: &str,
        query: &str,
        channel: &str,
        limit: usize,
    ) -> Result<String, String> {
        match action {
            "search" => {
                let url = format!(
                    "https://search.nixos.org/backend/search?query={}&channel={}",
                    query, channel
                );
                let resp = self
                    .client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| format!("nixos: {e}"))?;
                let text = resp.text().await.map_err(|e| format!("nixos read: {e}"))?;
                let json: Value =
                    serde_json::from_str(&text).map_err(|e| format!("nixos parse: {e}"))?;
                let results = json["results"].as_array().ok_or("nixos: no results")?;
                let mut out = String::new();
                for r in results.iter().take(limit) {
                    let pkg = r.as_str().unwrap_or("?");
                    out.push_str(&format!("- {pkg}\n"));
                }
                if out.is_empty() {
                    return Ok("No packages found.".into());
                }
                Ok(format!("nixpkgs/{channel}:\n{}", out.trim_end()))
            }
            "info" => {
                let url = format!(
                    "https://search.nixos.org/backend/search?query={}&channel={}&limit=1",
                    query, channel
                );
                let resp = self
                    .client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| format!("nixos: {e}"))?;
                let text = resp.text().await.map_err(|e| format!("nixos read: {e}"))?;
                let json: Value =
                    serde_json::from_str(&text).map_err(|e| format!("nixos parse: {e}"))?;
                Ok(serde_json::to_string_pretty(&json).unwrap_or_default())
            }
            _ => Err(format!("nixos-nix: unsupported action '{action}'")),
        }
    }

    pub async fn arxiv_search(&self, query: &str, max_results: usize) -> Result<String, String> {
        let url = format!(
            "http://export.arxiv.org/api/query?search_query=all:{query}&max_results={max_results}&sortBy=relevance"
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("arxiv: {e}"))?;
        let body = resp.text().await.map_err(|e| format!("arxiv read: {e}"))?;
        // Simple XML extraction — grab title, summary, and id
        let mut out = String::new();
        let entries: Vec<&str> = body.split("<entry>").collect();
        for (i, entry) in entries.iter().skip(1).take(max_results).enumerate() {
            let title = extract_xml(entry, "title");
            let summary = extract_xml(entry, "summary");
            let id = extract_xml(entry, "id");
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                title.as_deref().unwrap_or("?"),
                id.as_deref().unwrap_or("?"),
                summary
                    .as_deref()
                    .unwrap_or("?")
                    .chars()
                    .take(300)
                    .collect::<String>(),
            ));
        }
        if out.is_empty() {
            return Ok("No papers found.".into());
        }
        Ok(out.trim_end().to_string())
    }

    pub async fn context7_search(&self, query: &str) -> Result<String, String> {
        // Context7 API — resolve library then query docs
        let resolve_url =
            format!("https://context7.com/api/resolve?query={query}&libraryName={query}");
        let resp = self
            .client
            .get(&resolve_url)
            .send()
            .await
            .map_err(|e| format!("context7 resolve: {e}"))?;
        let body = resp
            .text()
            .await
            .map_err(|e| format!("context7 read: {e}"))?;
        let json: Value =
            serde_json::from_str(&body).map_err(|e| format!("context7 parse: {e}"))?;
        if let Some(libraries) = json.as_array() {
            if let Some(first) = libraries.first() {
                if let Some(lib_id) = first["id"].as_str() {
                    let query_url =
                        format!("https://context7.com/api/query/{lib_id}?query={query}");
                    let resp2 = self
                        .client
                        .get(&query_url)
                        .send()
                        .await
                        .map_err(|e| format!("context7 query: {e}"))?;
                    let body2 = resp2
                        .text()
                        .await
                        .map_err(|e| format!("context7 read: {e}"))?;
                    return Ok(body2.chars().take(8000).collect());
                }
            }
        }
        Ok("Context7: no results found.".into())
    }

    /// Run ripgrep (rg) with JSON output in the given directory.
    /// Falls back gracefully if rg is not installed.
    pub fn grep_code(cwd: &str, pattern: &str, max_results: usize) -> Result<String, String> {
        let output = std::process::Command::new("rg")
            .args(["--json", "--line-number", "--no-heading", "--", pattern])
            .current_dir(cwd)
            .output();
        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                if stdout.is_empty() && !o.status.success() {
                    return Err(format!("rg failed: {}", String::from_utf8_lossy(&o.stderr)));
                }
                if stdout.is_empty() {
                    return Ok(format!("No matches for '{pattern}' in {cwd}"));
                }
                let mut out = String::new();
                let mut count = 0usize;
                for line in stdout.lines() {
                    if count >= max_results {
                        out.push_str(&format!(
                            "\n…[{} more matches truncated]",
                            stdout.lines().count().saturating_sub(max_results)
                        ));
                        break;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(line) {
                        if v["type"] == "match" {
                            let path = v["data"]["path"]["text"].as_str().unwrap_or("?");
                            let ln = v["data"]["line_number"].as_u64().unwrap_or(0);
                            let text = v["data"]["lines"]["text"].as_str().unwrap_or("");
                            let text = text.trim();
                            out.push_str(&format!("{path}:{ln}: {text}\n"));
                            count += 1;
                        }
                    }
                }
                if out.is_empty() {
                    return Ok(format!("No matches for '{pattern}' in {cwd}"));
                }
                Ok(out.trim_end().to_string())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err("ripgrep (rg) is not installed. Install it for code search.".into())
            }
            Err(e) => Err(format!("rg error: {e}")),
        }
    }
}

fn strip_html(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // Collapse multiple blank lines
    let mut result = String::new();
    let mut prev_empty = false;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_empty {
                result.push('\n');
                prev_empty = true;
            }
        } else {
            result.push_str(trimmed);
            result.push('\n');
            prev_empty = false;
        }
    }
    result.trim().to_string()
}

fn extract_xml(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(
        xml[start..start + end]
            .chars()
            .map(|c| match c {
                '\n' | '\r' => ' ',
                _ => c,
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    )
}
