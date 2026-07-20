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
        let canonical = fs::canonicalize(cwd).await.unwrap_or_else(|_| cwd.to_path_buf());
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

    pub fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    async fn is_in_cwd(&self, path: &Path) -> bool {
        let canonical = match fs::canonicalize(path).await {
            Ok(p) => p,
            Err(_) => return path.starts_with(&self.cwd),
        };
        canonical.starts_with(&self.cwd)
    }

    async fn is_in_allowed(&self, path: &Path) -> bool {
        let canonical = match fs::canonicalize(path).await {
            Ok(p) => p,
            Err(_) => return false,
        };
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
    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }
}
