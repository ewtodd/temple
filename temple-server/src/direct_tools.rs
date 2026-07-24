use reqwest::Client as HttpClient;
use serde_json::Value;
use std::time::Duration;

const MAX_RETRIES: u32 = 3;
const RETRY_BASE_MS: u64 = 500;

async fn retry_http<T, F, Fut>(name: &str, f: F) -> Result<T, String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut last_err = String::new();
    for attempt in 0..MAX_RETRIES {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = e;
                if attempt < MAX_RETRIES - 1 {
                    tokio::time::sleep(Duration::from_millis(RETRY_BASE_MS * (attempt as u64 + 1)))
                        .await;
                    tracing::warn!("{name}: retry {}/{}", attempt + 1, MAX_RETRIES - 1);
                }
            }
        }
    }
    Err(format!("{name}: {last_err} (after {MAX_RETRIES} retries)"))
}

fn base64_encode(data: &[u8]) -> String {
    use std::io::Write;
    let mut enc =
        base64::write::EncoderStringWriter::new(&base64::engine::general_purpose::STANDARD);
    enc.write_all(data).ok();
    enc.into_inner()
}

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
        let client = self.client.clone();
        let url = url.to_string();
        retry_http("fetch", || {
            let client = client.clone();
            let url = url.clone();
            async move {
                let resp = client.get(&url).send().await.map_err(|e| format!("{e}"))?;
                let body = resp.text().await.map_err(|e| format!("read: {e}"))?;
                Ok(strip_html(&body))
            }
        })
        .await
    }

    pub async fn searxng_search(&self, query: &str, count: usize) -> Result<String, String> {
        let client = self.client.clone();
        let query = query.to_string();
        retry_http("searxng", || {
            let client = client.clone();
            let query = query.clone();
            async move {
                let fmt = "json".to_string();
                let cat = "general".to_string();
                let resp = client
                    .get("http://localhost:8081/search")
                    .query(&[("q", &query), ("format", &fmt), ("categories", &cat)])
                    .send()
                    .await
                    .map_err(|e| format!("{e}"))?;
                let json: Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
                let results = json["results"].as_array().ok_or("no results")?;
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
        })
        .await
    }

    pub async fn nixos_nix(
        &self,
        action: &str,
        query: &str,
        channel: &str,
        limit: usize,
    ) -> Result<String, String> {
        let client = self.client.clone();
        let action = action.to_string();
        let query = query.to_string();
        let channel = channel.to_string();
        retry_http("nixos-nix", || {
            let client = client.clone();
            let query = query.clone();
            let channel = channel.clone();
            let action = action.clone();
            async move {
                match action.as_str() {
                    "search" => {
                        let url = format!(
                            "https://search.nixos.org/backend/search?query={query}&channel={channel}"
                        );
                        let resp = client
                            .get(&url)
                            .send()
                            .await
                            .map_err(|e| format!("{e}"))?;
                        let text =
                            resp.text().await.map_err(|e| format!("read: {e}"))?;
                        let json: Value = serde_json::from_str(&text)
                            .map_err(|e| format!("parse: {e}"))?;
                        let results =
                            json["results"].as_array().ok_or("no results")?;
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
                            "https://search.nixos.org/backend/search?query={query}&channel={channel}&limit=1"
                        );
                        let resp = client
                            .get(&url)
                            .send()
                            .await
                            .map_err(|e| format!("{e}"))?;
                        let text =
                            resp.text().await.map_err(|e| format!("read: {e}"))?;
                        let json: Value = serde_json::from_str(&text)
                            .map_err(|e| format!("parse: {e}"))?;
                        Ok(serde_json::to_string_pretty(&json).unwrap_or_default())
                    }
                    _ => Err(format!("unsupported action '{action}'")),
                }
            }
        })
        .await
    }

    pub async fn arxiv_search(&self, query: &str, max_results: usize) -> Result<String, String> {
        let client = self.client.clone();
        let query = query.to_string();
        retry_http("arxiv", || {
            let client = client.clone();
            let query = query.clone();
            async move {
                let url = format!(
                    "http://export.arxiv.org/api/query?search_query=all:{query}&max_results={max_results}&sortBy=relevance"
                );
                let resp = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| format!("{e}"))?;
                let body =
                    resp.text().await.map_err(|e| format!("read: {e}"))?;
                let mut out = String::new();
                let entries: Vec<&str> = body.split("<entry>").collect();
                for (i, entry) in
                    entries.iter().skip(1).take(max_results).enumerate()
                {
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
        })
        .await
    }

    pub async fn context7_search(&self, query: &str) -> Result<String, String> {
        let client = self.client.clone();
        let query = query.to_string();
        retry_http("context7", || {
            let client = client.clone();
            let query = query.clone();
            async move {
                let resolve_url =
                    format!("https://context7.com/api/resolve?query={query}&libraryName={query}");
                let resp = client
                    .get(&resolve_url)
                    .send()
                    .await
                    .map_err(|e| format!("resolve: {e}"))?;
                let body = resp.text().await.map_err(|e| format!("read: {e}"))?;
                let json: Value = serde_json::from_str(&body).map_err(|e| format!("parse: {e}"))?;
                if let Some(libraries) = json.as_array() {
                    if let Some(first) = libraries.first() {
                        if let Some(lib_id) = first["id"].as_str() {
                            let query_url =
                                format!("https://context7.com/api/query/{lib_id}?query={query}");
                            let resp2 = client
                                .get(&query_url)
                                .send()
                                .await
                                .map_err(|e| format!("query: {e}"))?;
                            let body2 = resp2.text().await.map_err(|e| format!("read: {e}"))?;
                            return Ok(body2.chars().take(8000).collect());
                        }
                    }
                }
                Ok("Context7: no results found.".into())
            }
        })
        .await
    }

    /// Read an image file from disk, base64 encode it, and return a
    /// data URL suitable for vision model requests.
    pub fn read_image_base64(path: &str) -> Result<String, String> {
        let allowed = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !allowed.iter().any(|a| *a == ext) {
            return Err(format!(
                "unsupported format '.{ext}'. Supported: {}",
                allowed.join(", ")
            ));
        }
        let data = std::fs::read(path).map_err(|e| format!("cannot read: {e}"))?;
        if data.len() > 20 * 1024 * 1024 {
            return Err("image too large (max 20 MB)".into());
        }
        let b64 = base64_encode(&data);
        let mime = match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            _ => "image/png",
        };
        Ok(format!("data:{mime};base64,{b64}"))
    }

    /// Run ripgrep (rg) with JSON output in the given directory.
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

    /// Run ast-grep (tree-sitter structured pattern search) in the given
    /// directory. Falls back gracefully if sg is not installed.
    pub fn ast_grep(
        cwd: &str,
        pattern: &str,
        language: &str,
        max_results: usize,
    ) -> Result<String, String> {
        let mut cmd = std::process::Command::new("sg");
        cmd.args(["--pattern", pattern, "--lang", language])
            .current_dir(cwd);
        let output = cmd.output();
        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                if stdout.is_empty() && !o.status.success() {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    return Err(format!("sg failed: {}", stderr));
                }
                if stdout.is_empty() {
                    return Ok(format!(
                        "No matches for pattern in {} (lang: {language})",
                        cwd
                    ));
                }
                let lines: Vec<&str> = stdout.lines().take(max_results).collect();
                let mut out = lines.join("\n");
                if stdout.lines().count() > max_results {
                    out.push_str(&format!(
                        "\n…[{} more matches truncated]",
                        stdout.lines().count() - max_results
                    ));
                }
                Ok(out)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err("ast-grep (sg) is not installed. Install it for structured code search.".into())
            }
            Err(e) => Err(format!("sg error: {e}")),
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
