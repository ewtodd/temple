use std::path::{Path, PathBuf};
use temple_protocol::{AccessKind, PermissionMode};
use tokio::fs;

/// Represents a session's permission scope.
#[derive(Debug, Clone)]
pub struct PermissionScope {
    cwd: PathBuf,
    mode: PermissionMode,
    allowed: Vec<PathBuf>,
}

impl PermissionScope {
    pub async fn new(cwd: &Path, mode: PermissionMode, allowed: &[String]) -> Self {
        let canonical = fs::canonicalize(cwd)
            .await
            .unwrap_or_else(|_| cwd.to_path_buf());
        let mut allowed_paths = Vec::with_capacity(allowed.len());
        for s in allowed {
            if let Ok(p) = fs::canonicalize(Path::new(s)).await {
                allowed_paths.push(p);
            }
        }
        Self {
            cwd: canonical,
            mode,
            allowed: allowed_paths,
        }
    }

    #[allow(dead_code)] // Public API — used by session inspection paths.
    pub fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }

    #[allow(dead_code)]
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// Resolve a path to its canonical form, handling non-existent paths
    /// by canonicalizing the nearest existing ancestor and re-joining the
    /// tail. Without this, `cwd/link/evil.txt` (where link → /etc) passes
    /// the lexical prefix check and writes escape the sandbox.
    async fn canonical_or_ancestor(&self, path: &Path) -> PathBuf {
        if let Ok(p) = fs::canonicalize(path).await {
            return p;
        }
        // Walk up until an existing component is found, canonicalize it,
        // then re-join the non-existent tail.
        let mut candidate = path.to_path_buf();
        let mut suffix = PathBuf::new();
        loop {
            if candidate.exists() {
                break;
            }
            match candidate.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    if let Some(name) = candidate.file_name() {
                        suffix = PathBuf::from(name).join(&suffix);
                    }
                    candidate = parent.to_path_buf();
                }
                _ => break,
            }
        }
        match fs::canonicalize(&candidate).await {
            Ok(base) => base.join(&suffix),
            Err(_) => normalize_path(path),
        }
    }

    async fn is_in_cwd(&self, path: &Path) -> bool {
        self.canonical_or_ancestor(path)
            .await
            .starts_with(&self.cwd)
    }

    async fn is_in_allowed(&self, path: &Path) -> bool {
        let canonical = self.canonical_or_ancestor(path).await;
        self.allowed.iter().any(|a| canonical.starts_with(a))
    }

    /// Ok(()) → granted. Err(path) → needs user prompt.
    pub async fn check_access(&self, path: &Path, access: AccessKind) -> Result<(), PathBuf> {
        match self.mode {
            PermissionMode::Yolo => Ok(()),

            PermissionMode::Lockdown => {
                if (access == AccessKind::Read || access == AccessKind::ReadDir)
                    && (self.is_in_cwd(path).await || self.is_in_allowed(path).await)
                {
                    return Ok(());
                }
                if access == AccessKind::Write && self.is_in_allowed(path).await {
                    return Ok(());
                }
                if access == AccessKind::Execute {
                    // never allowed in lockdown without prompt
                    return Err(path.to_path_buf());
                }
                Err(path.to_path_buf())
            }

            PermissionMode::Ask => {
                if self.is_in_cwd(path).await || self.is_in_allowed(path).await {
                    Ok(())
                } else {
                    Err(path.to_path_buf())
                }
            }

            PermissionMode::Default => {
                if self.is_in_cwd(path).await || self.is_in_allowed(path).await {
                    Ok(())
                } else {
                    Err(path.to_path_buf())
                }
            }
        }
    }

    /// Canonicalise a path relative to CWD.
    #[allow(dead_code)]
    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }
}

/// Lexically resolve `.`/`..` without touching the filesystem (for paths
/// that don't exist yet, where canonicalize fails).
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "temple-perm-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn cwd_paths_allowed_in_default_mode() {
        let dir = tmpdir();
        let scope = PermissionScope::new(&dir, PermissionMode::Default, &[]).await;
        assert!(scope
            .check_access(&dir.join("file.txt"), AccessKind::Read)
            .await
            .is_ok());
        assert!(scope
            .check_access(&dir.join("newfile.txt"), AccessKind::Write)
            .await
            .is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn outside_paths_prompt_in_default_mode() {
        let dir = tmpdir();
        let scope = PermissionScope::new(&dir, PermissionMode::Default, &[]).await;
        assert!(scope
            .check_access(Path::new("/etc/passwd"), AccessKind::Read)
            .await
            .is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn dotdot_escape_prompts() {
        let dir = tmpdir();
        let scope = PermissionScope::new(&dir, PermissionMode::Default, &[]).await;
        assert!(scope
            .check_access(&dir.join("..").join("etc").join("passwd"), AccessKind::Read)
            .await
            .is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The symlink regression: `cwd/link/evil.txt` where link → /tmp/elsewhere
    /// must NOT pass the cwd check for new files. Lexical normalization
    /// alone lets it through; ancestor canonicalization closes it.
    #[tokio::test]
    async fn symlinked_parent_escape_prompts() {
        let dir = tmpdir();
        let elsewhere = tmpdir();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, dir.join("link")).unwrap();
        let scope = PermissionScope::new(&dir, PermissionMode::Default, &[]).await;
        // The file doesn't exist — the old lexical check approved this.
        assert!(scope
            .check_access(&dir.join("link").join("evil.txt"), AccessKind::Write)
            .await
            .is_err());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&elsewhere).ok();
    }

    #[tokio::test]
    async fn lockdown_blocks_execute() {
        let dir = tmpdir();
        let scope = PermissionScope::new(&dir, PermissionMode::Lockdown, &[]).await;
        assert!(scope
            .check_access(&dir.join("x"), AccessKind::Execute)
            .await
            .is_err());
        // Reads inside cwd still fine.
        assert!(scope
            .check_access(&dir.join("x"), AccessKind::Read)
            .await
            .is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn yolo_allows_everything() {
        let dir = tmpdir();
        let scope = PermissionScope::new(&dir, PermissionMode::Yolo, &[]).await;
        assert!(scope
            .check_access(Path::new("/etc/passwd"), AccessKind::Read)
            .await
            .is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
