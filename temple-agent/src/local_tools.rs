//! Local filesystem tool execution for the embedded agent. The daemon runs
//! the full agent in-process, so fs tools execute here instead of being
//! shipped to a remote client. Paths are confined to the session working
//! directory (reads may additionally touch /tmp); consent for risky
//! operations is the caller's job (permission scope + mode).

use std::path::{Path, PathBuf};

const MAX_READ_BYTES: usize = 1 << 20; // 1 MiB
const MAX_CMD_OUTPUT: usize = 1 << 20; // 1 MiB
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Resolve a tool path against the session cwd. Absolute paths must stay
/// under cwd; relative paths resolve from cwd. Reads may additionally
/// touch /tmp. Writes are confined to cwd.
pub fn resolve_tool_path(path: &str, cwd: &str) -> Result<PathBuf, String> {
    resolve_tool_path_for(path, cwd, false)
}

fn resolve_tool_path_for(path: &str, cwd: &str, for_write: bool) -> Result<PathBuf, String> {
    let cwd_path = PathBuf::from(cwd);
    let cwd_canon = cwd_path.canonicalize().unwrap_or(cwd_path.clone());

    let raw = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd_canon.join(path)
    };
    // Lexically normalize first so ../sibling resolves before the check.
    let normalized = normalize(&raw);
    let resolved = normalized.canonicalize().unwrap_or(normalized);

    if resolved.starts_with(&cwd_canon) || (!for_write && resolved.starts_with("/tmp")) {
        Ok(resolved)
    } else {
        Err(format!(
            "{:?} escapes working directory ({})",
            path,
            cwd_canon.display()
        ))
    }
}

/// Lexical .. collapse without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Execute a tool against the local filesystem, confined to `cwd`.
pub async fn execute_local_tool(name: &str, args_json: &str, cwd: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => return format!("Error: invalid tool arguments JSON: {e}"),
    };

    match name {
        "read_file" => {
            let path = args["path"].as_str().unwrap_or(".");
            match resolve_tool_path(path, cwd) {
                Err(e) => format!("Error: {e}"),
                Ok(resolved) => match tokio::fs::File::open(&resolved).await {
                    Err(e) => format!("Error: {e}"),
                    Ok(f) => {
                        use tokio::io::AsyncReadExt;
                        let mut buf = Vec::with_capacity(8192);
                        let mut capped = f.take(MAX_READ_BYTES as u64 + 1);
                        match capped.read_to_end(&mut buf).await {
                            Err(e) => format!("Error: {e}"),
                            Ok(_) => {
                                let truncated = buf.len() as u64 > MAX_READ_BYTES as u64;
                                buf.truncate(MAX_READ_BYTES);
                                let mut out = String::from_utf8_lossy(&buf).to_string();
                                if truncated {
                                    out.push_str("\n[truncated at 1 MiB]");
                                }
                                out.push_str(&format!("\n[read {path}]"));
                                out
                            }
                        }
                    }
                },
            }
        }
        "write_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            match resolve_tool_path_for(path, cwd, true) {
                Err(e) => format!("Error: {e}"),
                Ok(resolved) => {
                    if resolved.is_dir() {
                        format!("Error: {:?} is a directory, not a file", resolved.display())
                    } else {
                        if let Some(parent) = resolved.parent() {
                            tokio::fs::create_dir_all(parent).await.ok();
                        }
                        match tokio::fs::write(&resolved, content).await {
                            Ok(()) => {
                                format!("wrote {} ({} bytes)", resolved.display(), content.len())
                            }
                            Err(e) => format!("Error: {e}"),
                        }
                    }
                }
            }
        }
        "list_dir" => {
            let path = args["path"].as_str().unwrap_or(".");
            match resolve_tool_path(path, cwd) {
                Err(e) => format!("Error: {e}"),
                Ok(resolved) => match tokio::fs::read_dir(&resolved).await {
                    Err(e) => format!("Error: {e}"),
                    Ok(mut entries) => {
                        let mut out = Vec::new();
                        while let Ok(Some(e)) = entries.next_entry().await {
                            let name = e.file_name().to_string_lossy().to_string();
                            let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                            out.push(format!("{}{}", name, if is_dir { "/" } else { "" }));
                        }
                        out.join("\n")
                    }
                },
            }
        }
        "execute_command" => {
            let command = args["command"].as_str().unwrap_or("");
            let shell = std::env::var("SHELL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/bin/sh".to_string());
            let child = tokio::process::Command::new(&shell)
                .arg("-c")
                .arg(command)
                .current_dir(cwd)
                .kill_on_drop(true)
                .output();
            match tokio::time::timeout(CMD_TIMEOUT, child).await {
                Err(_) => format!("Error: command timed out after {}s", CMD_TIMEOUT.as_secs()),
                Ok(Err(e)) => format!("Error: {e}"),
                Ok(Ok(output)) => {
                    let cap = |bytes: &[u8]| -> String {
                        if bytes.len() > MAX_CMD_OUTPUT {
                            let mut s =
                                String::from_utf8_lossy(&bytes[..MAX_CMD_OUTPUT]).to_string();
                            s.push_str("\n[truncated]");
                            s
                        } else {
                            String::from_utf8_lossy(bytes).to_string()
                        }
                    };
                    let stdout = cap(&output.stdout);
                    let stderr = cap(&output.stderr);
                    let mut out = String::new();
                    if !stdout.is_empty() {
                        out.push_str(&stdout);
                    }
                    if !stderr.is_empty() {
                        out.push_str(&format!("\n[stderr]\n{stderr}"));
                    }
                    if !output.status.success() {
                        out.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
                    }
                    if out.is_empty() {
                        out.push_str("(no output)");
                    }
                    out
                }
            }
        }
        "edit_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let old_str = args["old_str"].as_str().unwrap_or("");
            let new_str = args["new_str"].as_str().unwrap_or("");
            match resolve_tool_path_for(path, cwd, true) {
                Err(e) => format!("Error: {e}"),
                Ok(resolved) => {
                    if !resolved.exists() {
                        format!(
                            "Error: {:?} does not exist — use write_file to create it",
                            resolved.display()
                        )
                    } else {
                        match tokio::fs::read_to_string(&resolved).await {
                            Err(e) => format!("Error: {e}"),
                            Ok(content) => {
                                if !content.contains(old_str) {
                                    format!("Error: old_str not found in {}", resolved.display())
                                } else {
                                    let count = content.matches(old_str).count();
                                    if count > 1 {
                                        format!(
                                            "Error: old_str found {count} times — must be unique"
                                        )
                                    } else {
                                        let mut n = content.replacen(old_str, new_str, 1);
                                        if content.ends_with('\n') && !n.ends_with('\n') {
                                            n.push('\n');
                                        }
                                        // Atomic write: temp file in the same
                                        // directory, then rename.
                                        let tmp_path = {
                                            let mut p = resolved.clone().into_os_string();
                                            p.push(".temple-tmp");
                                            PathBuf::from(p)
                                        };
                                        match tokio::fs::write(&tmp_path, &n).await {
                                            Err(e) => format!("Error: {e}"),
                                            Ok(()) => match tokio::fs::rename(&tmp_path, &resolved)
                                                .await
                                            {
                                                Err(e) => {
                                                    tokio::fs::remove_file(&tmp_path).await.ok();
                                                    format!("Error: {e}")
                                                }
                                                Ok(()) => format!(
                                                    "edited {} (replaced 1 occurrence)",
                                                    resolved.display()
                                                ),
                                            },
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => format!("Error: unknown tool {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_resolve_tool_path_relative() {
        let tmp = TempDir::new().unwrap();
        // Resolve symlinks in TMPDIR for NixOS compatibility
        let cwd = std::fs::canonicalize(tmp.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        let file_path = std::path::Path::new(&cwd).join("foo.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let resolved = resolve_tool_path("foo.txt", &cwd).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&file_path).unwrap());
    }

    #[test]
    fn test_resolve_tool_path_new_file() {
        let tmp = TempDir::new().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let cwd = base.to_string_lossy().to_string();
        let resolved = resolve_tool_path_for("a/b/new.txt", &cwd, true).unwrap();
        assert_eq!(resolved, base.join("a/b/new.txt"));
        assert!(resolved.starts_with(&base));
    }

    #[test]
    fn test_resolve_tool_path_escape() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let cwd = sub.to_str().unwrap();

        let result = resolve_tool_path_for("../sibling.txt", cwd, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("escapes"));
    }

    #[test]
    fn test_resolve_tool_path_tmp_write_denied_read_allowed() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        assert!(resolve_tool_path_for("/tmp/evil.sh", cwd, true).is_err());
        assert!(resolve_tool_path("/tmp/somefile", cwd).is_ok());
    }
}
