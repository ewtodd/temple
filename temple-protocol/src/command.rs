//! Shared command-safety classification, used by both the server (which
//! decides when to prompt for AccessKind::Execute) and the client (which
//! decides whether a server-requested command may run without local
//! consent). One definition — the two sides can never drift.

/// How a shell command should be treated before execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRisk {
    /// Read-only, safe to run without prompting.
    Safe,
    /// Potentially destructive or state-changing — prompt the user.
    Prompt,
    /// Clearly destructive — always prompt, even in Yolo mode.
    Dangerous,
}

/// Read-only commands that are safe to run without prompting.
/// Interpreters (python etc.) are NOT here — they run arbitrary code.
const SAFE_COMMANDS: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "grep",
    "rg",
    "fd",
    "tree",
    "pwd",
    "whoami",
    "hostname",
    "uname",
    "date",
    "uptime",
    "wc",
    "sort",
    "uniq",
    "diff",
    "cmp",
    "file",
    "stat",
    "du",
    "df",
    "rustc",
    "nixos-version",
    "echo",
    "printf",
    "which",
    "type",
    "env",
    "printenv",
    "test",
    "true",
    "false",
    "less",
    "more",
    "man",
    "info",
    "ps",
    "top",
    "htop",
    "free",
    "lspci",
    "lsusb",
    "lsblk",
];

/// Binaries that are safe ONLY with read-only arguments — a destructive
/// flag/subcommand drops the segment to Prompt. Checked per-segment.
const GUARDED_COMMANDS: &[&str] = &["find", "git", "cargo", "nix"];

/// Subcommand/flag patterns that make a guarded command destructive.
const GUARDED_DESTRUCTIVE: &[&str] = &[
    // find
    "-delete",
    "-exec rm",
    "-execdir rm",
    "-ok rm",
    "-okdir rm",
    // git
    "clean",
    "reset --hard",
    "push",
    "checkout --",
    "checkout .",
    "restore",
    "rebase",
    "filter-branch",
    "update-ref -d",
    "branch -d",
    "branch -D",
    "tag -d",
    "stash drop",
    "stash clear",
    "rm ",
    "mv ",
    "commit",
    "cherry-pick",
    "revert",
    "merge",
    "pull",
    "fetch",
    "gc",
    "prune",
    "reflog expire",
    // cargo
    "clean",
    // nix
    "store gc",
    "store delete",
    "store rm",
    "collect-garbage",
    "profile install",
    "profile remove",
    "profile upgrade",
    "flake update",
    "flake lock --update",
];

/// Patterns that indicate a dangerous command — always prompt.
/// Compared against whitespace-normalized lowercase input, so
/// `rm  -rf` (double space) can't evade.
const DANGEROUS_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rm -r -f",
    "rm -f -r",
    "rm -r /",
    "rm -rf /",
    "rm -rf ~",
    "rm -rf *",
    "mkfs",
    "dd if=",
    "dd of=/dev/",
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
    ":(){",
    "fork bomb",
    "> /dev/sd",
    "> /dev/nvme",
    "> /dev/mmc",
    "chmod -r 777",
    "chmod 777",
    "systemctl stop",
    "systemctl disable",
    "kill -9",
    "killall",
    "pkill",
    "kill -kill",
    "iptables -f",
    "ip6tables -f",
    "mount",
    "umount",
    "fdisk",
    "parted",
    "wipefs",
    "shred",
    "scrub",
    "git push --force",
    "git push -f",
    "nix-collect-garbage -d",
    "visudo",
    "passwd",
    "useradd",
    "userdel",
    "usermod",
    "groupadd",
    "groupdel",
    "sudo rm",
    "sudo dd",
    "sudo mkfs",
];

/// Collapse runs of whitespace to single spaces and lowercase — pattern
/// matching against this catches spacing evasions like `rm  -rf`.
fn normalize(command: &str) -> String {
    command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// First word of each pipeline/chain segment. Checking every segment
/// blocks `ls && rm -r ~` and `cat x | sh`.
fn segment_heads(command: &str) -> Vec<&str> {
    command
        .split(['|', ';', '&'])
        .filter_map(|seg| seg.split_whitespace().next())
        .filter(|first| !first.is_empty())
        .collect()
}

/// Classify a shell command for consent purposes.
pub fn classify_command(command: &str) -> CommandRisk {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return CommandRisk::Prompt;
    }

    let normalized = normalize(trimmed);

    // Clearly destructive — always prompt, even in Yolo.
    if DANGEROUS_PATTERNS
        .iter()
        .any(|pat| normalized.contains(pat))
    {
        return CommandRisk::Dangerous;
    }

    // Redirection, substitution, sudo, multi-line, or background-chaining
    // into anything unknown always prompts. (Redirection detection uses the
    // raw text — `echo a > b` contains no safe interpretation.)
    if trimmed.contains(['>', '<', '`', '\n'])
        || trimmed.contains("$(")
        || trimmed.contains("${")
        || normalized.contains("sudo")
        || normalized.contains("chmod")
        || normalized.contains("chown")
    {
        return CommandRisk::Prompt;
    }

    let heads = segment_heads(trimmed);
    if heads.is_empty() {
        return CommandRisk::Prompt;
    }

    // Every segment must start with a known binary.
    for head in &heads {
        if SAFE_COMMANDS.contains(head) {
            continue;
        }
        if GUARDED_COMMANDS.contains(head) {
            // Safe binary, but only with read-only args. Check the whole
            // segment (not just the head) for destructive flags. We check
            // against the normalized full command — conservative: a flag
            // anywhere taints the pipeline.
            let destructive = GUARDED_DESTRUCTIVE.iter().any(|pat| {
                // "rm " and "mv " have trailing spaces to avoid matching
                // "rmail"/"mvdir" style words; check with the pattern as-is
                // plus word-boundary at the end via contains on segments.
                normalized
                    .split(['|', ';', '&'])
                    .map(str::trim)
                    .filter(|seg| seg.starts_with(head))
                    .any(|seg| seg.contains(pat))
            });
            if destructive {
                return CommandRisk::Prompt;
            }
            continue;
        }
        // Unknown binary — prompt.
        return CommandRisk::Prompt;
    }

    CommandRisk::Safe
}

/// Returns true if the command is safe to run without a permission prompt.
/// Kept for the server's existing call sites.
pub fn is_safe_command(command: &str) -> bool {
    classify_command(command) == CommandRisk::Safe
}

/// Returns true if the command must prompt even in Yolo mode.
pub fn is_dangerous_command(command: &str) -> bool {
    classify_command(command) == CommandRisk::Dangerous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_read_only() {
        assert_eq!(classify_command("ls -la"), CommandRisk::Safe);
        assert_eq!(classify_command("cat flake.nix"), CommandRisk::Safe);
        assert_eq!(classify_command("rg foo | head"), CommandRisk::Safe);
        assert_eq!(classify_command("git status"), CommandRisk::Safe);
        assert_eq!(classify_command("git diff --stat"), CommandRisk::Safe);
        assert_eq!(classify_command("cargo check"), CommandRisk::Safe);
        assert_eq!(classify_command("cargo build --release"), CommandRisk::Safe);
        assert_eq!(classify_command("cargo test"), CommandRisk::Safe);
        assert_eq!(classify_command("nix build"), CommandRisk::Safe);
        assert_eq!(classify_command("nix eval .#x"), CommandRisk::Safe);
        assert_eq!(classify_command("find . -name '*.rs'"), CommandRisk::Safe);
    }

    #[test]
    fn destructive_flags_on_safe_binaries_prompt() {
        assert_eq!(classify_command("find . -delete"), CommandRisk::Prompt);
        assert_eq!(classify_command("git clean -fdx"), CommandRisk::Prompt);
        assert_eq!(classify_command("git reset --hard"), CommandRisk::Prompt);
        assert_eq!(
            classify_command("git push origin main"),
            CommandRisk::Prompt
        );
        assert_eq!(classify_command("git commit -m x"), CommandRisk::Prompt);
        assert_eq!(classify_command("cargo clean"), CommandRisk::Prompt);
        assert_eq!(classify_command("nix store gc"), CommandRisk::Prompt);
        assert_eq!(classify_command("nix-collect-garbage"), CommandRisk::Prompt);
        assert_eq!(classify_command("nix flake update"), CommandRisk::Prompt);
    }

    #[test]
    fn dangerous_always_prompts() {
        assert_eq!(classify_command("rm -rf /"), CommandRisk::Dangerous);
        assert_eq!(classify_command("rm  -rf   ~"), CommandRisk::Dangerous);
        assert_eq!(classify_command("rm -r -f /tmp/x"), CommandRisk::Dangerous);
        assert_eq!(classify_command("shutdown now"), CommandRisk::Dangerous);
        assert_eq!(
            classify_command("dd if=/dev/zero of=/dev/sda"),
            CommandRisk::Dangerous
        );
    }

    #[test]
    fn evasions_caught() {
        assert_eq!(classify_command("ls && rm -r ~"), CommandRisk::Prompt);
        assert_eq!(classify_command("cat x | sh"), CommandRisk::Prompt);
        assert_eq!(classify_command("echo hi > file"), CommandRisk::Prompt);
        assert_eq!(classify_command("cat $(rm x)"), CommandRisk::Prompt);
        assert_eq!(classify_command("sudo ls"), CommandRisk::Prompt);
        assert_eq!(classify_command("chmod 644 x"), CommandRisk::Prompt);
        assert_eq!(
            classify_command("python -c 'print(1)'"),
            CommandRisk::Prompt
        );
        assert_eq!(classify_command(""), CommandRisk::Prompt);
    }
}
