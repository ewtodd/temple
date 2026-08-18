//! Mandatory filesystem confinement for executed commands, via the
//! Landlock LSM. The daemon runs commands in a child process that applies
//! a read-only-everywhere domain before exec — only the session working
//! directory, the configured allowed dirs, the agent's HOME, and the
//! standard temp/device paths stay writable. A command that cannot be
//! confined fails instead of running unsandboxed.
//!
//! In-process fs tools (read/write/edit/list) are not landlocked — their
//! sandbox is the cwd confinement in `local_tools`; this module covers
//! `execute_command`, which is the arbitrary-code surface.

use landlock::{
    Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, ABI,
};

/// Paths every confined command may write, besides the caller-provided
/// writable dirs: temp files and the (already private) device tree.
const EXTRA_WRITABLE: &[&str] = &["/tmp", "/dev"];

/// Apply the Landlock domain to the current process. Must run in the
/// child, after fork and before exec.
pub fn apply_landlock(writable: &[String]) -> Result<(), String> {
    let abi = ABI::V9;
    let ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| format!("landlock: handle_access: {e}"))?;
    let ro_root = PathBeneath::new(
        PathFd::new("/").map_err(|e| format!("landlock: open /: {e}"))?,
        AccessFs::from_read(abi),
    );
    let mut created = ruleset
        .create()
        .map_err(|e| format!("landlock: create ruleset: {e}"))?
        .add_rule(ro_root)
        .map_err(|e| format!("landlock: read-only / rule: {e}"))?;

    let mut all_writable: Vec<String> = writable.to_vec();
    for extra in EXTRA_WRITABLE {
        if !all_writable.iter().any(|d| d == extra) {
            all_writable.push((*extra).to_string());
        }
    }
    for dir in &all_writable {
        let rule = PathBeneath::new(
            PathFd::new(dir).map_err(|e| format!("landlock: open {dir}: {e}"))?,
            AccessFs::from_all(abi),
        );
        created = created
            .add_rule(rule)
            .map_err(|e| format!("landlock: writable rule {dir}: {e}"))?;
    }

    created
        .set_compatibility(CompatLevel::HardRequirement)
        .restrict_self()
        .map_err(|e| format!("landlock: restrict_self: {e}"))?;
    Ok(())
}

/// Whether the running kernel supports Landlock (probe only — nothing is
/// restricted). Tests skip when unavailable.
pub fn landlock_available() -> bool {
    landlock::Ruleset::default()
        .handle_access(AccessFs::Execute)
        .and_then(|r| r.create())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// Run one command confined by the sandbox; returns its output.
    async fn confined(command: &str, writable: &[String]) -> std::process::Output {
        let writable = writable.to_vec();
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            cmd.pre_exec(move || apply_landlock(&writable).map_err(std::io::Error::other));
        }
        cmd.output().await.expect("spawn sandboxed command")
    }

    #[tokio::test]
    async fn sandbox_blocks_writes_outside_writable_dirs() {
        if !landlock_available() {
            eprintln!("skipping: Landlock unavailable on this kernel");
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let writable = vec![dir.path().to_string_lossy().to_string()];
        let denied = confined("touch /etc/temple-landlock-probe", &writable).await;
        assert!(!denied.status.success(), "write to /etc must be denied");
        assert!(!std::path::Path::new("/etc/temple-landlock-probe").exists());
        let allowed = confined(&format!("touch {}/ok.txt", dir.path().display()), &writable).await;
        assert!(allowed.status.success(), "write inside cwd must succeed");
        assert!(dir.path().join("ok.txt").exists());
    }

    #[tokio::test]
    async fn sandbox_fails_loudly_when_landlock_is_unavailable() {
        // On a kernel without Landlock, confined commands must fail, not
        // run unrestricted.
        if landlock_available() {
            eprintln!("skipping: Landlock is available on this kernel");
            return;
        }
        let out = confined("echo hi", &["/tmp".to_string()]).await;
        assert!(!out.status.success());
    }
}
