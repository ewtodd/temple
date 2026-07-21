use std::path::{Path, PathBuf};
use tokio::process::Command;
use crate::config::SshTarget;

/// SSH client for remote tool execution on workstations.
/// Connects via bastion (ProxyJump) to reach LAN hosts behind the firewall.
pub struct SshExecutor {
    target: SshTarget,
    bastion: Option<String>,
    key_path: PathBuf,
}

impl SshExecutor {
    pub fn new(target: SshTarget, bastion: Option<String>, key_path: PathBuf) -> Self {
        Self {
            target,
            bastion,
            key_path,
        }
    }

    /// Build the SSH command prefix (ssh -i key -p port [-J bastion] account@host)
    fn ssh_args(&self, remote_command: &str) -> Vec<String> {
        let mut args = vec![
            "-F".into(),
            "/var/lib/temple/.ssh/config".into(),
            "-i".into(),
            self.key_path.display().to_string(),
            "-p".into(),
            self.target.port.to_string(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "BatchMode=yes".into(),
        ];

        if let Some(ref bastion) = self.bastion {
            args.push("-J".into());
            args.push(bastion.clone());
        }

        args.push(format!("{}@{}", self.target.account, self.target.host));
        args.push(remote_command.into());
        args
    }

    /// Execute a command on the remote host.
    pub async fn execute(&self, command: &str) -> Result<String, String> {
        let args = self.ssh_args(command);
        let output = Command::new("ssh")
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("ssh spawn: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut out = String::new();
        if !stdout.is_empty() { out.push_str(&stdout); }
        if !stderr.is_empty() { out.push_str(&format!("\n[stderr]\n{stderr}")); }
        if !output.status.success() {
            out.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
        }
        if out.is_empty() { out.push_str("(no output)"); }
        Ok(out)
    }

    /// Read a file on the remote host.
    pub async fn read_file(&self, path: &str) -> Result<String, String> {
        // Use cat with proper quoting
        let cmd = format!("cat -- {}", shell_quote(path));
        self.execute(&cmd).await
    }

    /// Write content to a file on the remote host.
    pub async fn write_file(&self, path: &str, content: &str) -> Result<String, String> {
        // Create parent dirs, then write via stdin
        let parent = Path::new(path).parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mkdir_cmd = if !parent.is_empty() {
            format!("mkdir -p -- {} && ", shell_quote(&parent))
        } else {
            String::new()
        };

        // Use a heredoc to avoid quoting issues
        let cmd = format!(
            "{mkdir_cmd}cat > {} << 'TEMPLE_EOF'\n{}\nTEMPLE_EOF\nwc -c < {}",
            shell_quote(path),
            content,
            shell_quote(path),
        );
        let result = self.execute(&cmd).await?;
        Ok(format!("wrote {path} ({} bytes)", result.trim()))
    }

    /// List directory contents on the remote host.
    pub async fn list_dir(&self, path: &str) -> Result<String, String> {
        let cmd = format!(
            "ls -la --color=never --group-directories-first -- {}",
            shell_quote(path)
        );
        self.execute(&cmd).await
    }

    /// Check if the host is reachable.
    pub async fn ping(&self) -> Result<(), String> {
        self.execute("echo ok").await?;
        Ok(())
    }

    /// Get the home directory of the remote user.
    pub async fn home_dir(&self) -> Result<String, String> {
        let result = self.execute("echo $HOME").await?;
        Ok(result.trim().to_string())
    }

    /// Resolve a path relative to the remote home directory.
    pub async fn resolve_path(&self, path: &str) -> Result<String, String> {
        if path.starts_with('/') {
            return Ok(path.to_string());
        }
        if path.starts_with("~/") {
            let home = self.home_dir().await?;
            return Ok(format!("{}{}", home, &path[1..]));
        }
        if path == "~" {
            return self.home_dir().await;
        }
        // Relative paths resolve from $HOME (not CWD, since each SSH call is fresh)
        let home = self.home_dir().await?;
        Ok(format!("{home}/{path}"))
    }

    /// Check if a path is within the allowed directories.
    pub fn is_path_allowed(&self, abs_path: &str, home: &str) -> bool {
        // Always allow $HOME and /tmp
        if abs_path.starts_with(home) || abs_path.starts_with("/tmp/") || abs_path == "/tmp" {
            return true;
        }
        // Check configured allowed_dirs
        for dir in &self.target.allowed_dirs {
            if abs_path.starts_with(dir) {
                return true;
            }
        }
        false
    }

    /// Get the target name for display.
    pub fn target_name(&self) -> &str {
        &self.target.name
    }
}

/// Shell-quote a path to prevent injection.
fn shell_quote(s: &str) -> String {
    // Single-quote and escape any embedded single quotes
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}
