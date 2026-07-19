use std::path::{Path, PathBuf};
use temple_protocol::{AccessKind, PermissionMode};

/// Represents a session's permission scope.
#[derive(Debug, Clone)]
pub struct PermissionScope {
    cwd: PathBuf,
    mode: PermissionMode,
    allowed: Vec<PathBuf>,
}

impl PermissionScope {
    pub fn new(cwd: &Path, mode: PermissionMode, allowed: &[String]) -> Self {
        let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let allowed_paths = allowed
            .iter()
            .filter_map(|s| std::fs::canonicalize(Path::new(s)).ok())
            .collect();
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

    fn is_in_cwd(&self, path: &Path) -> bool {
        let canonical = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => return path.starts_with(&self.cwd),
        };
        canonical.starts_with(&self.cwd)
    }

    fn is_in_allowed(&self, path: &Path) -> bool {
        let canonical = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => return false,
        };
        self.allowed.iter().any(|a| canonical.starts_with(a))
    }

    /// Ok(()) → granted. Err(path) → needs user prompt.
    pub fn check_access(&self, path: &Path, access: AccessKind) -> Result<(), PathBuf> {
        match self.mode {
            PermissionMode::Yolo => Ok(()),

            PermissionMode::Lockdown => {
                if (access == AccessKind::Read || access == AccessKind::ReadDir)
                    && (self.is_in_cwd(path) || self.is_in_allowed(path))
                {
                    return Ok(());
                }
                if access == AccessKind::Write && self.is_in_allowed(path) {
                    return Ok(());
                }
                if access == AccessKind::Execute {
                    // never allowed in lockdown without prompt
                    return Err(path.to_path_buf());
                }
                Err(path.to_path_buf())
            }

            PermissionMode::Ask => {
                if self.is_in_cwd(path) || self.is_in_allowed(path) {
                    Ok(())
                } else {
                    Err(path.to_path_buf())
                }
            }

            PermissionMode::Default => {
                if self.is_in_cwd(path) || self.is_in_allowed(path) {
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
