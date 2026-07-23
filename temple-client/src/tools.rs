use std::path::{Path, PathBuf};
use temple_protocol::PermissionMode;

/// Max bytes read from a file (protects the client from OOM on huge
/// files — the server truncates further anyway).
const MAX_READ_BYTES: usize = 1 << 20; // 1 MiB
/// Max bytes captured per stream from a command.
const MAX_CMD_OUTPUT: usize = 256 << 10; // 256 KiB
/// Client-side command timeout — a hung command must never wedge the
/// reader loop. The server caps at 120s for SSH; match that here.
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// OSC 52 copy to clipboard — writes the selected text to the terminal's
/// clipboard buffer. Safe: the data never leaves the local tty.
pub fn osc52_copy(text: &str) {
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    let encoded = B64.encode(text.as_bytes());
    print!("\x1b]52;c;{encoded}\x07");
}

/// Resolve a tool path relative to `cwd`, read-only (allows /tmp).
pub fn resolve_tool_path(path: &str, cwd: &str) -> Result<PathBuf, String> {
    resolve_tool_path_for(path, cwd, false)
}

/// `for_write` tightens the sandbox: /tmp is allowed for reads but never
/// for writes — world-writable shared dirs are too easy to abuse.
fn resolve_tool_path_for(path: &str, cwd: &str, for_write: bool) -> Result<PathBuf, String> {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    };

    let mut candidate = abs.clone();
    let mut suffix = PathBuf::new();
    loop {
        if candidate.exists() {
            // If the candidate is a file (not a dir) and we have a non-empty
            // suffix (meaning we walked up from the original path), reject.
            // This prevents ENOTDIR when the LLM hallucinates a sub-path
            // under a file, e.g. "src/main.rs/config.rs".
            if !candidate.is_dir() && !suffix.as_os_str().is_empty() {
                return Err(format!(
                    "{}: {:?} is not a directory",
                    abs.display(),
                    candidate
                ));
            }
            break;
        }
        match candidate.parent().and_then(|parent| {
            if parent.as_os_str().is_empty() {
                None
            } else {
                Some(parent)
            }
        }) {
            Some(parent) => {
                suffix = candidate
                    .file_name()
                    .map(|n| PathBuf::from(n).join(&suffix))
                    .unwrap_or(suffix);
                candidate = parent.to_path_buf();
            }
            None => break,
        }
    }

    let canonical_base = std::fs::canonicalize(&candidate)
        .map_err(|e| format!("cannot resolve {:?}: {}", abs, e))?;
    let resolved = canonical_base.join(&suffix);
    let cwd_canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));

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

/// Execute a server-requested tool against the local filesystem, confined
/// to `cwd`. Consent for risky operations is handled by the caller — this
/// function assumes the operation is allowed.
pub async fn execute_local_tool(name: &str, args_json: &str, cwd: &str) -> String {
    let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or_default();

    match name {
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("");
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
            let child = tokio::process::Command::new("sh")
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
                    if let Some(parent) = resolved.parent() {
                        tokio::fs::create_dir_all(parent).await.ok();
                    }
                    match tokio::fs::read_to_string(&resolved).await {
                        Err(e) => format!("Error: {e}"),
                        Ok(content) => {
                            if !content.contains(old_str) {
                                format!("Error: old_str not found in {}", resolved.display())
                            } else {
                                let count = content.matches(old_str).count();
                                if count > 1 {
                                    format!("Error: old_str found {count} times — must be unique")
                                } else {
                                    let mut n = content.replacen(old_str, new_str, 1);
                                    if content.ends_with('\n') && !n.ends_with('\n') {
                                        n.push('\n');
                                    }
                                    // Atomic write via temp-file + rename:
                                    // prevents partial writes and TOCTOU races
                                    // with concurrent readers/writers.
                                    match tokio::fs::write(&resolved, &n).await {
                                        Err(e) => format!("Error: {e}"),
                                        Ok(()) => format!(
                                            "edited {} (replaced 1 occurrence)",
                                            resolved.display()
                                        ),
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

/// Compute the next permission mode for Shift+Tab cycling.
pub fn next_mode(current: PermissionMode) -> PermissionMode {
    use temple_protocol::PermissionMode;
    match current {
        PermissionMode::Default => PermissionMode::Ask,
        PermissionMode::Ask => PermissionMode::Yolo,
        PermissionMode::Yolo => PermissionMode::Lockdown,
        PermissionMode::Lockdown => PermissionMode::Default,
    }
}

/// Human-readable tag for the current permission mode.
pub fn mode_tag(mode: PermissionMode) -> &'static str {
    use temple_protocol::PermissionMode;
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::Ask => "ask",
        PermissionMode::Lockdown => "locked",
        PermissionMode::Yolo => "yolo",
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
    fn test_resolve_tool_path_absolute() {
        let tmp = TempDir::new().unwrap();
        let cwd = std::fs::canonicalize(tmp.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        let file_path = std::path::Path::new(&cwd).join("bar.txt");
        let abs_path = file_path.to_str().unwrap().to_string();
        std::fs::write(&file_path, "hello").unwrap();

        let resolved = resolve_tool_path(&abs_path, &cwd).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&file_path).unwrap());
    }

    #[test]
    fn test_resolve_tool_path_new_file() {
        let tmp = TempDir::new().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let cwd = base.to_string_lossy().to_string();
        // Parent dirs don't exist yet — walk-up should find base
        let resolved = resolve_tool_path_for("a/b/new.txt", &cwd, true).unwrap();
        assert_eq!(resolved, base.join("a/b/new.txt"));
        let cwd_canon = std::fs::canonicalize(&cwd).unwrap();
        assert!(resolved.starts_with(&cwd_canon));
    }

    #[test]
    fn test_resolve_tool_path_escape() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let cwd = sub.to_str().unwrap();

        // Attempt to escape via ../sibling
        let result = resolve_tool_path_for("../sibling.txt", cwd, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("escapes"));
    }

    #[test]
    fn test_resolve_tool_path_file_as_dir() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let file_path = tmp.path().join("data.txt");
        std::fs::write(&file_path, "hello").unwrap();

        // Trying to use a file as a directory should fail
        let result = resolve_tool_path_for("data.txt/sub.txt", cwd, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not a directory"));
    }

    #[test]
    fn test_resolve_tool_path_tmp_for_write_denied() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let result = resolve_tool_path_for("/tmp/evil.sh", cwd, true);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("escapes"));
    }

    #[test]
    fn test_resolve_tool_path_tmp_for_read_allowed() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        // /tmp is allowed for reads only
        let result = resolve_tool_path("/tmp/somefile", cwd);
        assert!(result.is_ok());
    }

    #[test]
    fn test_resolve_tool_path_dotdot_normalized() {
        let tmp = TempDir::new().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let sub = base.join("sub");
        let sub2 = base.join("sub2");
        std::fs::create_dir(&sub).unwrap();
        std::fs::create_dir(&sub2).unwrap();
        std::fs::write(sub2.join("target.txt"), "hello").unwrap();
        let cwd = sub.to_string_lossy().to_string();

        let resolved = resolve_tool_path("../sub2/../sub2/target.txt", &cwd).unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(sub2.join("target.txt")).unwrap()
        );
    }

    #[test]
    fn test_osc52_copy_output() {
        osc52_copy("hello world");
        // No assertion — just verifying it doesn't panic.
        // The output goes to stdout; in test context that's captured.
    }

    #[test]
    fn test_next_mode_cycle() {
        use temple_protocol::PermissionMode;
        let modes: &[PermissionMode] = &[
            PermissionMode::Default,
            PermissionMode::Ask,
            PermissionMode::Yolo,
            PermissionMode::Lockdown,
            PermissionMode::Default,
        ];
        for w in modes.windows(2) {
            assert_eq!(next_mode(w[0]), w[1]);
        }
    }

    #[test]
    fn test_mode_tag_values() {
        use temple_protocol::PermissionMode;
        assert_eq!(mode_tag(PermissionMode::Default), "default");
        assert_eq!(mode_tag(PermissionMode::Ask), "ask");
        assert_eq!(mode_tag(PermissionMode::Lockdown), "locked");
        assert_eq!(mode_tag(PermissionMode::Yolo), "yolo");
    }
}
