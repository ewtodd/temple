use crate::PermissionMode;

/// Shared help text — identical across Signal and TUI.
pub const HELP_SHARED: &str = "\
Commands:
  /clear <account>   delete sessions for a user (admin)
  /delete <id>       permanently delete session
  /effort [e]        show/set reasoning effort
  /help              this help
  /mode <m>          set permission mode
  /model [name]      show/set model (\"auto\" to re-route)
  /models            list available models
  /new [target]      start new session
  /q                 exit (TUI only)
  /session <id>      resume session
  /sessions          list your sessions";

/// Full TUI help — shared text + keybindings.
pub const HELP_TUI: &str = "\
Commands:
  /clear          clear chat
  /delete <n>     permanently delete session
  /effort [e]     show/set reasoning effort
  /help           this help
  /mode <m>       set permission mode
  /model <name>   set model (bare to show current)
  /models         list available models
  /new [target]   start new session
  /q              exit
  /quit           exit
  /session <n>    resume session
  /sessions       list sessions

Keys:
  Ctrl+C      cancel agent
  Ctrl+G      edit in $EDITOR
  Ctrl+J/K    scroll by 10
  Ctrl+L      clear chat
  Ctrl+U      clear prompt
  Esc         clear prompt
  PgUp/Dn     scroll by 10
  Shift+Tab   cycle permission mode
  Tab         cycle commands";

/// Valid reasoning-effort values and error text.
pub const EFFORT_VALUES: &[&str] = &["low", "medium", "high", "max", "off"];
pub const EFFORT_HINT: &str = "low · medium · high · max · off";

/// Parse a raw `/effort` argument. Returns the canonical effort string
/// ("off" for "none"/"off") or an error message on invalid input.
pub fn parse_effort(raw: &str) -> Result<&str, &str> {
    let cleaned = raw.trim();
    if cleaned.is_empty() {
        return Err(""); // caller should show current value
    }
    let lower = cleaned.to_lowercase();
    match lower.as_str() {
        "low" | "medium" | "high" | "max" => Ok(cleaned),
        "none" | "off" => Ok("off"),
        _ => Err(EFFORT_HINT),
    }
}

/// Parse a raw `/mode` argument. Returns the permission mode or an error
/// hint on invalid input.
pub fn parse_mode(raw: &str) -> Result<PermissionMode, &str> {
    match raw.trim().to_lowercase().as_str() {
        "default" => Ok(PermissionMode::Default),
        "ask" => Ok(PermissionMode::Ask),
        "lockdown" => Ok(PermissionMode::Lockdown),
        "yolo" => Ok(PermissionMode::Yolo),
        _ => Err("default · ask · lockdown · yolo"),
    }
}

/// Parsed `/model` action.
pub enum ModelAction<'a> {
    /// No argument — show current model.
    Show,
    /// `auto` — reset to automatic routing.
    Auto,
    /// Explicit model name.
    Set(&'a str),
}

/// Parse a raw `/model` argument. Returns the action to take.
pub fn parse_model(raw: &str) -> ModelAction<'_> {
    let cleaned = raw.trim();
    if cleaned.is_empty() {
        ModelAction::Show
    } else if cleaned.eq_ignore_ascii_case("auto") {
        ModelAction::Auto
    } else {
        ModelAction::Set(cleaned)
    }
}

/// Parsed `/new` arguments. Target and optional start directory.
pub struct NewSessionArgs {
    pub ssh_target: Option<String>,
    pub start_dir: Option<String>,
}

/// Parse raw `/new` arguments. Empty input means no args.
pub fn parse_new(raw: &str) -> NewSessionArgs {
    let parts: Vec<&str> = raw
        .split(' ')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    NewSessionArgs {
        ssh_target: parts.first().map(|s| s.to_string()),
        start_dir: parts.get(1).map(|s| s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_valid() {
        assert_eq!(parse_effort("high").unwrap(), "high");
        assert_eq!(parse_effort("max").unwrap(), "max");
        assert_eq!(parse_effort("off").unwrap(), "off");
        assert_eq!(parse_effort("none").unwrap(), "off");
        assert_eq!(parse_effort("  low ").unwrap(), "low");
    }

    #[test]
    fn effort_invalid() {
        assert!(parse_effort("supermode").is_err());
        assert!(parse_effort("x").is_err());
    }

    #[test]
    fn effort_empty() {
        assert!(parse_effort("").is_err_and(|e| e.is_empty()));
    }

    #[test]
    fn mode_valid() {
        assert_eq!(parse_mode("ask").unwrap(), PermissionMode::Ask);
        assert_eq!(parse_mode("YOLO").unwrap(), PermissionMode::Yolo);
        assert_eq!(parse_mode(" default ").unwrap(), PermissionMode::Default);
    }

    #[test]
    fn mode_invalid() {
        assert!(parse_mode("open").is_err());
    }

    #[test]
    fn model_actions() {
        assert!(matches!(parse_model(""), ModelAction::Show));
        assert!(matches!(parse_model("auto"), ModelAction::Auto));
        assert!(matches!(parse_model("AUTO"), ModelAction::Auto));
        assert!(matches!(
            parse_model("deepseek"),
            ModelAction::Set("deepseek")
        ));
    }

    #[test]
    fn new_session_parsing() {
        let a = parse_new("e-work src");
        assert_eq!(a.ssh_target.as_deref(), Some("e-work"));
        assert_eq!(a.start_dir.as_deref(), Some("src"));

        let a = parse_new("  e-desktop  ");
        assert_eq!(a.ssh_target.as_deref(), Some("e-desktop"));
        assert_eq!(a.start_dir, None);

        let a = parse_new("");
        assert_eq!(a.ssh_target, None);
        assert_eq!(a.start_dir, None);
    }
}
